//! Resumable concern-fan-out review orchestration.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;

use signalbox_domain::{
    ReviewFinding, ReviewFindingId, ReviewFindingRef, ReviewFindingStatus, ReviewKey,
    ReviewPassEvidence, ReviewPassKind, ReviewPassRef, ReviewPassResult, ReviewPassState,
    ReviewPolicy, ReviewRunEvidence, ReviewRunState, ReviewTargetId, ReviewText,
    ReviewWorkflowKind,
};
use tokio::task::JoinSet;
use uuid::Uuid;

/// Identity of one immutable review-orchestration attempt.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReviewOrchestrationAttemptId(Uuid);

impl ReviewOrchestrationAttemptId {
    /// Constructs an attempt identity from its UUID representation.
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Returns the UUID representation.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

/// Exact digest of resolved review template content and execution choices.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReviewTemplateDigest([u8; 32]);

impl ReviewTemplateDigest {
    /// Constructs an exact digest.
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the exact digest bytes.
    pub const fn bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Exact templates used by the non-concern stages of one attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewStageTemplateDigests {
    import: ReviewTemplateDigest,
    judgment: ReviewTemplateDigest,
    repair: ReviewTemplateDigest,
    publication: ReviewTemplateDigest,
}

impl ReviewStageTemplateDigests {
    /// Binds every non-concern stage to its resolved template.
    pub const fn new(
        import: ReviewTemplateDigest,
        judgment: ReviewTemplateDigest,
        repair: ReviewTemplateDigest,
        publication: ReviewTemplateDigest,
    ) -> Self {
        Self {
            import,
            judgment,
            repair,
            publication,
        }
    }

    /// Returns the import template digest.
    pub const fn import(self) -> ReviewTemplateDigest {
        self.import
    }

    /// Returns the judgment template digest.
    pub const fn judgment(self) -> ReviewTemplateDigest {
        self.judgment
    }

    /// Returns the repair template digest.
    pub const fn repair(self) -> ReviewTemplateDigest {
        self.repair
    }

    /// Returns the publication template digest.
    pub const fn publication(self) -> ReviewTemplateDigest {
        self.publication
    }
}

/// One ordered concern and its exact derived session-template digest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewConcernSpec {
    key: ReviewKey,
    template_digest: ReviewTemplateDigest,
}

impl ReviewConcernSpec {
    /// Binds one configured concern to its resolved template.
    pub const fn new(key: ReviewKey, template_digest: ReviewTemplateDigest) -> Self {
        Self {
            key,
            template_digest,
        }
    }

    /// Borrows the closed configured concern key.
    pub const fn key(&self) -> &ReviewKey {
        &self.key
    }

    /// Returns the exact derived template digest.
    pub const fn template_digest(&self) -> ReviewTemplateDigest {
        self.template_digest
    }
}

/// Immutable input and expected concern inventory for one attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewOrchestrationAttempt {
    id: ReviewOrchestrationAttemptId,
    target: ReviewTargetId,
    policy: ReviewPolicy,
    concern_set_version: ReviewKey,
    stage_templates: ReviewStageTemplateDigests,
    concerns: Vec<ReviewConcernSpec>,
}

impl ReviewOrchestrationAttempt {
    /// Constructs one attempt after checking its complete ordered concern inventory.
    pub fn try_new(
        id: ReviewOrchestrationAttemptId,
        target: ReviewTargetId,
        policy: ReviewPolicy,
        concern_set_version: ReviewKey,
        stage_templates: ReviewStageTemplateDigests,
        concerns: Vec<ReviewConcernSpec>,
    ) -> Result<Self, ReviewOrchestrationAttemptError> {
        if concerns.is_empty() {
            return Err(ReviewOrchestrationAttemptError::EmptyConcernInventory);
        }
        let mut keys = HashSet::new();
        for concern in &concerns {
            if !keys.insert(concern.key.clone()) {
                return Err(ReviewOrchestrationAttemptError::RepeatedConcern {
                    concern: concern.key.clone(),
                });
            }
        }
        Ok(Self {
            id,
            target,
            policy,
            concern_set_version,
            stage_templates,
            concerns,
        })
    }

    /// Returns the attempt identity.
    pub const fn id(&self) -> ReviewOrchestrationAttemptId {
        self.id
    }

    /// Returns the immutable target identity.
    pub const fn target(&self) -> ReviewTargetId {
        self.target
    }

    /// Returns the complete frozen policy.
    pub const fn policy(&self) -> ReviewPolicy {
        self.policy
    }

    /// Borrows the exact ordered concern-set version.
    pub const fn concern_set_version(&self) -> &ReviewKey {
        &self.concern_set_version
    }

    /// Returns all non-concern template digests.
    pub const fn stage_templates(&self) -> ReviewStageTemplateDigests {
        self.stage_templates
    }

    /// Borrows the complete expected concern inventory in configured order.
    pub fn concerns(&self) -> &[ReviewConcernSpec] {
        &self.concerns
    }
}

/// Why an immutable attempt inventory cannot be admitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewOrchestrationAttemptError {
    /// At least one concern is required.
    EmptyConcernInventory,
    /// One concern key appeared more than once.
    RepeatedConcern {
        /// Repeated configured key.
        concern: ReviewKey,
    },
}

/// Stable result of attempting to bind immutable durable orchestration data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewDurableSealOutcome {
    /// The value was durably recorded.
    Recorded,
    /// The exact value was already durably recorded.
    EqualReplay,
    /// The durable identity already names a different value.
    Conflict,
}

/// Terminal status shared by passes that produced no successful typed result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewPassIncompleteStatus {
    /// Execution failed definitively.
    Failed,
    /// Execution requires reconciliation.
    Blocked,
    /// Execution was cancelled.
    Cancelled,
}

/// Durable outcome of the external-context import pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewImportOutcome {
    /// Import succeeded with exact pass and imported-context evidence.
    Succeeded {
        /// Canonical succeeded import-pass evidence.
        pass: Box<ReviewPassEvidence>,
        /// Canonical succeeded import-run evidence.
        run: ReviewRunEvidence,
        /// Exact resolved import template.
        template_digest: ReviewTemplateDigest,
        /// Digest of the frozen imported external context.
        context_digest: [u8; 32],
    },
    /// Import did not produce usable context.
    Incomplete {
        /// Terminal pass, when it was admitted before cancellation.
        pass: Option<ReviewPassRef>,
        /// Exact terminal status.
        status: ReviewPassIncompleteStatus,
    },
}

/// Why imported context is cross-wired from its immutable attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewImportEvidenceFailure {
    /// Pass or run target differs.
    ForeignTarget,
    /// Pass or run policy differs.
    ForeignPolicy,
    /// Resolved import template differs.
    ForeignTemplate,
    /// Pass and run are not a canonical succeeded import pair.
    IncompatiblePass,
}

fn validate_import(
    attempt: &ReviewOrchestrationAttempt,
    outcome: &ReviewImportOutcome,
) -> Result<(), ReviewImportEvidenceFailure> {
    let ReviewImportOutcome::Succeeded {
        pass,
        run,
        template_digest,
        ..
    } = outcome
    else {
        return Ok(());
    };
    let pass_ref = pass.reference();
    if pass_ref.target() != attempt.target {
        return Err(ReviewImportEvidenceFailure::ForeignTarget);
    }
    if pass.policy() != attempt.policy || run.policy() != attempt.policy {
        return Err(ReviewImportEvidenceFailure::ForeignPolicy);
    }
    if *template_digest != attempt.stage_templates.import {
        return Err(ReviewImportEvidenceFailure::ForeignTemplate);
    }
    if pass.kind() != ReviewPassKind::ImportExternalContext
        || !matches!(pass.state(), ReviewPassState::Succeeded { .. })
        || run.reference() != pass_ref.run()
        || run.workflow() != ReviewWorkflowKind::ImportExternalContext
        || run.state()
            != (ReviewRunState::Succeeded {
                concluding_pass: pass_ref,
            })
    {
        return Err(ReviewImportEvidenceFailure::IncompatiblePass);
    }
    Ok(())
}

/// A successfully sealed typed inventory from one concern pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewConcernSuccess {
    producer: ReviewPassEvidence,
    run: ReviewRunEvidence,
    findings: Vec<ReviewFinding>,
}

impl ReviewConcernSuccess {
    /// Records authenticated producer ancestry, frozen policy, and complete findings.
    pub fn new(
        producer: ReviewPassEvidence,
        run: ReviewRunEvidence,
        findings: Vec<ReviewFinding>,
    ) -> Self {
        Self {
            producer,
            run,
            findings,
        }
    }

    /// Returns the exact producing pass.
    pub const fn producer(&self) -> ReviewPassRef {
        self.producer.reference()
    }

