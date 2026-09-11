//! PostgreSQL store for review-workflow aggregates.
//!
//! SQL rows remain adapter-private. Complete values are reconstructed through
//! the domain API defined by `docs/spec/review-workflows.md`.

mod decode;
mod load;
mod pass_codec;
mod store;
mod store_external;
mod store_finding;
mod write;

pub(crate) use load::load_pass_on_connection;

use signalbox_application::ReviewWorkflowReader;

use rust_decimal::Decimal;
use signalbox_domain::{
    AcceptedInputId, ContextFrontierId, ReviewConfidence, ReviewExternalLink,
    ReviewExternalLinkAssociation, ReviewExternalLinkId, ReviewExternalObjectKind,
    ReviewExternalObjectState, ReviewFinding, ReviewFindingDiffSide, ReviewFindingEventKind,
    ReviewFindingId, ReviewFindingSeverity, ReviewFindingStatus, ReviewKey, ReviewPass,
    ReviewPassId, ReviewPassKind, ReviewPassRef, ReviewPassState, ReviewPassTurnOutcome,
    ReviewPolicy, ReviewPolicyVersion, ReviewReferencedFindingEvidence, ReviewRun, ReviewRunId,
    ReviewRunRef, ReviewRunState, ReviewTarget, ReviewTargetId, ReviewText, ReviewWorkflowKind,
    SessionId, TurnId,
};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow, types::Uuid};

/// PostgreSQL adapter for the review-workflow bounded context.
#[derive(Clone, Debug)]
pub struct ReviewWorkflowStore {
    pool: PgPool,
}

async fn commit_mutation(
    transaction: Transaction<'_, Postgres>,
) -> Result<(), ReviewWorkflowStoreError> {
    transaction
        .commit()
        .await
        .map_err(classify_mutating_commit_error)
}

pub(crate) fn classify_mutating_commit_error(error: sqlx::Error) -> ReviewWorkflowStoreError {
    if crate::commit_failure_is_ambiguous(&error) {
        ReviewWorkflowStoreError::CommitAmbiguous(error)
    } else {
        ReviewWorkflowStoreError::Database(error)
    }
}

fn require_joined_reference(
    row: &PgRow,
    column: &str,
    aggregate: &'static str,
    detail: &'static str,
) -> Result<(), ReviewWorkflowStoreError> {
    if row.try_get::<Option<Uuid>, _>(column)?.is_none() {
        return Err(corruption(aggregate, String::from(detail)));
    }
    Ok(())
}

/// Opens one read-only `REPEATABLE READ` transaction.
///
/// Every statement issued on the returned transaction observes the same
/// database snapshot, which is how a multi-statement read stays coherent
/// without excluding writers.
pub(crate) async fn begin_repeatable_read(
    pool: &PgPool,
) -> Result<Transaction<'_, Postgres>, ReviewWorkflowStoreError> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *transaction)
        .await?;
    Ok(transaction)
}

fn encode_run_state(state: ReviewRunState) -> (&'static str, Option<ReviewPassId>) {
    match state {
        ReviewRunState::Queued => ("queued", None),
        ReviewRunState::Running { active_pass } => ("running", Some(active_pass.pass())),
        ReviewRunState::Succeeded { concluding_pass } => {
            ("succeeded", Some(concluding_pass.pass()))
        }
        ReviewRunState::Failed { failed_pass } => ("failed", Some(failed_pass.pass())),
        ReviewRunState::Blocked { blocking_pass } => ("blocked", Some(blocking_pass.pass())),
        ReviewRunState::Cancelled { last_pass } => {
            ("cancelled", last_pass.map(ReviewPassRef::pass))
        }
    }
}

