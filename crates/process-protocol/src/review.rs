use serde::{Deserialize, Serialize};

use crate::{CanonicalDigest, CanonicalU64, CanonicalUuid, deserialize_required_nullable};

/// Maximum findings in one produced review inventory, owned by the domain.
pub const MAX_REVIEW_PRODUCED_FINDINGS: usize = signalbox_domain::ReviewProducedFindings::MAXIMUM;

/// One closed review target subject at the process boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewTargetSubject {
    /// A change request frozen at exact head and base revisions.
    ChangeRequest {
        /// Positive provider-local change-request number.
        number: CanonicalU64,
    },
    /// One immutable commit revision.
    Commit {},
}

/// One immutable review target snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewTargetSnapshot {
    /// Stable target identity.
    pub target_id: CanonicalUuid,
    /// Opaque canonical provider key.
    pub provider: String,
    /// Opaque canonical repository key.
    pub repository: String,
    /// Exact subject kind.
    pub subject: ReviewTargetSubject,
    /// Frozen head revision.
    pub head_revision: String,
    /// Frozen comparison revision when the subject has one.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub base_revision: Option<String>,
    /// Immediate stack parent snapshot when present.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub stack_parent_target_id: Option<CanonicalUuid>,
}

/// One admitted review workflow.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewWorkflow {
    /// Import provider-side review context.
    ImportExternalContext,
    /// Produce findings without mutation.
    ReadOnlyReview,
    /// Judge proposed findings.
    JudgeFindings,
    /// Deduplicate proposed findings.
    DedupeFindings,
    /// Publish findings to the provider.
    PublishReview,
    /// Repair accepted findings.
    FixFindings,
    /// Propagate one reviewed stack edge.
    PropagateStack,
}

/// One review pass purpose.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPassKind {
    /// Import provider-side context.
    ImportExternalContext,
    /// Produce read-only findings.
    ReadOnlyReview,
    /// Judge findings.
    Judge,
    /// Deduplicate findings.
    Dedupe,
    /// Publish findings.
    Publish,
    /// Repair findings.
    Fix,
    /// Propagate one stack edge.
    PropagateStack,
}

/// One projected run lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewRunLifecycle {
    /// Waiting for its pass turn.
    Queued,
    /// Its pass turn is active.
    Running,
    /// Its pass completed successfully.
    Succeeded,
    /// Its pass failed.
    Failed,
    /// Its pass needs external resolution.
    Blocked,
    /// It was cancelled.
    Cancelled,
}

/// One projected pass lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPassLifecycle {
    /// Accepted input exists but its turn is not active.
    Queued,
    /// The pass turn is active.
    Running,
    /// The pass turn completed successfully.
    Succeeded,
    /// The pass turn failed.
    Failed,
    /// The pass needs external resolution.
    Blocked,
    /// The pass was cancelled.
    Cancelled,
}

/// One complete review-run read projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewRunSnapshot {
    /// Owning target.
    pub target_id: CanonicalUuid,
    /// Stable run identity.
    pub run_id: CanonicalUuid,
    /// Frozen workflow.
    pub workflow: ReviewWorkflow,
    /// Frozen policy version.
    pub policy_version: CanonicalU64,
    /// Minimum judgment confidence in basis points.
    pub minimum_judge_confidence: CanonicalU64,
    /// Minimum publication confidence in basis points.
    pub minimum_publication_confidence: CanonicalU64,
    /// Current lifecycle projection.
    pub state: ReviewRunLifecycle,
    /// The run's sole pass when admitted.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub pass_id: Option<CanonicalUuid>,
}