    /// Borrows the canonical producing-pass evidence.
    pub const fn producer_evidence(&self) -> &ReviewPassEvidence {
        &self.producer
    }

    /// Returns the canonical producing-run evidence.
    pub const fn run_evidence(&self) -> ReviewRunEvidence {
        self.run
    }

    /// Borrows all typed findings in canonical identity order.
    pub fn findings(&self) -> &[ReviewFinding] {
        &self.findings
    }
}

/// Durable terminal outcome of one expected concern member.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewConcernOutcome {
    /// The pass succeeded and bound its complete typed inventory, including empty.
    Succeeded(Box<ReviewConcernSuccess>),
    /// The pass failed and may be retried under the same attempt.
    Failed {
        /// Exact failed pass.
        pass: ReviewPassRef,
    },
    /// The pass blocked and requires reconciliation.
    Blocked {
        /// Exact blocked pass.
        pass: ReviewPassRef,
    },
    /// The pass was cancelled.
    Cancelled {
        /// Exact cancelled pass, when one was admitted.
        pass: Option<ReviewPassRef>,
    },
    /// A historical member claim was superseded and is not barrier-eligible.
    Superseded {
        /// Exact superseded pass.
        pass: ReviewPassRef,
    },
}

/// One durable concern outcome keyed by its frozen attempt input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewConcernClaim {
    concern: ReviewKey,
    template_digest: ReviewTemplateDigest,
    outcome: ReviewConcernOutcome,
}

impl ReviewConcernClaim {
    /// Binds a concern outcome to its configured key and exact template.
    pub const fn new(
        concern: ReviewKey,
        template_digest: ReviewTemplateDigest,
        outcome: ReviewConcernOutcome,
    ) -> Self {
        Self {
            concern,
            template_digest,
            outcome,
        }
    }

    /// Borrows the configured concern key.
    pub const fn concern(&self) -> &ReviewKey {
        &self.concern
    }

    /// Returns the exact template digest.
    pub const fn template_digest(&self) -> ReviewTemplateDigest {
        self.template_digest
    }

    /// Borrows the durable member outcome.
    pub const fn outcome(&self) -> &ReviewConcernOutcome {
        &self.outcome
    }
}

/// Work supplied to one concurrent concern pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewConcernWork {
    attempt: ReviewOrchestrationAttempt,
    imported_context_digest: [u8; 32],
    concern: ReviewConcernSpec,
}

impl ReviewConcernWork {
    /// Borrows the immutable attempt.
    pub const fn attempt(&self) -> &ReviewOrchestrationAttempt {
        &self.attempt
    }

    /// Returns the imported-context digest.
    pub const fn imported_context_digest(&self) -> [u8; 32] {
        self.imported_context_digest
    }

    /// Borrows the one concern assigned to this pass.
    pub const fn concern(&self) -> &ReviewConcernSpec {
        &self.concern
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CompleteReviewFanout {
    attempt: ReviewOrchestrationAttempt,
    members: Vec<ReviewConcernClaim>,
    findings: Vec<ReviewFinding>,
}

/// Why durable concern claims do not prove one complete fan-out.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewFanoutBarrierFailure {
    /// An expected concern has no current claim.
    MissingConcern {
        /// Missing configured concern.
        concern: ReviewKey,
    },
    /// An unconfigured concern claim is present.
    ExtraConcern {
        /// Extra concern key.
        concern: ReviewKey,
    },
    /// More than one current claim exists for one concern.
    RepeatedConcern {
        /// Repeated concern key.
        concern: ReviewKey,
    },
    /// A member claim carries a different resolved template.
    TemplateMismatch {
        /// Concern with the mismatch.
        concern: ReviewKey,
    },
    /// A member has not succeeded with a sealed inventory.
    MemberIncomplete {
        /// Incomplete concern.
        concern: ReviewKey,
    },
    /// A successful producer belongs to another target.
    ForeignProducerTarget {
        /// Concern with cross-wired ancestry.
        concern: ReviewKey,
    },
    /// A successful producer carries a different frozen policy.
    ForeignProducerPolicy {
        /// Concern with cross-wired policy.
        concern: ReviewKey,
    },
    /// A finding is not open or does not belong to its claimed producer.
    InvalidSealedFinding {
        /// Concern carrying the invalid member.
        concern: ReviewKey,
        /// Invalid finding reference.
        finding: ReviewFindingRef,
    },
    /// A finding identity appears in multiple member inventories.
    RepeatedFinding {
        /// Repeated complete finding reference.
        finding: ReviewFindingRef,
    },
}

fn complete_fanout(
    attempt: &ReviewOrchestrationAttempt,
    claims: Vec<ReviewConcernClaim>,
) -> Result<CompleteReviewFanout, ReviewFanoutBarrierFailure> {
    let expected: HashMap<_, _> = attempt
        .concerns
        .iter()
        .map(|concern| (concern.key.clone(), concern))
        .collect();
    let mut current = HashMap::new();
    for claim in claims {
        let concern = claim.concern.clone();
        if !expected.contains_key(&concern) {
            return Err(ReviewFanoutBarrierFailure::ExtraConcern { concern });
        }
        if current.insert(concern.clone(), claim).is_some() {
            return Err(ReviewFanoutBarrierFailure::RepeatedConcern { concern });
        }
    }

    let mut members = Vec::with_capacity(attempt.concerns.len());
    let mut findings = Vec::new();
    let mut finding_ids = BTreeSet::new();
    for expected_member in &attempt.concerns {
        let Some(claim) = current.remove(&expected_member.key) else {
            return Err(ReviewFanoutBarrierFailure::MissingConcern {
                concern: expected_member.key.clone(),
            });
        };
        if claim.template_digest != expected_member.template_digest {
            return Err(ReviewFanoutBarrierFailure::TemplateMismatch {
                concern: expected_member.key.clone(),
            });
        }
        let ReviewConcernOutcome::Succeeded(success) = &claim.outcome else {
            return Err(ReviewFanoutBarrierFailure::MemberIncomplete {
                concern: expected_member.key.clone(),
            });
        };
        let producer = success.producer.reference();
        if producer.target() != attempt.target {
            return Err(ReviewFanoutBarrierFailure::ForeignProducerTarget {
                concern: expected_member.key.clone(),
            });
        }
        if success.producer.policy() != attempt.policy || success.run.policy() != attempt.policy {
            return Err(ReviewFanoutBarrierFailure::ForeignProducerPolicy {
                concern: expected_member.key.clone(),
            });
        }
        let sealed_inventory = match success.producer.state() {
            ReviewPassState::Succeeded {
                result: Some(ReviewPassResult::ProducedFindings(inventory)),
                ..
            } if success.producer.kind() == ReviewPassKind::ReadOnlyReview
                && success.run.reference() == producer.run()
                && success.run.workflow() == ReviewWorkflowKind::ReadOnlyReview
                && success.run.state()
                    == (ReviewRunState::Succeeded {
                        concluding_pass: producer,
                    }) =>
            {
                inventory.findings()
            }
            _ => {
                return Err(ReviewFanoutBarrierFailure::MemberIncomplete {
                    concern: expected_member.key.clone(),
                });
            }
        };
        let claimed_inventory: Vec<_> = success
            .findings
            .iter()
            .map(|finding| finding.proposal().reference())
            .collect();
        if claimed_inventory != sealed_inventory {
            let finding = claimed_inventory
                .first()
                .copied()
                .or_else(|| sealed_inventory.first().copied())
                .unwrap_or_else(|| {
                    ReviewFindingRef::new(producer, ReviewFindingId::from_uuid(Uuid::nil()))
                });
            return Err(ReviewFanoutBarrierFailure::InvalidSealedFinding {
                concern: expected_member.key.clone(),
                finding,
            });
        }
        let mut previous = None;
        for finding in &success.findings {
            let reference = finding.proposal().reference();
            if reference.pass() != producer
                || finding.proposal().producing_pass() != &success.producer
                || finding.status() != ReviewFindingStatus::Open
                || previous.is_some_and(|prior| prior >= reference)
            {
                return Err(ReviewFanoutBarrierFailure::InvalidSealedFinding {
                    concern: expected_member.key.clone(),
                    finding: reference,
                });
            }
            if !finding_ids.insert(reference) {
                return Err(ReviewFanoutBarrierFailure::RepeatedFinding { finding: reference });
            }
            previous = Some(reference);
            findings.push(finding.clone());
        }
        members.push(claim);
    }
    findings.sort_unstable_by_key(|finding| finding.proposal().reference());
    Ok(CompleteReviewFanout {
        attempt: attempt.clone(),
        members,
        findings,
    })
}

/// One judgment or deduplication disposition in a complete plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewPlannedDisposition {
    /// Accept a sufficiently confident finding.
    Accepted,
    /// Reject a finding with an exact reason.
    Rejected { reason: ReviewText },
    /// Classify a finding as a duplicate.
    Duplicate { canonical: ReviewFindingRef },
    /// Classify a finding as superseded.
    Superseded { successor: ReviewFindingRef },
    /// Classify a finding as stale.
    Stale,
}