fn decode_run_state(
    run: ReviewRunRef,
    kind: &str,
    pass: Option<Uuid>,
) -> Result<ReviewRunState, ReviewWorkflowStoreError> {
    let pass = pass.map(|pass| ReviewPassRef::new(run, pass_id(pass)));
    match (kind, pass) {
        ("queued", None) => Ok(ReviewRunState::Queued),
        ("running", Some(active_pass)) => Ok(ReviewRunState::Running { active_pass }),
        ("succeeded", Some(concluding_pass)) => Ok(ReviewRunState::Succeeded { concluding_pass }),
        ("failed", Some(failed_pass)) => Ok(ReviewRunState::Failed { failed_pass }),
        ("blocked", Some(blocking_pass)) => Ok(ReviewRunState::Blocked { blocking_pass }),
        ("cancelled", last_pass) => Ok(ReviewRunState::Cancelled { last_pass }),
        _ => Err(corruption(
            "review_run",
            format!("invalid state shape {kind}"),
        )),
    }
}

struct EncodedLinkAssociation {
    kind: &'static str,
    run: Option<ReviewRunId>,
    finding: Option<ReviewFindingId>,
    finding_pass: Option<ReviewPassId>,
}

fn encode_link_association(association: ReviewExternalLinkAssociation) -> EncodedLinkAssociation {
    match association {
        ReviewExternalLinkAssociation::Target(_) => EncodedLinkAssociation {
            kind: "target",
            run: None,
            finding: None,
            finding_pass: None,
        },
        ReviewExternalLinkAssociation::Run(run) => EncodedLinkAssociation {
            kind: "run",
            run: Some(run.run()),
            finding: None,
            finding_pass: None,
        },
        ReviewExternalLinkAssociation::Finding(finding) => EncodedLinkAssociation {
            kind: "finding",
            run: Some(finding.run().run()),
            finding: Some(finding.finding()),
            finding_pass: Some(finding.pass().pass()),
        },
    }
}

struct EncodedFindingEvent<'a> {
    kind: &'static str,
    judge_confidence: Option<i16>,
    reason: Option<&'a str>,
    referenced: Option<ReviewReferencedFindingEvidence>,
    referenced_status: Option<ReviewFindingStatus>,
    external_link: Option<ReviewExternalLinkId>,
}

fn encode_finding_event(event: &ReviewFindingEventKind) -> EncodedFindingEvent<'_> {
    let empty = |kind| EncodedFindingEvent {
        kind,
        judge_confidence: None,
        reason: None,
        referenced: None,
        referenced_status: None,
        external_link: None,
    };
    match event {
        ReviewFindingEventKind::Accepted { confidence } => EncodedFindingEvent {
            judge_confidence: Some(i16::from(confidence.get())),
            ..empty("accepted")
        },
        ReviewFindingEventKind::Rejected { reason } => EncodedFindingEvent {
            kind: "rejected",
            judge_confidence: None,
            reason: Some(reason.as_str()),
            referenced: None,
            referenced_status: None,
            external_link: None,
        },
        ReviewFindingEventKind::Duplicate { canonical } => EncodedFindingEvent {
            kind: "duplicate",
            judge_confidence: None,
            reason: None,
            referenced: Some(*canonical),
            referenced_status: Some(canonical.status()),
            external_link: None,
        },
        ReviewFindingEventKind::Superseded { successor } => EncodedFindingEvent {
            kind: "superseded",
            judge_confidence: None,
            reason: None,
            referenced: Some(*successor),
            referenced_status: Some(successor.status()),
            external_link: None,
        },
        ReviewFindingEventKind::Stale => empty("stale"),
        ReviewFindingEventKind::Posted { link } => EncodedFindingEvent {
            kind: "posted",
            judge_confidence: None,
            reason: None,
            referenced: None,
            referenced_status: None,
            external_link: Some(link.link()),
        },
        ReviewFindingEventKind::Fixed => empty("fixed"),
        ReviewFindingEventKind::BlockedWithReason { reason, link } => EncodedFindingEvent {
            kind: "blocked_with_reason",
            judge_confidence: None,
            reason: Some(reason.as_str()),
            referenced: None,
            referenced_status: None,
            external_link: link.as_ref().map(|link| link.link()),
        },
    }
}