/// One complete review-pass read projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewPassSnapshot {
    /// Stable pass identity.
    pub pass_id: CanonicalUuid,
    /// Owning run.
    pub run_id: CanonicalUuid,
    /// Owning target.
    pub target_id: CanonicalUuid,
    /// Exact pass purpose.
    pub kind: ReviewPassKind,
    /// Bound session.
    pub session_id: CanonicalUuid,
    /// Bound accepted input.
    pub accepted_input_id: CanonicalUuid,
    /// Bound origin turn.
    pub origin_turn_id: CanonicalUuid,
    /// Current lifecycle projection.
    pub state: ReviewPassLifecycle,
    /// Exact active or terminal turn when present.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub turn_id: Option<CanonicalUuid>,
    /// Exact successful output frontier when present.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub output_frontier_id: Option<CanonicalUuid>,
}

/// Finding location side relative to a frozen comparison.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDiffSide {
    /// Frozen base side.
    Left,
    /// Frozen head side.
    Right,
}

/// Finding severity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSeverity {
    /// Informational observation.
    Info,
    /// Low-severity defect.
    Low,
    /// Medium-severity defect.
    Medium,
    /// High-severity defect.
    High,
    /// Critical defect.
    Critical,
}

/// Immutable finding content admitted with one read-only pass result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewFindingInput {
    /// Stable finding identity.
    pub finding_id: CanonicalUuid,
    /// Exact repository-relative file path.
    pub file_path: String,
    /// Optional positive first line.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub line_start: Option<CanonicalU64>,
    /// Optional positive final line.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub line_end: Option<CanonicalU64>,
    /// Optional frozen diff side.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub diff_side: Option<ReviewDiffSide>,
    /// Short exact title.
    pub title: String,
    /// Exact explanatory body.
    pub body: String,
    /// Severity classification.
    pub severity: ReviewSeverity,
    /// Producer confidence that the issue is real, in basis points.
    pub is_real_confidence: CanonicalU64,
    /// Producer confidence that the severity label is correct, in basis points.
    pub severity_label_confidence: CanonicalU64,
    /// Opaque canonical category key.
    pub category: String,
    /// Optional exact recommended repair.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub recommended_fix: Option<String>,
}

/// Current finding lifecycle status.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFindingStatus {
    /// Proposed and not yet judged.
    Open,
    /// Accepted by judgment.
    Accepted,
    /// Rejected by judgment.
    Rejected,
    /// Classified as a duplicate.
    Duplicate,
    /// Replaced by a later finding.
    Superseded,
    /// No longer applies.
    Stale,
    /// Published externally.
    Posted,
    /// Repaired.
    Fixed,
    /// Publication or repair was blocked.
    BlockedWithReason,
}

/// One complete finding read projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewFindingSnapshot {
    /// Owning target.
    pub target_id: CanonicalUuid,
    /// Owning run.
    pub run_id: CanonicalUuid,
    /// Producing read-only pass.
    pub producing_pass_id: CanonicalUuid,
    /// Immutable content.
    pub finding: ReviewFindingInput,
    /// Current derived lifecycle status.
    pub status: ReviewFindingStatus,
    /// Number of committed lifecycle events.
    pub event_count: CanonicalU64,
}

/// One immutable finding-machine event recorded by a result-bearing pass.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewFindingEvent {
    Accepted {
        confidence: CanonicalU64,
    },
    Rejected {
        reason: String,
    },
    Duplicate {
        canonical_finding_id: CanonicalUuid,
    },
    Superseded {
        successor_finding_id: CanonicalUuid,
    },
    Stale {},
    Fixed {},
    BlockedWithReason {
        reason: String,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        external_link_id: Option<CanonicalUuid>,
    },
}

/// Terminal outcome for a pass that does not otherwise carry typed result data.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPassTerminalOutcome {
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
}

/// One concern entry in a new frozen orchestration attempt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewOrchestrationConcernInput {
    pub key: String,
    pub template_name: String,
}

/// Terminal imported-context stage outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewImportTerminalOutcome {
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
}

/// Terminal concern-member outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewConcernTerminalOutcome {
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
}

/// One closed disposition in an immutable judgment plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewJudgmentDisposition {
    Accepted {},
    Rejected { reason: String },
    Duplicate { canonical_finding_id: CanonicalUuid },
    Superseded { successor_finding_id: CanonicalUuid },
    Stale {},
}