/// One exact member of a complete judgment plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewJudgmentPlanMember {
    finding: ReviewFindingRef,
    disposition: ReviewPlannedDisposition,
}

impl ReviewJudgmentPlanMember {
    /// Binds one input finding to exactly one planned disposition.
    pub const fn new(finding: ReviewFindingRef, disposition: ReviewPlannedDisposition) -> Self {
        Self {
            finding,
            disposition,
        }
    }

    /// Returns the input finding.
    pub const fn finding(&self) -> ReviewFindingRef {
        self.finding
    }

    /// Borrows the planned disposition.
    pub const fn disposition(&self) -> &ReviewPlannedDisposition {
        &self.disposition
    }
}

/// Structured output of the complete-set judgment analysis pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewJudgmentPlan {
    analysis_pass: Box<ReviewPassEvidence>,
    analysis_run: ReviewRunEvidence,
    template_digest: ReviewTemplateDigest,
    members: Vec<ReviewJudgmentPlanMember>,
}

impl ReviewJudgmentPlan {
    /// Records the analysis pass, policy, and exact proposed dispositions.
    pub fn new(
        analysis_pass: ReviewPassEvidence,
        analysis_run: ReviewRunEvidence,
        template_digest: ReviewTemplateDigest,
        members: Vec<ReviewJudgmentPlanMember>,
    ) -> Self {
        Self {
            analysis_pass: Box::new(analysis_pass),
            analysis_run,
            template_digest,
            members,
        }
    }

    /// Returns the judgment-analysis pass.
    pub const fn analysis_pass(&self) -> ReviewPassRef {
        self.analysis_pass.reference()
    }

    /// Borrows canonical judgment-pass evidence.
    pub const fn analysis_pass_evidence(&self) -> &ReviewPassEvidence {
        &self.analysis_pass
    }

    /// Returns canonical judgment-run evidence.
    pub const fn analysis_run_evidence(&self) -> ReviewRunEvidence {
        self.analysis_run
    }

    /// Returns the exact resolved judgment template.
    pub const fn template_digest(&self) -> ReviewTemplateDigest {
        self.template_digest
    }

    /// Borrows the complete planned dispositions.
    pub fn members(&self) -> &[ReviewJudgmentPlanMember] {
        &self.members
    }
}

/// Why a judgment plan does not cover exactly the complete fan-out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewJudgmentPlanFailure {
    /// The analysis pass belongs to another target.
    ForeignAnalysisTarget,
    /// The analysis pass carries another policy.
    ForeignAnalysisPolicy,
    /// The resolved judgment template differs from the attempt.
    ForeignAnalysisTemplate,
    /// A plan member is missing, extra, repeated, or not identity ordered.
    InexactFindingInventory,
    /// An accepted finding is below the frozen judgment threshold.
    AcceptedBelowThreshold { finding: ReviewFindingRef },
    /// A referenced finding is self, foreign, or outside the sealed set.
    InvalidReferencedFinding { finding: ReviewFindingRef },
    /// The reference graph contains a direct or transitive cycle.
    ReferenceCycle { finding: ReviewFindingRef },
    /// Canonical effect order would terminalize a reference before admission.
    ReferencedFindingTerminalBeforeAdmission { finding: ReviewFindingRef },
}

fn validate_plan(
    fanout: &CompleteReviewFanout,
    plan: &ReviewJudgmentPlan,
) -> Result<(), ReviewJudgmentPlanFailure> {
    let analysis_pass = plan.analysis_pass.reference();
    if analysis_pass.target() != fanout.attempt.target {
        return Err(ReviewJudgmentPlanFailure::ForeignAnalysisTarget);
    }
    if plan.template_digest != fanout.attempt.stage_templates.judgment {
        return Err(ReviewJudgmentPlanFailure::ForeignAnalysisTemplate);
    }
    if plan.analysis_pass.policy() != fanout.attempt.policy
        || plan.analysis_run.policy() != fanout.attempt.policy
        || plan.analysis_pass.kind() != ReviewPassKind::Judge
        || !matches!(
            plan.analysis_pass.state(),
            ReviewPassState::Succeeded { .. }
        )
        || plan.analysis_run.reference() != analysis_pass.run()
        || plan.analysis_run.workflow() != ReviewWorkflowKind::JudgeFindings
        || plan.analysis_run.state()
            != (ReviewRunState::Succeeded {
                concluding_pass: analysis_pass,
            })
    {
        return Err(ReviewJudgmentPlanFailure::ForeignAnalysisPolicy);
    }
    let expected: Vec<_> = fanout
        .findings
        .iter()
        .map(|finding| finding.proposal().reference())
        .collect();
    let actual: Vec<_> = plan.members.iter().map(|member| member.finding).collect();
    if actual != expected {
        return Err(ReviewJudgmentPlanFailure::InexactFindingInventory);
    }
    let expected_set: BTreeSet<_> = expected.iter().copied().collect();
    for (finding, member) in fanout.findings.iter().zip(&plan.members) {
        if matches!(member.disposition, ReviewPlannedDisposition::Accepted)
            && finding.proposal().content().is_real_confidence()
                < fanout.attempt.policy.minimum_judge_confidence()
        {
            return Err(ReviewJudgmentPlanFailure::AcceptedBelowThreshold {
                finding: member.finding,
            });
        }
        let referenced = match member.disposition {
            ReviewPlannedDisposition::Duplicate { canonical } => Some(canonical),
            ReviewPlannedDisposition::Superseded { successor } => Some(successor),
            ReviewPlannedDisposition::Accepted
            | ReviewPlannedDisposition::Rejected { .. }
            | ReviewPlannedDisposition::Stale => None,
        };
        if referenced.is_some_and(|reference| {
            reference == member.finding || !expected_set.contains(&reference)
        }) {
            return Err(ReviewJudgmentPlanFailure::InvalidReferencedFinding {
                finding: member.finding,
            });
        }
    }
    validate_reference_graph(&expected, &plan.members)?;
    Ok(())
}

fn validate_reference_graph(
    expected: &[ReviewFindingRef],
    members: &[ReviewJudgmentPlanMember],
) -> Result<(), ReviewJudgmentPlanFailure> {
    let expected_set: BTreeSet<_> = expected.iter().copied().collect();
    let edges: HashMap<_, _> = members
        .iter()
        .filter_map(|member| match member.disposition {
            ReviewPlannedDisposition::Duplicate { canonical } => Some((member.finding, canonical)),
            ReviewPlannedDisposition::Superseded { successor } => Some((member.finding, successor)),
            ReviewPlannedDisposition::Accepted
            | ReviewPlannedDisposition::Rejected { .. }
            | ReviewPlannedDisposition::Stale => None,
        })
        .collect();
    for member in members {
        if edges.get(&member.finding).is_some_and(|reference| {
            *reference == member.finding || !expected_set.contains(reference)
        }) {
            return Err(ReviewJudgmentPlanFailure::InvalidReferencedFinding {
                finding: member.finding,
            });
        }
    }
    for start in expected {
        let mut visited = BTreeSet::new();
        let mut current = *start;
        while let Some(next) = edges.get(&current) {
            if !visited.insert(current) {
                return Err(ReviewJudgmentPlanFailure::ReferenceCycle { finding: *start });
            }
            current = *next;
        }
    }
    let positions: HashMap<_, _> = expected
        .iter()
        .enumerate()
        .map(|(position, finding)| (*finding, position))
        .collect();
    for (position, member) in members.iter().enumerate() {
        let Some(reference) = edges.get(&member.finding) else {
            continue;
        };
        let Some(reference_position) = positions.get(reference).copied() else {
            return Err(ReviewJudgmentPlanFailure::InvalidReferencedFinding {
                finding: member.finding,
            });
        };
        let Some(reference_member) = members.get(reference_position) else {
            return Err(ReviewJudgmentPlanFailure::InvalidReferencedFinding {
                finding: member.finding,
            });
        };
        if reference_position < position
            && !matches!(
                reference_member.disposition,
                ReviewPlannedDisposition::Accepted
            )
        {
            return Err(
                ReviewJudgmentPlanFailure::ReferencedFindingTerminalBeforeAdmission {
                    finding: member.finding,
                },
            );
        }
    }
    Ok(())
}

/// Stable idempotency identity for one planned finding effect.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReviewJudgmentEffectId {
    attempt: ReviewOrchestrationAttemptId,
    finding: ReviewFindingRef,
}

impl ReviewJudgmentEffectId {
    /// Binds an effect to one attempt and finding.
    pub const fn new(attempt: ReviewOrchestrationAttemptId, finding: ReviewFindingRef) -> Self {
        Self { attempt, finding }
    }