fn encode_finding_status(status: ReviewFindingStatus) -> &'static str {
    match status {
        ReviewFindingStatus::Open => "open",
        ReviewFindingStatus::Accepted => "accepted",
        ReviewFindingStatus::Rejected => "rejected",
        ReviewFindingStatus::Duplicate => "duplicate",
        ReviewFindingStatus::Superseded => "superseded",
        ReviewFindingStatus::Stale => "stale",
        ReviewFindingStatus::Posted => "posted",
        ReviewFindingStatus::Fixed => "fixed",
        ReviewFindingStatus::BlockedWithReason => "blocked_with_reason",
    }
}

fn decode_finding_status(status: &str) -> Result<ReviewFindingStatus, ReviewWorkflowStoreError> {
    match status {
        "open" => Ok(ReviewFindingStatus::Open),
        "accepted" => Ok(ReviewFindingStatus::Accepted),
        "rejected" => Ok(ReviewFindingStatus::Rejected),
        "duplicate" => Ok(ReviewFindingStatus::Duplicate),
        "superseded" => Ok(ReviewFindingStatus::Superseded),
        "stale" => Ok(ReviewFindingStatus::Stale),
        "posted" => Ok(ReviewFindingStatus::Posted),
        "fixed" => Ok(ReviewFindingStatus::Fixed),
        "blocked_with_reason" => Ok(ReviewFindingStatus::BlockedWithReason),
        other => Err(corruption(
            "review_finding_event",
            format!("unknown referenced-finding status {other}"),
        )),
    }
}

fn encode_workflow_kind(kind: ReviewWorkflowKind) -> &'static str {
    match kind {
        ReviewWorkflowKind::ImportExternalContext => "import_external_context",
        ReviewWorkflowKind::ReadOnlyReview => "read_only_review",
        ReviewWorkflowKind::JudgeFindings => "judge_findings",
        ReviewWorkflowKind::DedupeFindings => "dedupe_findings",
        ReviewWorkflowKind::PublishReview => "publish_review",
        ReviewWorkflowKind::FixFindings => "fix_findings",
        ReviewWorkflowKind::PropagateStack => "propagate_stack",
    }
}

fn decode_workflow_kind(kind: &str) -> Result<ReviewWorkflowKind, ReviewWorkflowStoreError> {
    match kind {
        "import_external_context" => Ok(ReviewWorkflowKind::ImportExternalContext),
        "read_only_review" => Ok(ReviewWorkflowKind::ReadOnlyReview),
        "judge_findings" => Ok(ReviewWorkflowKind::JudgeFindings),
        "dedupe_findings" => Ok(ReviewWorkflowKind::DedupeFindings),
        "publish_review" => Ok(ReviewWorkflowKind::PublishReview),
        "fix_findings" => Ok(ReviewWorkflowKind::FixFindings),
        "propagate_stack" => Ok(ReviewWorkflowKind::PropagateStack),
        _ => Err(corruption(
            "review_run",
            format!("unknown workflow kind {kind}"),
        )),
    }
}

fn encode_pass_kind(kind: ReviewPassKind) -> &'static str {
    match kind {
        ReviewPassKind::ImportExternalContext => "import_external_context",
        ReviewPassKind::ReadOnlyReview => "read_only_review",
        ReviewPassKind::Judge => "judge",
        ReviewPassKind::Dedupe => "dedupe",
        ReviewPassKind::Publish => "publish",
        ReviewPassKind::Fix => "fix",
        ReviewPassKind::PropagateStack => "propagate_stack",
    }
}

const fn workflow_matches_pass_kind(workflow: ReviewWorkflowKind, pass: ReviewPassKind) -> bool {
    matches!(
        (workflow, pass),
        (
            ReviewWorkflowKind::ImportExternalContext,
            ReviewPassKind::ImportExternalContext
        ) | (
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPassKind::ReadOnlyReview
        ) | (ReviewWorkflowKind::JudgeFindings, ReviewPassKind::Judge)
            | (ReviewWorkflowKind::DedupeFindings, ReviewPassKind::Dedupe)
            | (ReviewWorkflowKind::PublishReview, ReviewPassKind::Publish)
            | (ReviewWorkflowKind::FixFindings, ReviewPassKind::Fix)
            | (
                ReviewWorkflowKind::PropagateStack,
                ReviewPassKind::PropagateStack
            )
    )
}