/// One finding member in a complete judgment plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewJudgmentPlanMember {
    pub finding_id: CanonicalUuid,
    pub disposition: ReviewJudgmentDisposition,
    /// Independent categorical result supporting the disposition.
    pub judgment: ReviewJudgmentResult,
}

/// Categorical judgment returned independently of producer confidence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewJudgmentResult {
    /// Acceptance category, or `none` for a declined candidate.
    pub bar_category: String,
    /// Required decline class for `none`, otherwise null.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub decline_class: Option<String>,
    /// Independent verdict confidence from one through five.
    pub confidence: CanonicalU64,
    /// Explanation of the decisive evidence.
    pub reason: String,
}

/// Terminal result of applying one judgment-plan member.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewJudgmentEffectTerminalOutcome {
    Applied,
    Failed,
    Blocked,
    Cancelled,
}

/// Terminal result of one repair member.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewRepairTerminalOutcome {
    Fixed,
    Failed,
    Blocked,
    Cancelled,
}

/// One finding-indexed repair result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewRepairOutcome {
    pub finding_id: CanonicalUuid,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub event_pass_id: Option<CanonicalUuid>,
    pub outcome: ReviewRepairTerminalOutcome,
}

/// Terminal result of one publication member.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPublicationTerminalOutcome {
    Published,
    Failed,
    Blocked,
    Cancelled,
}

/// One finding-indexed publication result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewPublicationOutcome {
    pub finding_id: CanonicalUuid,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub external_link_id: Option<CanonicalUuid>,
    pub outcome: ReviewPublicationTerminalOutcome,
}

/// Durable stage of one client-driven review-orchestration attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewOrchestrationState {
    AwaitingImport,
    ImportIncomplete,
    AwaitingConcerns,
    FanoutIncomplete,
    AwaitingJudgment,
    AwaitingJudgmentEffects,
    JudgmentIncomplete,
    AwaitingRepair,
    RepairIncomplete,
    AwaitingPublication,
    PublicationIncomplete,
    Complete,
}

/// Durable progress of one frozen concern member.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewOrchestrationConcernStatus {
    Pending,
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
    Superseded,
}

/// Resolved non-concern templates frozen into one attempt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewOrchestrationStageTemplateDigests {
    pub import: CanonicalDigest,
    pub judgment: CanonicalDigest,
    pub repair: CanonicalDigest,
    pub publication: CanonicalDigest,
}

/// One frozen concern and its durable progress.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewOrchestrationConcernSnapshot {
    pub key: String,
    pub template_digest: CanonicalDigest,
    pub status: ReviewOrchestrationConcernStatus,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub pass_id: Option<CanonicalUuid>,
}

/// Progress counts needed to observe one orchestration attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewOrchestrationCounts {
    pub finding_count: CanonicalU64,
    pub judgment_member_count: CanonicalU64,
    pub judgment_effect_applied_count: CanonicalU64,
    pub repair_fixed_count: CanonicalU64,
    pub publication_published_count: CanonicalU64,
}

/// Complete read projection of one review-orchestration attempt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewOrchestrationSnapshot {
    pub attempt_id: CanonicalUuid,
    pub target_id: CanonicalUuid,
    pub state: ReviewOrchestrationState,
    pub concern_set_version: String,
    pub stage_template_digests: ReviewOrchestrationStageTemplateDigests,
    pub concerns: Vec<ReviewOrchestrationConcernSnapshot>,
    pub counts: ReviewOrchestrationCounts,
}

/// Provider object kind reserved for one review aggregate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewExternalObjectKind {
    /// Provider review object.
    Review,
    /// Provider review thread.
    ReviewThread,
    /// Provider inline review comment.
    ReviewComment,
    /// Provider change-request comment.
    ChangeRequestComment,
}