    /// Returns the attempt identity.
    pub const fn attempt(self) -> ReviewOrchestrationAttemptId {
        self.attempt
    }

    /// Returns the finding identity.
    pub const fn finding(self) -> ReviewFindingRef {
        self.finding
    }
}

/// Work for one idempotently resumed judgment or deduplication effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewJudgmentEffectWork {
    id: ReviewJudgmentEffectId,
    attempt: ReviewOrchestrationAttempt,
    member: ReviewJudgmentPlanMember,
}

impl ReviewJudgmentEffectWork {
    /// Returns the stable effect identity.
    pub const fn id(&self) -> ReviewJudgmentEffectId {
        self.id
    }
    /// Borrows the attempt.
    pub const fn attempt(&self) -> &ReviewOrchestrationAttempt {
        &self.attempt
    }
    /// Borrows the exact sealed plan member.
    pub const fn member(&self) -> &ReviewJudgmentPlanMember {
        &self.member
    }
}

/// Terminal outcome of applying one planned finding effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewJudgmentEffectOutcome {
    Applied,
    Failed,
    Blocked,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AppliedJudgmentPlan {
    fanout: CompleteReviewFanout,
    plan: ReviewJudgmentPlan,
}

/// One terminal finding-repair pass outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewRepairMemberOutcome {
    Fixed(ReviewFindingRef),
    Failed(ReviewFindingRef),
    Cancelled(ReviewFindingRef),
    Blocked(ReviewFindingRef),
}

impl ReviewRepairMemberOutcome {
    const fn finding(self) -> ReviewFindingRef {
        match self {
            Self::Fixed(finding)
            | Self::Failed(finding)
            | Self::Cancelled(finding)
            | Self::Blocked(finding) => finding,
        }
    }
}

/// Work for all repair passes selected by one fully applied judgment plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewRepairWork {
    attempt: ReviewOrchestrationAttempt,
    findings: Vec<ReviewFindingRef>,
}

impl ReviewRepairWork {
    /// Borrows the immutable attempt.
    pub const fn attempt(&self) -> &ReviewOrchestrationAttempt {
        &self.attempt
    }
    /// Borrows the exact canonical repair inventory.
    pub fn findings(&self) -> &[ReviewFindingRef] {
        &self.findings
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CompleteRepairBarrier {
    attempt: ReviewOrchestrationAttempt,
    surviving: Vec<ReviewFindingRef>,
}

/// One terminal external-publication pass outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewPublicationMemberOutcome {
    Published(ReviewFindingRef),
    Failed(ReviewFindingRef),
    Blocked(ReviewFindingRef),
    Cancelled(ReviewFindingRef),
}

impl ReviewPublicationMemberOutcome {
    const fn finding(self) -> ReviewFindingRef {
        match self {
            Self::Published(finding)
            | Self::Failed(finding)
            | Self::Blocked(finding)
            | Self::Cancelled(finding) => finding,
        }
    }
}

/// Work for publication through existing reservation-then-attachment passes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewPublicationWork {
    attempt: ReviewOrchestrationAttempt,
    findings: Vec<ReviewFindingRef>,
}

impl ReviewPublicationWork {
    /// Borrows the immutable attempt.
    pub const fn attempt(&self) -> &ReviewOrchestrationAttempt {
        &self.attempt
    }
    /// Borrows the exact canonical publication inventory.
    pub fn findings(&self) -> &[ReviewFindingRef] {
        &self.findings
    }
}

/// Why a repair or publication result does not cover its exact input inventory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewTerminalBarrierFailure {
    InexactFindingInventory,
}

fn accepted_findings(plan: &ReviewJudgmentPlan) -> Vec<ReviewFindingRef> {
    plan.members
        .iter()
        .filter_map(|member| {
            matches!(member.disposition, ReviewPlannedDisposition::Accepted)
                .then_some(member.finding)
        })
        .collect()
}

#[derive(Debug, Eq, PartialEq)]
enum CompletedRepairStage {
    Blocked(Vec<ReviewRepairMemberOutcome>),
    Complete(Box<CompleteRepairBarrier>),
}

fn complete_repairs(
    attempt: &ReviewOrchestrationAttempt,
    plan: &ReviewJudgmentPlan,
    outcomes: Vec<ReviewRepairMemberOutcome>,
) -> Result<CompletedRepairStage, ReviewTerminalBarrierFailure> {
    let expected = accepted_findings(plan);
    let actual: Vec<_> = outcomes.iter().map(|outcome| outcome.finding()).collect();
    if actual != expected {
        return Err(ReviewTerminalBarrierFailure::InexactFindingInventory);
    }
    if outcomes
        .iter()
        .any(|outcome| matches!(outcome, ReviewRepairMemberOutcome::Blocked(_)))
    {
        return Ok(CompletedRepairStage::Blocked(outcomes));
    }
    let surviving = outcomes
        .into_iter()
        .filter_map(|outcome| match outcome {
            ReviewRepairMemberOutcome::Failed(finding)
            | ReviewRepairMemberOutcome::Cancelled(finding) => Some(finding),
            ReviewRepairMemberOutcome::Fixed(_) | ReviewRepairMemberOutcome::Blocked(_) => None,
        })
        .collect();
    Ok(CompletedRepairStage::Complete(Box::new(
        CompleteRepairBarrier {
            attempt: attempt.clone(),
            surviving,
        },
    )))
}

fn validate_publication(
    repairs: &CompleteRepairBarrier,
    outcomes: &[ReviewPublicationMemberOutcome],
) -> Result<(), ReviewTerminalBarrierFailure> {
    let actual: Vec<_> = outcomes.iter().map(|outcome| outcome.finding()).collect();
    if actual == repairs.surviving {
        Ok(())
    } else {
        Err(ReviewTerminalBarrierFailure::InexactFindingInventory)
    }
}