fn decode_pass_kind(kind: &str) -> Result<ReviewPassKind, ReviewWorkflowStoreError> {
    match kind {
        "import_external_context" => Ok(ReviewPassKind::ImportExternalContext),
        "read_only_review" => Ok(ReviewPassKind::ReadOnlyReview),
        "judge" => Ok(ReviewPassKind::Judge),
        "dedupe" => Ok(ReviewPassKind::Dedupe),
        "publish" => Ok(ReviewPassKind::Publish),
        "fix" => Ok(ReviewPassKind::Fix),
        "propagate_stack" => Ok(ReviewPassKind::PropagateStack),
        _ => Err(corruption(
            "review_pass",
            format!("unknown pass kind {kind}"),
        )),
    }
}

fn encode_diff_side(side: ReviewFindingDiffSide) -> &'static str {
    match side {
        ReviewFindingDiffSide::Left => "left",
        ReviewFindingDiffSide::Right => "right",
    }
}

fn decode_diff_side(side: &str) -> Result<ReviewFindingDiffSide, ReviewWorkflowStoreError> {
    match side {
        "left" => Ok(ReviewFindingDiffSide::Left),
        "right" => Ok(ReviewFindingDiffSide::Right),
        _ => Err(corruption(
            "review_finding",
            format!("unknown diff side {side}"),
        )),
    }
}

fn encode_severity(severity: ReviewFindingSeverity) -> &'static str {
    match severity {
        ReviewFindingSeverity::Info => "info",
        ReviewFindingSeverity::Low => "low",
        ReviewFindingSeverity::Medium => "medium",
        ReviewFindingSeverity::High => "high",
        ReviewFindingSeverity::Critical => "critical",
    }
}

fn decode_severity(severity: &str) -> Result<ReviewFindingSeverity, ReviewWorkflowStoreError> {
    match severity {
        "info" => Ok(ReviewFindingSeverity::Info),
        "low" => Ok(ReviewFindingSeverity::Low),
        "medium" => Ok(ReviewFindingSeverity::Medium),
        "high" => Ok(ReviewFindingSeverity::High),
        "critical" => Ok(ReviewFindingSeverity::Critical),
        _ => Err(corruption(
            "review_finding",
            format!("unknown severity {severity}"),
        )),
    }
}

fn encode_external_object_kind(kind: ReviewExternalObjectKind) -> &'static str {
    match kind {
        ReviewExternalObjectKind::ChangeRequest => "change_request",
        ReviewExternalObjectKind::Commit => "commit",
        ReviewExternalObjectKind::Review => "review",
        ReviewExternalObjectKind::ReviewThread => "review_thread",
        ReviewExternalObjectKind::ReviewComment => "review_comment",
        ReviewExternalObjectKind::ChangeRequestComment => "change_request_comment",
    }
}

fn decode_external_object_kind(
    kind: &str,
) -> Result<ReviewExternalObjectKind, ReviewWorkflowStoreError> {
    match kind {
        "change_request" => Ok(ReviewExternalObjectKind::ChangeRequest),
        "commit" => Ok(ReviewExternalObjectKind::Commit),
        "review" => Ok(ReviewExternalObjectKind::Review),
        "review_thread" => Ok(ReviewExternalObjectKind::ReviewThread),
        "review_comment" => Ok(ReviewExternalObjectKind::ReviewComment),
        "change_request_comment" => Ok(ReviewExternalObjectKind::ChangeRequestComment),
        _ => Err(corruption(
            "review_external_link",
            format!("unknown object kind {kind}"),
        )),
    }
}

fn encode_external_object_state(state: ReviewExternalObjectState) -> &'static str {
    match state {
        ReviewExternalObjectState::Current => "current",
        ReviewExternalObjectState::Outdated => "outdated",
        ReviewExternalObjectState::Resolved => "resolved",
    }
}

fn decode_external_object_state(
    state: &str,
) -> Result<ReviewExternalObjectState, ReviewWorkflowStoreError> {
    match state {
        "current" => Ok(ReviewExternalObjectState::Current),
        "outdated" => Ok(ReviewExternalObjectState::Outdated),
        "resolved" => Ok(ReviewExternalObjectState::Resolved),
        _ => Err(corruption(
            "review_external_link_observation",
            format!("unknown state {state}"),
        )),
    }
}