/// Durable application-owned boundary for orchestration attempts and barriers.
pub trait ReviewOrchestrationAttemptStore {
    type Error;
    fn record_attempt(
        &mut self,
        attempt: ReviewOrchestrationAttempt,
    ) -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn load_import(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> impl Future<Output = Result<Option<ReviewImportOutcome>, Self::Error>> + Send;
    fn record_import(
        &mut self,
        attempt: ReviewOrchestrationAttemptId,
        outcome: ReviewImportOutcome,
    ) -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn load_concern_claims(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> impl Future<Output = Result<Vec<ReviewConcernClaim>, Self::Error>> + Send;
    /// Replaces only the current retryable failed claim in one expected slot; historical attempts remain store evidence but are not returned as current claims.
    /// A distinct successful, blocked, cancelled, or superseded current claim conflicts.
    fn record_concern_claim(
        &mut self,
        attempt: ReviewOrchestrationAttemptId,
        claim: ReviewConcernClaim,
    ) -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn seal_complete_fanout(
        &mut self,
        attempt: ReviewOrchestrationAttemptId,
        claims: Vec<ReviewConcernClaim>,
    ) -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn seal_judgment_plan(
        &mut self,
        attempt: ReviewOrchestrationAttemptId,
        plan: ReviewJudgmentPlan,
    ) -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn load_judgment_plan(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> impl Future<Output = Result<Option<ReviewJudgmentPlan>, Self::Error>> + Send;
    fn load_applied_judgment_effects(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> impl Future<Output = Result<Vec<ReviewJudgmentEffectId>, Self::Error>> + Send;
    fn record_applied_judgment_effect(
        &mut self,
        effect: ReviewJudgmentEffectId,
    ) -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn seal_repair_inventory(
        &mut self,
        attempt: ReviewOrchestrationAttemptId,
        findings: Vec<ReviewFindingRef>,
    ) -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn record_repair_outcomes(
        &mut self,
        attempt: ReviewOrchestrationAttemptId,
        outcomes: Vec<ReviewRepairMemberOutcome>,
    ) -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn load_repair_outcomes(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> impl Future<Output = Result<Option<Vec<ReviewRepairMemberOutcome>>, Self::Error>> + Send;
    fn seal_publication_inventory(
        &mut self,
        attempt: ReviewOrchestrationAttemptId,
        findings: Vec<ReviewFindingRef>,
    ) -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn record_publication_outcomes(
        &mut self,
        attempt: ReviewOrchestrationAttemptId,
        outcomes: Vec<ReviewPublicationMemberOutcome>,
    ) -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn load_publication_outcomes(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> impl Future<Output = Result<Option<Vec<ReviewPublicationMemberOutcome>>, Self::Error>> + Send;
}

/// Port realizing existing session-backed review and publication passes.
pub trait ReviewOrchestrationPassRunner: Send + Sync + 'static {
    type Error: Send + 'static;
    fn import_external_context(
        &self,
        attempt: ReviewOrchestrationAttempt,
    ) -> impl Future<Output = Result<ReviewImportOutcome, Self::Error>> + Send;
    fn run_concern(
        &self,
        work: ReviewConcernWork,
    ) -> impl Future<Output = Result<ReviewConcernOutcome, Self::Error>> + Send + 'static;
    fn judge(
        &self,
        attempt: ReviewOrchestrationAttempt,
        findings: Vec<ReviewFinding>,
    ) -> impl Future<Output = Result<ReviewJudgmentPlan, Self::Error>> + Send;
    fn apply_judgment_effect(
        &self,
        work: ReviewJudgmentEffectWork,
    ) -> impl Future<Output = Result<ReviewJudgmentEffectOutcome, Self::Error>> + Send;
    fn repair(
        &self,
        work: ReviewRepairWork,
    ) -> impl Future<Output = Result<Vec<ReviewRepairMemberOutcome>, Self::Error>> + Send;
    fn publish(
        &self,
        work: ReviewPublicationWork,
    ) -> impl Future<Output = Result<Vec<ReviewPublicationMemberOutcome>, Self::Error>> + Send;
}

/// The durable stage at which an attempt stopped or completed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewOrchestrationOutcome {
    ImportIncomplete(Box<ReviewImportOutcome>),
    FanoutIncomplete(ReviewFanoutBarrierFailure),
    JudgmentIncomplete {
        effect: ReviewJudgmentEffectId,
        outcome: ReviewJudgmentEffectOutcome,
    },
    RepairIncomplete {
        repairs: Vec<ReviewRepairMemberOutcome>,
    },
    PublicationIncomplete {
        publications: Vec<ReviewPublicationMemberOutcome>,
    },
    Complete {
        publications: Vec<ReviewPublicationMemberOutcome>,
    },
}

/// Infrastructure, corruption, or immutable-seal failure while resuming an attempt.
#[derive(Debug)]
pub enum ReviewOrchestrationServiceError<StoreError, RunnerError> {
    Store(StoreError),
    InvalidImportEvidence(ReviewImportEvidenceFailure),
    Runner(RunnerError),
    ConcernTaskTerminated,
    DurableConflict,
    InvalidJudgmentPlan(ReviewJudgmentPlanFailure),
    InvalidAppliedEffects,
    InvalidTerminalBarrier(ReviewTerminalBarrierFailure),
}

/// Resumable application service for the complete review pipeline.
#[derive(Debug)]
pub struct ReviewOrchestrationService<Store, Runner> {
    store: Store,
    runner: Arc<Runner>,
}

impl<Store, Runner> ReviewOrchestrationService<Store, Runner> {
    /// Composes the durable attempt store with session-backed pass execution.
    pub fn new(store: Store, runner: Runner) -> Self {
        Self {
            store,
            runner: Arc::new(runner),
        }
    }
}

impl<Store, Runner> ReviewOrchestrationService<Store, Runner>
where
    Store: ReviewOrchestrationAttemptStore,
    Runner: ReviewOrchestrationPassRunner,
{
    /// Records then resumes import, concurrent fan-out, judgment, repair, and publication.
    pub async fn execute(
        &mut self,
        attempt: ReviewOrchestrationAttempt,
    ) -> Result<
        ReviewOrchestrationOutcome,
        ReviewOrchestrationServiceError<Store::Error, Runner::Error>,
    > {
        ensure_sealed(
            self.store
                .record_attempt(attempt.clone())
                .await
                .map_err(ReviewOrchestrationServiceError::Store)?,
        )?;
        let imported = match self
            .store
            .load_import(attempt.id)
            .await
            .map_err(ReviewOrchestrationServiceError::Store)?
        {
            Some(outcome) => outcome,
            None => {
                let outcome = self
                    .runner
                    .import_external_context(attempt.clone())
                    .await
                    .map_err(ReviewOrchestrationServiceError::Runner)?;
                validate_import(&attempt, &outcome)
                    .map_err(ReviewOrchestrationServiceError::InvalidImportEvidence)?;
                ensure_sealed(
                    self.store
                        .record_import(attempt.id, outcome.clone())
                        .await
                        .map_err(ReviewOrchestrationServiceError::Store)?,
                )?;
                outcome
            }
        };
        validate_import(&attempt, &imported)
            .map_err(ReviewOrchestrationServiceError::InvalidImportEvidence)?;
        let ReviewImportOutcome::Succeeded { context_digest, .. } = imported else {
            return Ok(ReviewOrchestrationOutcome::ImportIncomplete(Box::new(
                imported,
            )));
        };

        let existing = self
            .store
            .load_concern_claims(attempt.id)
            .await
            .map_err(ReviewOrchestrationServiceError::Store)?;
        let members_to_run = retryable_members(&attempt, &existing);
        let mut tasks = JoinSet::new();
        for concern in members_to_run {
            let runner = Arc::clone(&self.runner);
            let work = ReviewConcernWork {
                attempt: attempt.clone(),
                imported_context_digest: context_digest,
                concern,
            };
            tasks.spawn(async move {
                let key = work.concern.key.clone();
                let digest = work.concern.template_digest;
                let outcome = runner.run_concern(work).await;
                (key, digest, outcome)
            });
        }
        while let Some(joined) = tasks.join_next().await {
            let (concern, template_digest, outcome) =
                joined.map_err(|_| ReviewOrchestrationServiceError::ConcernTaskTerminated)?;
            let outcome = outcome.map_err(ReviewOrchestrationServiceError::Runner)?;
            ensure_sealed(
                self.store
                    .record_concern_claim(
                        attempt.id,
                        ReviewConcernClaim::new(concern, template_digest, outcome),
                    )
                    .await
                    .map_err(ReviewOrchestrationServiceError::Store)?,
            )?;
        }

        let claims = self
            .store
            .load_concern_claims(attempt.id)
            .await
            .map_err(ReviewOrchestrationServiceError::Store)?;
        let fanout = match complete_fanout(&attempt, claims) {
            Ok(fanout) => fanout,
            Err(failure) => return Ok(ReviewOrchestrationOutcome::FanoutIncomplete(failure)),
        };
        ensure_sealed(
            self.store
                .seal_complete_fanout(attempt.id, fanout.members.clone())
                .await
                .map_err(ReviewOrchestrationServiceError::Store)?,
        )?;

        let plan = match self
            .store
            .load_judgment_plan(attempt.id)
            .await
            .map_err(ReviewOrchestrationServiceError::Store)?
        {
            Some(plan) => plan,
            None => {
                let plan = self
                    .runner
                    .judge(attempt.clone(), fanout.findings.clone())
                    .await
                    .map_err(ReviewOrchestrationServiceError::Runner)?;
                validate_plan(&fanout, &plan)
                    .map_err(ReviewOrchestrationServiceError::InvalidJudgmentPlan)?;
                ensure_sealed(
                    self.store
                        .seal_judgment_plan(attempt.id, plan.clone())
                        .await
                        .map_err(ReviewOrchestrationServiceError::Store)?,
                )?;
                plan
            }
        };
        validate_plan(&fanout, &plan)
            .map_err(ReviewOrchestrationServiceError::InvalidJudgmentPlan)?;

        let applied = self
            .store
            .load_applied_judgment_effects(attempt.id)
            .await
            .map_err(ReviewOrchestrationServiceError::Store)?;
        let mut applied_set = BTreeSet::new();
        if applied.iter().any(|effect| {
            effect.attempt != attempt.id
                || !plan
                    .members
                    .iter()
                    .any(|member| member.finding == effect.finding)
                || !applied_set.insert(*effect)
        }) {
            return Err(ReviewOrchestrationServiceError::InvalidAppliedEffects);
        }
        for member in &plan.members {
            let effect = ReviewJudgmentEffectId::new(attempt.id, member.finding);
            if applied_set.contains(&effect) {
                continue;
            }
            let outcome = self
                .runner
                .apply_judgment_effect(ReviewJudgmentEffectWork {
                    id: effect,
                    attempt: attempt.clone(),
                    member: member.clone(),
                })
                .await
                .map_err(ReviewOrchestrationServiceError::Runner)?;
            if outcome != ReviewJudgmentEffectOutcome::Applied {
                return Ok(ReviewOrchestrationOutcome::JudgmentIncomplete { effect, outcome });
            }
            ensure_sealed(
                self.store
                    .record_applied_judgment_effect(effect)
                    .await
                    .map_err(ReviewOrchestrationServiceError::Store)?,
            )?;
        }

        let applied = AppliedJudgmentPlan { fanout, plan };
        let repairs = ReviewRepairWork {
            attempt: attempt.clone(),
            findings: accepted_findings(&applied.plan),
        };
        ensure_sealed(
            self.store
                .seal_repair_inventory(attempt.id, repairs.findings.clone())
                .await
                .map_err(ReviewOrchestrationServiceError::Store)?,
        )?;
        let repair_outcomes = match self
            .store
            .load_repair_outcomes(attempt.id)
            .await
            .map_err(ReviewOrchestrationServiceError::Store)?
        {
            Some(outcomes) => outcomes,
            None => {
                let outcomes = self
                    .runner
                    .repair(repairs)
                    .await
                    .map_err(ReviewOrchestrationServiceError::Runner)?;
                complete_repairs(&attempt, &applied.plan, outcomes.clone())
                    .map_err(ReviewOrchestrationServiceError::InvalidTerminalBarrier)?;
                ensure_sealed(
                    self.store
                        .record_repair_outcomes(attempt.id, outcomes.clone())
                        .await
                        .map_err(ReviewOrchestrationServiceError::Store)?,
                )?;
                outcomes
            }
        };
        let repair_barrier = complete_repairs(&attempt, &applied.plan, repair_outcomes)
            .map_err(ReviewOrchestrationServiceError::InvalidTerminalBarrier)?;
        let repair_barrier = match repair_barrier {
            CompletedRepairStage::Blocked(repairs) => {
                return Ok(ReviewOrchestrationOutcome::RepairIncomplete { repairs });
            }
            CompletedRepairStage::Complete(barrier) => *barrier,
        };

        ensure_sealed(
            self.store
                .seal_publication_inventory(attempt.id, repair_barrier.surviving.clone())
                .await
                .map_err(ReviewOrchestrationServiceError::Store)?,
        )?;
        let publications = match self
            .store
            .load_publication_outcomes(attempt.id)
            .await
            .map_err(ReviewOrchestrationServiceError::Store)?
        {
            Some(outcomes) => outcomes,
            None => {
                let outcomes = self
                    .runner
                    .publish(ReviewPublicationWork {
                        attempt: repair_barrier.attempt.clone(),
                        findings: repair_barrier.surviving.clone(),
                    })
                    .await
                    .map_err(ReviewOrchestrationServiceError::Runner)?;
                validate_publication(&repair_barrier, &outcomes)
                    .map_err(ReviewOrchestrationServiceError::InvalidTerminalBarrier)?;
                ensure_sealed(
                    self.store
                        .record_publication_outcomes(attempt.id, outcomes.clone())
                        .await
                        .map_err(ReviewOrchestrationServiceError::Store)?,
                )?;
                outcomes
            }
        };
        validate_publication(&repair_barrier, &publications)
            .map_err(ReviewOrchestrationServiceError::InvalidTerminalBarrier)?;
        if publications
            .iter()
            .all(|outcome| matches!(outcome, ReviewPublicationMemberOutcome::Published(_)))
        {
            Ok(ReviewOrchestrationOutcome::Complete { publications })
        } else {
            Ok(ReviewOrchestrationOutcome::PublicationIncomplete { publications })
        }
    }
}

fn ensure_sealed<StoreError, RunnerError>(
    outcome: ReviewDurableSealOutcome,
) -> Result<(), ReviewOrchestrationServiceError<StoreError, RunnerError>> {
    match outcome {
        ReviewDurableSealOutcome::Recorded | ReviewDurableSealOutcome::EqualReplay => Ok(()),
        ReviewDurableSealOutcome::Conflict => Err(ReviewOrchestrationServiceError::DurableConflict),
    }
}

fn retryable_members(
    attempt: &ReviewOrchestrationAttempt,
    claims: &[ReviewConcernClaim],
) -> Vec<ReviewConcernSpec> {
    attempt
        .concerns
        .iter()
        .filter(|expected| {
            let matching: Vec<_> = claims
                .iter()
                .filter(|claim| claim.concern == expected.key)
                .collect();
            matching.is_empty()
                || (matching.len() == 1
                    && matching.first().is_some_and(|claim| {
                        matches!(claim.outcome, ReviewConcernOutcome::Failed { .. })
                    }))
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use signalbox_domain::{
        AcceptedInputId, ContextFrontierId, ReviewPass, ReviewPassAcceptedInputEvidence,
        ReviewPassId, ReviewPassTurnEvidence, ReviewPassTurnOutcome, ReviewProducedFindings,
        ReviewRun, ReviewRunId, ReviewRunRef, SessionId, TurnId,
    };

    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum RunnerMode {
        Partial,
        Complete,
    }

    #[derive(Debug)]
    struct FakeRunner {
        mode: RunnerMode,
        attempt_recorded: Arc<AtomicBool>,
        inventory_order_violations: Arc<AtomicUsize>,
        judgment_calls: Arc<AtomicUsize>,
        repair_calls: Arc<AtomicUsize>,
        publication_calls: Arc<AtomicUsize>,
    }

    impl FakeRunner {
        fn new(mode: RunnerMode, attempt_recorded: Arc<AtomicBool>) -> Self {
            Self {
                mode,
                attempt_recorded,
                inventory_order_violations: Arc::new(AtomicUsize::new(0)),
                judgment_calls: Arc::new(AtomicUsize::new(0)),
                repair_calls: Arc::new(AtomicUsize::new(0)),
                publication_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl ReviewOrchestrationPassRunner for FakeRunner {
        type Error = ();

        async fn import_external_context(
            &self,
            attempt: ReviewOrchestrationAttempt,
        ) -> Result<ReviewImportOutcome, Self::Error> {
            let (pass, run) = succeeded_evidence(
                attempt.target,
                attempt.policy,
                10,
                ReviewPassKind::ImportExternalContext,
                ReviewWorkflowKind::ImportExternalContext,
                None,
            );
            Ok(ReviewImportOutcome::Succeeded {
                pass: Box::new(pass),
                run,
                template_digest: attempt.stage_templates.import,
                context_digest: [90; 32],
            })
        }

        fn run_concern(
            &self,
            work: ReviewConcernWork,
        ) -> impl Future<Output = Result<ReviewConcernOutcome, Self::Error>> + Send + 'static
        {
            let mode = self.mode;
            let recorded = Arc::clone(&self.attempt_recorded);
            let violations = Arc::clone(&self.inventory_order_violations);
            async move {
                if !recorded.load(Ordering::SeqCst) {
                    violations.fetch_add(1, Ordering::SeqCst);
                }
                let seed = if work.concern.key.as_str() == "defects" {
                    20
                } else {
                    30
                };
                let (pass, run) = succeeded_evidence(
                    work.attempt.target,
                    work.attempt.policy,
                    seed,
                    ReviewPassKind::ReadOnlyReview,
                    ReviewWorkflowKind::ReadOnlyReview,
                    Some(ReviewPassResult::ProducedFindings(
                        ReviewProducedFindings::try_new(Vec::new())
                            .expect("empty inventory is canonical"),
                    )),
                );
                if mode == RunnerMode::Partial && work.concern.key.as_str() == "naming" {
                    Ok(ReviewConcernOutcome::Failed {
                        pass: pass.reference(),
                    })
                } else {
                    Ok(ReviewConcernOutcome::Succeeded(Box::new(
                        ReviewConcernSuccess::new(pass, run, Vec::new()),
                    )))
                }
            }
        }

        async fn judge(
            &self,
            attempt: ReviewOrchestrationAttempt,
            _findings: Vec<ReviewFinding>,
        ) -> Result<ReviewJudgmentPlan, Self::Error> {
            self.judgment_calls.fetch_add(1, Ordering::SeqCst);
            let (pass, run) = succeeded_evidence(
                attempt.target,
                attempt.policy,
                40,
                ReviewPassKind::Judge,
                ReviewWorkflowKind::JudgeFindings,
                None,
            );
            Ok(ReviewJudgmentPlan::new(
                pass,
                run,
                attempt.stage_templates.judgment,
                Vec::new(),
            ))
        }

        async fn apply_judgment_effect(
            &self,
            _work: ReviewJudgmentEffectWork,
        ) -> Result<ReviewJudgmentEffectOutcome, Self::Error> {
            Ok(ReviewJudgmentEffectOutcome::Applied)
        }

        async fn repair(
            &self,
            _work: ReviewRepairWork,
        ) -> Result<Vec<ReviewRepairMemberOutcome>, Self::Error> {
            self.repair_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }

        async fn publish(
            &self,
            _work: ReviewPublicationWork,
        ) -> Result<Vec<ReviewPublicationMemberOutcome>, Self::Error> {
            self.publication_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
    }

    #[derive(Debug)]
    struct FakeStore {
        attempt_recorded: Arc<AtomicBool>,
        attempt: Option<ReviewOrchestrationAttempt>,
        import: Option<ReviewImportOutcome>,
        claims: Vec<ReviewConcernClaim>,
        fanout: Option<Vec<ReviewConcernClaim>>,
        plan: Option<ReviewJudgmentPlan>,
        effects: Vec<ReviewJudgmentEffectId>,
        repair_inventory: Option<Vec<ReviewFindingRef>>,
        repairs: Option<Vec<ReviewRepairMemberOutcome>>,
        publication_inventory: Option<Vec<ReviewFindingRef>>,
        publications: Option<Vec<ReviewPublicationMemberOutcome>>,
    }

    impl FakeStore {
        fn new(attempt_recorded: Arc<AtomicBool>) -> Self {
            Self {
                attempt_recorded,
                attempt: None,
                import: None,
                claims: Vec::new(),
                fanout: None,
                plan: None,
                effects: Vec::new(),
                repair_inventory: None,
                repairs: None,
                publication_inventory: None,
                publications: None,
            }
        }
    }

    impl ReviewOrchestrationAttemptStore for FakeStore {
        type Error = ();

        async fn record_attempt(
            &mut self,
            attempt: ReviewOrchestrationAttempt,
        ) -> Result<ReviewDurableSealOutcome, Self::Error> {
            self.attempt_recorded.store(true, Ordering::SeqCst);
            if self
                .attempt
                .as_ref()
                .is_some_and(|current| current != &attempt)
            {
                return Ok(ReviewDurableSealOutcome::Conflict);
            }
            let replay = self.attempt.is_some();
            self.attempt = Some(attempt);
            Ok(seal_outcome(replay))
        }

        async fn load_import(
            &self,
            _attempt: ReviewOrchestrationAttemptId,
        ) -> Result<Option<ReviewImportOutcome>, Self::Error> {
            Ok(self.import.clone())
        }

        async fn record_import(
            &mut self,
            _attempt: ReviewOrchestrationAttemptId,
            outcome: ReviewImportOutcome,
        ) -> Result<ReviewDurableSealOutcome, Self::Error> {
            let replay = self.import.is_some();
            self.import = Some(outcome);
            Ok(seal_outcome(replay))
        }

        async fn load_concern_claims(
            &self,
            _attempt: ReviewOrchestrationAttemptId,
        ) -> Result<Vec<ReviewConcernClaim>, Self::Error> {
            Ok(self.claims.clone())
        }

        async fn record_concern_claim(
            &mut self,
            _attempt: ReviewOrchestrationAttemptId,
            claim: ReviewConcernClaim,
        ) -> Result<ReviewDurableSealOutcome, Self::Error> {
            self.claims
                .retain(|current| current.concern != claim.concern);
            self.claims.push(claim);
            Ok(ReviewDurableSealOutcome::Recorded)
        }

        async fn seal_complete_fanout(
            &mut self,
            _attempt: ReviewOrchestrationAttemptId,
            claims: Vec<ReviewConcernClaim>,
        ) -> Result<ReviewDurableSealOutcome, Self::Error> {
            let replay = self.fanout.is_some();
            self.fanout = Some(claims);
            Ok(seal_outcome(replay))
        }

        async fn seal_judgment_plan(
            &mut self,
            _attempt: ReviewOrchestrationAttemptId,
            plan: ReviewJudgmentPlan,
        ) -> Result<ReviewDurableSealOutcome, Self::Error> {
            let replay = self.plan.is_some();
            self.plan = Some(plan);
            Ok(seal_outcome(replay))
        }

        async fn load_judgment_plan(
            &self,
            _attempt: ReviewOrchestrationAttemptId,
        ) -> Result<Option<ReviewJudgmentPlan>, Self::Error> {
            Ok(self.plan.clone())
        }

        async fn load_applied_judgment_effects(
            &self,
            _attempt: ReviewOrchestrationAttemptId,
        ) -> Result<Vec<ReviewJudgmentEffectId>, Self::Error> {
            Ok(self.effects.clone())
        }

        async fn record_applied_judgment_effect(
            &mut self,
            effect: ReviewJudgmentEffectId,
        ) -> Result<ReviewDurableSealOutcome, Self::Error> {
            self.effects.push(effect);
            Ok(ReviewDurableSealOutcome::Recorded)
        }

        async fn seal_repair_inventory(
            &mut self,
            _attempt: ReviewOrchestrationAttemptId,
            findings: Vec<ReviewFindingRef>,
        ) -> Result<ReviewDurableSealOutcome, Self::Error> {
            let replay = self.repair_inventory.is_some();
            self.repair_inventory = Some(findings);
            Ok(seal_outcome(replay))
        }

        async fn record_repair_outcomes(
            &mut self,
            _attempt: ReviewOrchestrationAttemptId,
            outcomes: Vec<ReviewRepairMemberOutcome>,
        ) -> Result<ReviewDurableSealOutcome, Self::Error> {
            self.repairs = Some(outcomes);
            Ok(ReviewDurableSealOutcome::Recorded)
        }

        async fn load_repair_outcomes(
            &self,
            _attempt: ReviewOrchestrationAttemptId,
        ) -> Result<Option<Vec<ReviewRepairMemberOutcome>>, Self::Error> {
            Ok(self.repairs.clone())
        }

        async fn seal_publication_inventory(
            &mut self,
            _attempt: ReviewOrchestrationAttemptId,
            findings: Vec<ReviewFindingRef>,
        ) -> Result<ReviewDurableSealOutcome, Self::Error> {
            let replay = self.publication_inventory.is_some();
            self.publication_inventory = Some(findings);
            Ok(seal_outcome(replay))
        }

        async fn record_publication_outcomes(
            &mut self,
            _attempt: ReviewOrchestrationAttemptId,
            outcomes: Vec<ReviewPublicationMemberOutcome>,
        ) -> Result<ReviewDurableSealOutcome, Self::Error> {
            self.publications = Some(outcomes);
            Ok(ReviewDurableSealOutcome::Recorded)
        }

        async fn load_publication_outcomes(
            &self,
            _attempt: ReviewOrchestrationAttemptId,
        ) -> Result<Option<Vec<ReviewPublicationMemberOutcome>>, Self::Error> {
            Ok(self.publications.clone())
        }
    }

    #[test]
    fn missing_concern_cannot_form_complete_barrier() {
        let immutable_attempt = attempt();

        let error = complete_fanout(&immutable_attempt, Vec::new())
            .expect_err("missing members cannot seal");

        assert!(matches!(
            error,
            ReviewFanoutBarrierFailure::MissingConcern { .. }
        ));
    }

    #[test]
    fn extra_concern_cannot_form_complete_barrier() {
        let immutable_attempt = attempt();
        let claims = vec![failed_claim("outside-config", 100)];

        let error =
            complete_fanout(&immutable_attempt, claims).expect_err("extra members cannot seal");

        assert!(matches!(
            error,
            ReviewFanoutBarrierFailure::ExtraConcern { .. }
        ));
    }

    #[test]
    fn repeated_concern_cannot_form_complete_barrier() {
        let immutable_attempt = attempt();
        let claims = vec![failed_claim("defects", 100), failed_claim("defects", 200)];

        let error = complete_fanout(&immutable_attempt, claims)
            .expect_err("repeated current members cannot seal");

        assert!(matches!(
            error,
            ReviewFanoutBarrierFailure::RepeatedConcern { .. }
        ));
    }

    #[test]
    fn direct_reference_cycle_is_rejected_before_plan_sealing() {
        let first = finding_ref(100);
        let second = finding_ref(200);
        let members = vec![
            ReviewJudgmentPlanMember::new(
                first,
                ReviewPlannedDisposition::Duplicate { canonical: second },
            ),
            ReviewJudgmentPlanMember::new(
                second,
                ReviewPlannedDisposition::Superseded { successor: first },
            ),
        ];

        let error = validate_reference_graph(&[first, second], &members)
            .expect_err("direct cycle must fail closed");

        assert!(matches!(
            error,
            ReviewJudgmentPlanFailure::ReferenceCycle { .. }
        ));
    }

    #[test]
    fn transitive_reference_cycle_is_rejected_before_plan_sealing() {
        let first = finding_ref(100);
        let second = finding_ref(200);
        let third = finding_ref(300);
        let members = vec![
            ReviewJudgmentPlanMember::new(
                first,
                ReviewPlannedDisposition::Duplicate { canonical: second },
            ),
            ReviewJudgmentPlanMember::new(
                second,
                ReviewPlannedDisposition::Superseded { successor: third },
            ),
            ReviewJudgmentPlanMember::new(
                third,
                ReviewPlannedDisposition::Duplicate { canonical: first },
            ),
        ];

        let error = validate_reference_graph(&[first, second, third], &members)
            .expect_err("transitive cycle must fail closed");

        assert!(matches!(
            error,
            ReviewJudgmentPlanFailure::ReferenceCycle { .. }
        ));
    }

    #[test]
    fn canonical_order_cannot_terminalize_a_later_reference() {
        let first = finding_ref(100);
        let second = finding_ref(200);
        let members = vec![
            ReviewJudgmentPlanMember::new(first, ReviewPlannedDisposition::Stale),
            ReviewJudgmentPlanMember::new(
                second,
                ReviewPlannedDisposition::Duplicate { canonical: first },
            ),
        ];

        let error = validate_reference_graph(&[first, second], &members)
            .expect_err("terminal reference cannot remain eligible");

        assert_eq!(
            error,
            ReviewJudgmentPlanFailure::ReferencedFindingTerminalBeforeAdmission { finding: second }
        );
    }

    #[test]
    fn blocked_repair_stage_cannot_form_publication_barrier() {
        let immutable_attempt = attempt();
        let finding = finding_ref(100);
        let (analysis_pass, analysis_run) = succeeded_evidence(
            immutable_attempt.target(),
            immutable_attempt.policy(),
            40,
            ReviewPassKind::Judge,
            ReviewWorkflowKind::JudgeFindings,
            None,
        );
        let plan = ReviewJudgmentPlan::new(
            analysis_pass,
            analysis_run,
            immutable_attempt.stage_templates().judgment(),
            vec![ReviewJudgmentPlanMember::new(
                finding,
                ReviewPlannedDisposition::Accepted,
            )],
        );
        let repairs = vec![ReviewRepairMemberOutcome::Blocked(finding)];

        let stage = complete_repairs(&immutable_attempt, &plan, repairs.clone())
            .expect("exact blocked repair inventory is classified");

        assert_eq!(stage, CompletedRepairStage::Blocked(repairs));
    }

    #[tokio::test]
    async fn failed_concern_cannot_reach_judgment_repair_or_publication() {
        let attempt_recorded = Arc::new(AtomicBool::new(false));
        let runner = FakeRunner::new(RunnerMode::Partial, Arc::clone(&attempt_recorded));
        let inventory_order_violations = Arc::clone(&runner.inventory_order_violations);
        let judgment_calls = Arc::clone(&runner.judgment_calls);
        let repair_calls = Arc::clone(&runner.repair_calls);
        let publication_calls = Arc::clone(&runner.publication_calls);
        let mut service = ReviewOrchestrationService::new(FakeStore::new(attempt_recorded), runner);

        let outcome = service
            .execute(attempt())
            .await
            .expect("fake store and runner succeed");

        assert!(matches!(
            outcome,
            ReviewOrchestrationOutcome::FanoutIncomplete(
                ReviewFanoutBarrierFailure::MemberIncomplete { .. }
            )
        ));
        assert_eq!(inventory_order_violations.load(Ordering::SeqCst), 0);
        assert_eq!(judgment_calls.load(Ordering::SeqCst), 0);
        assert_eq!(repair_calls.load(Ordering::SeqCst), 0);
        assert_eq!(publication_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn sealed_plan_and_terminal_downstream_stages_resume_without_reexecution() {
        let attempt_recorded = Arc::new(AtomicBool::new(false));
        let runner = FakeRunner::new(RunnerMode::Complete, Arc::clone(&attempt_recorded));
        let judgment_calls = Arc::clone(&runner.judgment_calls);
        let repair_calls = Arc::clone(&runner.repair_calls);
        let publication_calls = Arc::clone(&runner.publication_calls);
        let mut service = ReviewOrchestrationService::new(FakeStore::new(attempt_recorded), runner);
        let immutable_attempt = attempt();

        let first = service
            .execute(immutable_attempt.clone())
            .await
            .expect("first execution succeeds");
        let replay = service
            .execute(immutable_attempt)
            .await
            .expect("resume succeeds");

        assert_eq!(
            first,
            ReviewOrchestrationOutcome::Complete {
                publications: Vec::new()
            }
        );
        assert_eq!(replay, first);
        assert_eq!(judgment_calls.load(Ordering::SeqCst), 1);
        assert_eq!(repair_calls.load(Ordering::SeqCst), 1);
        assert_eq!(publication_calls.load(Ordering::SeqCst), 1);
    }

    fn failed_claim(key: &str, seed: u128) -> ReviewConcernClaim {
        ReviewConcernClaim::new(
            ReviewKey::try_new(String::from(key)).expect("fixture key is valid"),
            ReviewTemplateDigest::new([5; 32]),
            ReviewConcernOutcome::Failed {
                pass: ReviewPassRef::new(
                    ReviewRunRef::new(
                        ReviewTargetId::from_uuid(Uuid::from_u128(2)),
                        ReviewRunId::from_uuid(Uuid::from_u128(seed)),
                    ),
                    ReviewPassId::from_uuid(Uuid::from_u128(seed + 1)),
                ),
            },
        )
    }

    fn finding_ref(seed: u128) -> ReviewFindingRef {
        let target = ReviewTargetId::from_uuid(Uuid::from_u128(2));
        let run = ReviewRunRef::new(target, ReviewRunId::from_uuid(Uuid::from_u128(seed)));
        let pass = ReviewPassRef::new(run, ReviewPassId::from_uuid(Uuid::from_u128(seed + 1)));
        ReviewFindingRef::new(pass, ReviewFindingId::from_uuid(Uuid::from_u128(seed + 2)))
    }

    fn attempt() -> ReviewOrchestrationAttempt {
        ReviewOrchestrationAttempt::try_new(
            ReviewOrchestrationAttemptId::from_uuid(Uuid::from_u128(1)),
            ReviewTargetId::from_uuid(Uuid::from_u128(2)),
            ReviewPolicy::version_one(),
            ReviewKey::try_new(String::from("concerns-v1")).expect("fixture key is valid"),
            ReviewStageTemplateDigests::new(
                ReviewTemplateDigest::new([1; 32]),
                ReviewTemplateDigest::new([2; 32]),
                ReviewTemplateDigest::new([3; 32]),
                ReviewTemplateDigest::new([4; 32]),
            ),
            vec![
                ReviewConcernSpec::new(
                    ReviewKey::try_new(String::from("defects")).expect("fixture key is valid"),
                    ReviewTemplateDigest::new([5; 32]),
                ),
                ReviewConcernSpec::new(
                    ReviewKey::try_new(String::from("naming")).expect("fixture key is valid"),
                    ReviewTemplateDigest::new([6; 32]),
                ),
            ],
        )
        .expect("fixture attempt is valid")
    }

    fn succeeded_evidence(
        target: ReviewTargetId,
        policy: ReviewPolicy,
        seed: u128,
        pass_kind: ReviewPassKind,
        workflow: ReviewWorkflowKind,
        result: Option<ReviewPassResult>,
    ) -> (ReviewPassEvidence, ReviewRunEvidence) {
        let run_ref = ReviewRunRef::new(target, ReviewRunId::from_uuid(Uuid::from_u128(seed)));
        let pass_ref =
            ReviewPassRef::new(run_ref, ReviewPassId::from_uuid(Uuid::from_u128(seed + 1)));
        let session = SessionId::from_uuid(Uuid::from_u128(seed + 2));
        let accepted = AcceptedInputId::from_uuid(Uuid::from_u128(seed + 3));
        let turn = TurnId::from_uuid(Uuid::from_u128(seed + 4));
        let frontier = ContextFrontierId::from_uuid(Uuid::from_u128(seed + 5));
        let mut run = ReviewRun::new(run_ref, workflow, policy);
        let pass = ReviewPass::try_new(
            pass_ref,
            pass_kind,
            &mut run,
            session,
            ReviewPassAcceptedInputEvidence::new(accepted, session, Some(turn)),
        )
        .expect("fixture pass is valid");
        let active_turn = ReviewPassTurnEvidence::new(
            turn,
            session,
            accepted,
            ReviewPassTurnOutcome::Active,
            None,
        );
        let pass = pass
            .transition(ReviewPassState::Running { turn }, Some(active_turn))
            .expect("fixture pass starts");
        let running = ReviewPassEvidence::from_pass(&pass, policy);
        let run = run
            .transition(
                ReviewRunState::Running {
                    active_pass: pass_ref,
                },
                Some(running),
            )
            .expect("fixture run starts");
        let completed_turn = ReviewPassTurnEvidence::new(
            turn,
            session,
            accepted,
            ReviewPassTurnOutcome::Completed,
            Some(frontier),
        );
        let pass = pass
            .transition(
                ReviewPassState::Succeeded {
                    turn,
                    output_frontier: frontier,
                    result,
                },
                Some(completed_turn),
            )
            .expect("fixture pass succeeds");
        let pass_evidence = ReviewPassEvidence::from_pass(&pass, policy);
        let run = run
            .transition(
                ReviewRunState::Succeeded {
                    concluding_pass: pass_ref,
                },
                Some(pass_evidence.clone()),
            )
            .expect("fixture run succeeds");
        (pass_evidence, run.evidence())
    }

    const fn seal_outcome(replay: bool) -> ReviewDurableSealOutcome {
        if replay {
            ReviewDurableSealOutcome::EqualReplay
        } else {
            ReviewDurableSealOutcome::Recorded
        }
    }
}