fn review_key(
    value: String,
    aggregate: &'static str,
) -> Result<ReviewKey, ReviewWorkflowStoreError> {
    ReviewKey::try_new(value).map_err(|error| {
        corruption(
            aggregate,
            format!("invalid review key: {:?}", error.failure()),
        )
    })
}

fn review_text(
    value: String,
    aggregate: &'static str,
) -> Result<ReviewText, ReviewWorkflowStoreError> {
    ReviewText::try_new(value).map_err(|error| {
        corruption(
            aggregate,
            format!("invalid review text: {:?}", error.failure()),
        )
    })
}

fn decode_review_confidence(
    value: i32,
    aggregate: &'static str,
) -> Result<ReviewConfidence, ReviewWorkflowStoreError> {
    let value = u16::try_from(value)
        .map_err(|_| corruption(aggregate, format!("invalid confidence {value}")))?;
    ReviewConfidence::try_from_basis_points(value)
        .map_err(|error| corruption(aggregate, format!("{error:?}")))
}

fn decode_review_policy(
    row: &PgRow,
    version_column: &str,
    judge_column: &str,
    publication_column: &str,
    aggregate: &'static str,
) -> Result<ReviewPolicy, ReviewWorkflowStoreError> {
    let version = positive_u32(row.try_get(version_column)?, aggregate)?;
    let judge = decode_review_confidence(row.try_get(judge_column)?, aggregate)?;
    let publication = decode_review_confidence(row.try_get(publication_column)?, aggregate)?;
    ReviewPolicy::try_new(
        ReviewPolicyVersion::try_new(version)
            .map_err(|_| corruption(aggregate, String::from("zero policy version")))?,
        judge,
        publication,
    )
    .map_err(|error| corruption(aggregate, format!("{error:?}")))
}

fn positive_u32(value: i64, aggregate: &'static str) -> Result<u32, ReviewWorkflowStoreError> {
    let value = u32::try_from(value)
        .map_err(|_| corruption(aggregate, format!("invalid positive u32 {value}")))?;
    if value == 0 {
        Err(corruption(
            aggregate,
            String::from("zero where positive u32 required"),
        ))
    } else {
        Ok(value)
    }
}

fn decimal_u64(value: Decimal, aggregate: &'static str) -> Result<u64, ReviewWorkflowStoreError> {
    value
        .to_string()
        .parse()
        .map_err(|_| corruption(aggregate, format!("invalid u64 decimal {value}")))
}

pub(crate) fn target_id(value: Uuid) -> ReviewTargetId {
    ReviewTargetId::from_uuid(value)
}

pub(crate) fn run_id(value: Uuid) -> ReviewRunId {
    ReviewRunId::from_uuid(value)
}

pub(crate) fn pass_id(value: Uuid) -> ReviewPassId {
    ReviewPassId::from_uuid(value)
}

pub(crate) fn finding_id(value: Uuid) -> ReviewFindingId {
    ReviewFindingId::from_uuid(value)
}

pub(crate) fn external_link_id(value: Uuid) -> ReviewExternalLinkId {
    ReviewExternalLinkId::from_uuid(value)
}

fn session_id(value: Uuid) -> SessionId {
    SessionId::from_uuid(value)
}

fn accepted_input_id(value: Uuid) -> AcceptedInputId {
    AcceptedInputId::from_uuid(value)
}

fn turn_id(value: Uuid) -> TurnId {
    TurnId::from_uuid(value)
}

fn context_frontier_id(value: Uuid) -> ContextFrontierId {
    ContextFrontierId::from_uuid(value)
}

pub(crate) fn corruption(aggregate: &'static str, detail: String) -> ReviewWorkflowStoreError {
    ReviewWorkflowStoreError::Corruption(ReviewWorkflowCorruption { aggregate, detail })
}

/// The session and originating turn one accepted input records.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewAcceptedInputOrigin {
    session: SessionId,
    origin_turn: Option<TurnId>,
}

impl ReviewAcceptedInputOrigin {
    /// The session the input was accepted into.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// The turn the input originated, absent while it originated none.
    pub const fn origin_turn(&self) -> Option<TurnId> {
        self.origin_turn
    }
}

/// The lifecycle position one turn row records.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewTurnLifecycleState {
    /// Accepted, with no attempt started.
    Queued,
    /// Started, with no terminal disposition recorded.
    Active,
    /// Finished under the recorded disposition.
    Terminal(ReviewPassTurnOutcome),
}

/// One turn's durable lifecycle facts, as a review pass reads them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewTurnLifecycle {
    session: SessionId,
    accepted_input: Option<AcceptedInputId>,
    state: ReviewTurnLifecycleState,
    terminal_frontier: Option<ContextFrontierId>,
}

impl ReviewTurnLifecycle {
    /// The session the turn runs in.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// The input the turn originated from, absent for a delegated turn.
    pub const fn accepted_input(&self) -> Option<AcceptedInputId> {
        self.accepted_input
    }

    /// The lifecycle position the row records.
    pub const fn state(&self) -> ReviewTurnLifecycleState {
        self.state
    }

    /// The frontier the turn ended on, absent until it is terminal.
    pub const fn terminal_frontier(&self) -> Option<ContextFrontierId> {
        self.terminal_frontier
    }
}

/// First reservation outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReserveExternalLinkOutcome {
    /// This call inserted the pending reservation.
    Inserted(ReviewExternalLink),
    /// An equal reservation already existed and its complete state was loaded.
    Existing(ReviewExternalLink),
}

#[derive(signalbox_derive::Accessors, signalbox_derive::OperatorError)]
#[error("review external-link identity was reused for a different canonical reservation")]
/// Conflicting reuse of a review external-link reservation identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewExternalLinkReservationConflict {
    /// Borrows the retained canonical aggregate.
    #[get(unbox)]
    existing: Box<ReviewExternalLink>,
    /// Borrows the rejected reservation request.
    #[get(unbox)]
    requested: Box<ReviewExternalLink>,
}

impl ReviewExternalLinkReservationConflict {
    /// Returns both complete aggregates.
    pub fn into_parts(self) -> (ReviewExternalLink, ReviewExternalLink) {
        (*self.existing, *self.requested)
    }
}

#[derive(signalbox_derive::OperatorError)]
/// Caller-supplied aggregate shape that cannot begin a new store record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewWorkflowInsertionError {
    #[error("new review run is not queued: {state:?}")]
    /// A run insertion carried state that can only result from transition.
    RunNotQueued {
        /// Rejected current state.
        state: Box<ReviewRunState>,
    },
    #[error("new review pass is not queued: {state:?}")]
    /// A pass insertion carried state that can only result from transition.
    PassNotQueued {
        /// Rejected current state.
        state: Box<ReviewPassState>,
    },
    #[error("new review run and pass are not one coherent admission")]
    /// A paired run and pass do not describe one domain-coherent admission.
    RunPassMismatch,
    #[error("new review finding is not open: {status:?}")]
    /// A finding insertion already carried lifecycle history.
    FindingNotOpen {
        /// Rejected current status.
        status: ReviewFindingStatus,
    },
    #[error("new review external-link reservation is not pending")]
    /// A reservation insertion already carried post-effect evidence.
    ExternalLinkNotPending,
}

#[derive(signalbox_derive::OperatorError)]
/// Domain transition rejected before persistence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewWorkflowTransitionError {
    #[error("review-run transition rejected: {field_0:?}")]
    /// Run transition failed.
    Run(signalbox_domain::ReviewRunTransitionError),
    #[error("review-pass transition rejected: {field_0:?}")]
    /// Pass transition failed.
    Pass(signalbox_domain::ReviewPassTransitionError),
    #[error("review-finding transition rejected: {:?}", field_0.failure())]
    /// Finding event application failed.
    Finding(signalbox_domain::ReviewFindingTransitionError),
    #[error("review external-link transition rejected: {field_0:?}")]
    /// External-link attachment or observation failed.
    ExternalLink(signalbox_domain::ReviewExternalLinkTransitionError),
}

#[derive(signalbox_derive::Accessors, signalbox_derive::OperatorError)]
#[error("{} durable facts are corrupt: {}", aggregate, detail)]
/// Stored workflow facts could not form one domain aggregate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewWorkflowCorruption {
    aggregate: &'static str,
    /// Borrows the content-safe diagnostic detail.
    #[get(str)]
    detail: String,
}

impl ReviewWorkflowCorruption {
    /// Returns the aggregate family that failed.
    pub const fn aggregate(&self) -> &'static str {
        self.aggregate
    }
}

#[derive(signalbox_derive::OperatorError)]
/// Review-workflow persistence failure.
#[derive(Debug)]
pub enum ReviewWorkflowStoreError {
    #[error("review-workflow database failure: {field_0}")]
    /// PostgreSQL or transport failure.
    Database(#[source] sqlx::Error),
    #[error("review-workflow commit outcome is ambiguous: {field_0}")]
    /// PostgreSQL may have committed a mutation before the response was lost.
    CommitAmbiguous(#[source] sqlx::Error),
    #[error(transparent)]
    /// Stored facts failed closed reconstitution.
    Corruption(#[source] ReviewWorkflowCorruption),
    #[error(transparent)]
    /// A caller attempted to insert a post-transition aggregate as new.
    InvalidInsertion(#[source] ReviewWorkflowInsertionError),
    #[error(transparent)]
    /// A caller requested an invalid domain transition.
    InvalidTransition(#[source] ReviewWorkflowTransitionError),
    #[error("review pass results must bind in the same transaction as their exact effect")]
    /// A lifecycle-only transition attempted to persist an effect result.
    NonAtomicPassResult,
    #[error("produced findings must be admitted as one complete exact inventory")]
    /// A produced-finding write omitted or contradicted the exact inventory.
    IncompleteFindingInventory,
    #[error("blocked publication reservations require one atomic reconciliation effect")]
    /// A blocked publication reservation was not reconciled atomically.
    IncompletePublicationReconciliation,
    #[error(transparent)]
    /// An external-link identity was reused for another canonical payload.
    ReservationConflict(#[source] ReviewExternalLinkReservationConflict),
}

impl From<sqlx::Error> for ReviewWorkflowStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl ReviewWorkflowReader for ReviewWorkflowStore {
    type Error = ReviewWorkflowStoreError;

    async fn load_target(
        &self,
        target: ReviewTargetId,
    ) -> Result<Option<ReviewTarget>, Self::Error> {
        ReviewWorkflowStore::load_target(self, target).await
    }

    async fn load_run(&self, run: ReviewRunId) -> Result<Option<ReviewRun>, Self::Error> {
        ReviewWorkflowStore::load_run(self, run).await
    }

    async fn load_run_with_pass(
        &self,
        run: ReviewRunId,
    ) -> Result<Option<(ReviewRun, Option<ReviewPass>)>, Self::Error> {
        ReviewWorkflowStore::load_run_with_pass(self, run).await
    }

    async fn load_pass(&self, pass: ReviewPassId) -> Result<Option<ReviewPass>, Self::Error> {
        ReviewWorkflowStore::load_pass(self, pass).await
    }

    async fn load_finding(
        &self,
        finding: ReviewFindingId,
    ) -> Result<Option<ReviewFinding>, Self::Error> {
        ReviewWorkflowStore::load_finding(self, finding).await
    }

    async fn list_findings(&self, run: ReviewRunId) -> Result<Vec<ReviewFinding>, Self::Error> {
        ReviewWorkflowStore::list_findings(self, run).await
    }
}

#[cfg(test)]
mod tests {
    use super::{ReviewWorkflowStoreError, classify_mutating_commit_error};

    #[test]
    fn unknown_commit_transport_failure_is_ambiguous() {
        let classified = classify_mutating_commit_error(sqlx::Error::PoolClosed);
        assert!(matches!(
            classified,
            ReviewWorkflowStoreError::CommitAmbiguous(sqlx::Error::PoolClosed)
        ));
    }
}
