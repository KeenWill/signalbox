use base64::{Engine as _, engine::general_purpose::STANDARD as STANDARD_BASE64};
use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeSeq};

use crate::{
    CanonicalU64, CanonicalUuid, FrameValidationError, ServerMessage,
    deserialize_required_nullable, values_are_distinct,
};

/// Maximum UTF-8 bytes in one operator-status repository slug.
///
/// A slug is `owner/name`, and the provider admits 100 bytes on each side.
// numeric-bound: guard - the operator-status wire grammar advertises accepting repository slugs only to this length
pub const MAX_OPERATOR_STATUS_REPOSITORY_UTF8_BYTES: usize = 201;

/// Maximum UTF-8 bytes in one operator-status repository-watch rule identity.
// numeric-bound: guard - the operator-status wire grammar advertises accepting rule identities only to this length
pub const MAX_OPERATOR_STATUS_RULE_ID_UTF8_BYTES: usize = 128;

/// Maximum UTF-8 bytes in one operator-status branch name.
///
/// Covers a held slot's branch origin and a convergence row's base branch.
// numeric-bound: guard - the operator-status wire grammar advertises accepting branch names only to this length
pub const MAX_OPERATOR_STATUS_BRANCH_UTF8_BYTES: usize = 255;

/// Maximum sessions named by one operator-status dispatch inventory.
///
/// Bounds both a held slot's own sessions and the sessions occupying a queued
/// obligation, which name the same dispatch-action inventory.
// numeric-bound: guard - protects decoded frame memory from a runaway dispatch session fan-out
pub const MAX_OPERATOR_STATUS_DISPATCH_SESSIONS: usize = 32;

/// Maximum independently failing release clauses on one held slot.
// numeric-bound: not-a-bound - the closed blocker enum's exact variant count, which one slot cannot repeat
pub const MAX_OPERATOR_STATUS_HELD_SLOT_BLOCKERS: usize = 4;

/// Maximum unresolved review threads counted by one convergence assessment.
// numeric-bound: guard - refuses a thread count no durable assessment can have produced
pub const MAX_OPERATOR_STATUS_UNRESOLVED_THREADS: u64 = 10_000;

/// Maximum gating checks counted by one convergence assessment.
///
/// Persistence admits the same inventory, so a divergence here would reject an
/// otherwise valid projection and fail the whole snapshot.
// numeric-bound: guard - bounds the non-green check names one convergence frame can carry
pub const MAX_OPERATOR_STATUS_GATING_CHECKS: u64 = 10_000;

/// Maximum UTF-8 bytes in one operator-status gating-check name.
// numeric-bound: guard - the operator-status wire grammar advertises accepting check names only to this length
pub const MAX_OPERATOR_STATUS_CHECK_NAME_UTF8_BYTES: usize = 256;

/// Maximum UTF-8 bytes in one operator-status review node identity.
// numeric-bound: guard - the operator-status wire grammar advertises accepting review node identities only to this length
pub const MAX_OPERATOR_STATUS_REVIEW_NODE_ID_UTF8_BYTES: usize = 256;

/// Maximum UTF-8 bytes in one operator-status reviewer login.
// numeric-bound: guard - the operator-status wire grammar advertises accepting reviewer logins only to this length
pub const MAX_OPERATOR_STATUS_REVIEWER_UTF8_BYTES: usize = 44;

/// Maximum UTF-8 bytes in one operator-status reviewer login's base, the
/// spelling left once the optional App-bot suffix is set aside.
// numeric-bound: guard - the operator-status wire grammar advertises accepting a login base only to this length
pub const MAX_OPERATOR_STATUS_REVIEWER_BASE_UTF8_BYTES: usize = 39;

/// Literal suffix an App-bot reviewer login carries after its base.
pub const OPERATOR_STATUS_BOT_LOGIN_SUFFIX: &str = "[bot]";

/// The one base branch a merge-ready convergence verdict is settled against.
///
/// The durable assessment keys both converged verdicts to this spelling: a
/// merge-ready row's base branch is exactly this branch, and an
/// internally-converged row's base branch is any other.
pub const OPERATOR_STATUS_TRUNK_BASE_BRANCH: &str = "main";

/// Exact hexadecimal characters in one operator-status commit revision.
// numeric-bound: not-a-bound - the fixed width of a git SHA-1 object name
pub const OPERATOR_STATUS_COMMIT_SHA_LENGTH: usize = 40;

/// Singleton key class shown by repository-watch operator status.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorStatusSingletonScope {
    PullRequest,
    Stack,
    Rule,
    Repo,
}

/// One independently failing held-slot release clause.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorStatusHeldSlotBlocker {
    UndeliveredAction,
    DeliveryTurnRuntimeRelevant,
    LiveRuntimeTurn,
    PursuingGoal,
}

/// Current provider mergeability shown by repository-watch operator status.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorStatusMergeableState {
    Mergeable,
    Conflicting,
    Unknown,
}

/// Current provider review decision shown by repository-watch operator status.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorStatusReviewDecision {
    None,
    Approved,
    ReviewRequired,
    ChangesRequested,
}

/// Latest repository-watch convergence verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorStatusConvergenceVerdict {
    NotConverged,
    InternallyConverged,
    MergeReady,
}

/// Durable convergence seal attached to the latest assessment, when any.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorStatusConvergenceSeal {
    InternallyConverged,
    MergeReady,
}

/// Origin fact whose dispatch holds one repository-watch singleton slot.
///
/// A rule matching branch workflow-run completion under `Rule` or `Repo`
/// singleton scope holds a slot from a branch fact, which names no pull
/// request; every other admitted origin names one. The two are exclusive, so
/// the shape is a tagged choice rather than a pair of nullable numbers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperatorStatusHeldSlotOrigin {
    /// A pull-request fact, named by its number.
    PullRequest { pull_request_number: CanonicalU64 },
    /// A branch workflow-run fact, named by its branch.
    Branch { branch: String },
}

/// Payload for one active repository-watch dispatch slot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorStatusHeldSlotMessage {
    pub dispatch_id: CanonicalUuid,
    pub repository: String,
    pub origin: OperatorStatusHeldSlotOrigin,
    pub rule_id: String,
    pub rule_version: CanonicalU64,
    pub singleton_scope: OperatorStatusSingletonScope,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub singleton_repository: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub singleton_pull_request_number: Option<CanonicalU64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub singleton_stack_root_pull_request_number: Option<CanonicalU64>,
    pub held_for_seconds: CanonicalU64,
    pub session_ids: Vec<CanonicalUuid>,
    pub blockers: Vec<OperatorStatusHeldSlotBlocker>,
}

/// Payload for one owed repository-watch dispatch waiting for admission.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorStatusQueuedObligationMessage {
    pub obligation_id: CanonicalUuid,
    pub repository: String,
    pub rule_id: String,
    pub rule_version: CanonicalU64,
    pub singleton_scope: OperatorStatusSingletonScope,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub singleton_repository: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub singleton_pull_request_number: Option<CanonicalU64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub singleton_stack_root_pull_request_number: Option<CanonicalU64>,
    pub first_event_id: CanonicalUuid,
    pub latest_event_id: CanonicalUuid,
    pub matched_event_count: CanonicalU64,
    pub waiting_for_seconds: CanonicalU64,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub occupying_dispatch_id: Option<CanonicalUuid>,
    pub occupying_session_ids: Vec<CanonicalUuid>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub cooldown_remaining_seconds: Option<CanonicalU64>,
    pub cooldown_never_eligible: bool,
    pub ready: bool,
}

/// Payload for one latest pull-request convergence assessment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorStatusPullRequestConvergenceMessage {
    pub repository: String,
    pub pull_request_number: CanonicalU64,
    pub head_sha: String,
    pub base_branch: String,
    pub base_revision: String,
    pub mergeable_state: OperatorStatusMergeableState,
    pub review_decision: OperatorStatusReviewDecision,
    pub unresolved_thread_count: CanonicalU64,
    pub gating_check_count: CanonicalU64,
    #[serde(
        serialize_with = "serialize_operator_status_check_names",
        deserialize_with = "deserialize_operator_status_check_names"
    )]
    pub non_green_gating_checks: Vec<String>,
    pub verdict: OperatorStatusConvergenceVerdict,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub seal: Option<OperatorStatusConvergenceSeal>,
    pub assessed_seconds_ago: CanonicalU64,
}

/// Payload for one stale blocking review whose planned clearance is unsettled.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorStatusPendingStaleReviewClearanceMessage {
    pub repository: String,
    pub pull_request_number: CanonicalU64,
    pub current_head_sha: String,
    pub review_node_id: String,
    pub reviewer: String,
    pub reviewed_head_sha: String,
    pub pending_for_seconds: CanonicalU64,
}

/// One non-terminal session state a deadline violation can be reported under.
///
/// `terminal` is absent by construction: a terminal session owes no deadline,
/// so a violation naming one would contradict the invariant it reports on.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorStatusLifecycleState {
    Created,
    Dispatched,
    Active,
    Waiting,
    Recovering,
    Blocked,
    Parked,
}

/// One calendar week's session-lifecycle metrics.
///
/// Every rate travels as its exact numerator and denominator rather than as a
/// ratio, so a week with an empty population reports no rate at all instead of
/// a zero the durable columns do not claim, and a reader compares exact counts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorStatusLifecycleWeekMessage {
    /// The UTC start of the calendar week, as an ISO-8601 calendar date.
    pub week_start_date: String,
    /// Sessions counted as completion failures.
    pub completion_failure_numerator: CanonicalU64,
    /// The trimmed weekly terminal cohort the headline is over.
    pub completion_failure_denominator: CanonicalU64,
    /// `failed_unknown` closures inside that numerator.
    pub failed_unknown_count: CanonicalU64,
    /// Sessions recording context-headroom exhaustion on any turn.
    pub overflow_numerator: CanonicalU64,
    /// The untrimmed weekly terminal cohort, before the stopped and
    /// superseded trim.
    pub overflow_denominator: CanonicalU64,
    /// Overflow sessions whose outcome was `achieved_verified`.
    pub finish_given_overflow_numerator: CanonicalU64,
    /// Dispatch-cohort sessions recording a compaction wall.
    pub wall_numerator: CanonicalU64,
    /// The week's dispatch cohort.
    pub wall_denominator: CanonicalU64,
    /// Walls recorded in this week, whatever cohort they belong to.
    pub wall_occurrence_count: CanonicalU64,
    /// Terminal turns carrying a cause outside the catch-all set.
    pub classified_terminal_turn_count: CanonicalU64,
    /// Terminal turns recorded in this week.
    pub terminal_turn_count: CanonicalU64,
    /// `known_failed` calls carrying a cause outside the catch-all set.
    pub classified_known_failed_call_count: CanonicalU64,
    /// `known_failed` model calls recorded in this week.
    pub known_failed_call_count: CanonicalU64,
}

/// One owned non-terminal session violating the armed-deadline invariant.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorStatusLifecycleDeadlineViolationMessage {
    pub session_id: CanonicalUuid,
    pub state: OperatorStatusLifecycleState,
    /// Whether the session holds no armed deadline record at all.
    pub deadline_missing: bool,
    /// How long the armed expiry has been past, absent for a missing record.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub expired_for_seconds: Option<CanonicalU64>,
}

/// Terminal counts for one coherent repository-watch operator-status snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorStatusEndMessage {
    pub held_slot_count: CanonicalU64,
    pub queued_obligation_count: CanonicalU64,
    pub pull_request_convergence_count: CanonicalU64,
    pub pending_stale_review_clearance_count: CanonicalU64,
    pub lifecycle_week_count: CanonicalU64,
    /// The `nonterminal_past_deadline` alarm value, target zero.
    pub lifecycle_deadline_violation_count: CanonicalU64,
}

/// One member of a coherent repository-watch operator-status snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperatorStatusMessage {
    /// Begins the snapshot.
    Start {},
    /// One active repository-watch dispatch slot.
    HeldSlot(Box<OperatorStatusHeldSlotMessage>),
    /// One owed repository-watch dispatch waiting for admission.
    QueuedObligation(Box<OperatorStatusQueuedObligationMessage>),
    /// One latest pull-request convergence assessment.
    PullRequestConvergence(Box<OperatorStatusPullRequestConvergenceMessage>),
    /// One stale blocking review whose planned clearance is not yet settled.
    PendingStaleReviewClearance(Box<OperatorStatusPendingStaleReviewClearanceMessage>),
    /// One calendar week of session-lifecycle metrics.
    LifecycleWeek(Box<OperatorStatusLifecycleWeekMessage>),
    /// One owned non-terminal session past its armed-deadline obligation.
    LifecycleDeadlineViolation(Box<OperatorStatusLifecycleDeadlineViolationMessage>),
    /// Completes the snapshot with its section counts.
    End(Box<OperatorStatusEndMessage>),
}

pub(crate) fn validate_operator_status_message(
    message: &ServerMessage,
) -> Result<(), FrameValidationError> {
    let ServerMessage::OperatorStatus(message) = message else {
        return Ok(());
    };
    let valid = match message.as_ref() {
        OperatorStatusMessage::HeldSlot(item) => {
            operator_status_repository_is_valid(&item.repository)
                && operator_status_held_slot_origin_is_valid(&item.origin, item.singleton_scope)
                && operator_status_held_slot_origin_matches_singleton(
                    &item.origin,
                    item.singleton_scope,
                    item.singleton_pull_request_number,
                )
                && operator_status_rule_id_is_valid(&item.rule_id)
                && item.rule_version.value() > 0
                && operator_status_singleton_is_valid(
                    &item.repository,
                    &OperatorStatusSingletonAxes {
                        scope: item.singleton_scope,
                        repository: item.singleton_repository.as_deref(),
                        pull_request_number: item.singleton_pull_request_number,
                        stack_root_pull_request_number: item
                            .singleton_stack_root_pull_request_number,
                    },
                )
                && (1..=MAX_OPERATOR_STATUS_DISPATCH_SESSIONS).contains(&item.session_ids.len())
                && values_are_distinct(&item.session_ids)
                && item.blockers.len() <= MAX_OPERATOR_STATUS_HELD_SLOT_BLOCKERS
                && item.blockers.windows(2).all(|pair| {
                    operator_status_blocker_rank(pair[0]) < operator_status_blocker_rank(pair[1])
                })
        }
        OperatorStatusMessage::QueuedObligation(item) => {
            // A blocking occupant is either a watch dispatch, which names its
            // identity and its whole admitted session inventory, or one
            // independently commissioned live session, which names that single
            // session and no dispatch. Both a dispatch identity owning no
            // sessions and a dispatch-less occupant naming more than the one
            // session the obligation retains contradict the projection.
            let occupancy_is_valid = match item.occupying_dispatch_id {
                Some(_) => (1..=MAX_OPERATOR_STATUS_DISPATCH_SESSIONS)
                    .contains(&item.occupying_session_ids.len()),
                None => item.occupying_session_ids.len() <= 1,
            };
            let is_occupied =
                item.occupying_dispatch_id.is_some() || !item.occupying_session_ids.is_empty();
            operator_status_repository_is_valid(&item.repository)
                && operator_status_rule_id_is_valid(&item.rule_id)
                && item.rule_version.value() > 0
                && operator_status_singleton_is_valid(
                    &item.repository,
                    &OperatorStatusSingletonAxes {
                        scope: item.singleton_scope,
                        repository: item.singleton_repository.as_deref(),
                        pull_request_number: item.singleton_pull_request_number,
                        stack_root_pull_request_number: item
                            .singleton_stack_root_pull_request_number,
                    },
                )
                && operator_status_obligation_lineage_is_coherent(item)
                && values_are_distinct(&item.occupying_session_ids)
                && occupancy_is_valid
                // The projection reports a remaining cooldown only while the
                // eligibility instant is still ahead of the read, and rounds
                // that strictly positive interval up, so the smallest value it
                // can carry is one second. A zero would name a cooldown that
                // has already lapsed while still claiming to withhold the
                // obligation.
                && item
                    .cooldown_remaining_seconds
                    .is_none_or(|remaining| remaining.value() > 0)
                && !(item.cooldown_remaining_seconds.is_some() && item.cooldown_never_eligible)
                && !(item.ready
                    && (is_occupied
                        || item.cooldown_remaining_seconds.is_some()
                        || item.cooldown_never_eligible))
        }
        OperatorStatusMessage::PullRequestConvergence(item) => {
            operator_status_repository_is_valid(&item.repository)
                && item.pull_request_number.value() > 0
                && operator_status_sha_is_valid(&item.head_sha)
                && operator_status_branch_is_valid(&item.base_branch)
                && operator_status_sha_is_valid(&item.base_revision)
                && item.unresolved_thread_count.value() <= MAX_OPERATOR_STATUS_UNRESOLVED_THREADS
                && item.gating_check_count.value() <= MAX_OPERATOR_STATUS_GATING_CHECKS
                && u64::try_from(item.non_green_gating_checks.len())
                    .is_ok_and(|count| count <= item.gating_check_count.value())
                && item.non_green_gating_checks.iter().all(|name| {
                    operator_status_text_is_valid(name, MAX_OPERATOR_STATUS_CHECK_NAME_UTF8_BYTES)
                })
                && item
                    .non_green_gating_checks
                    .windows(2)
                    .all(|pair| pair[0] <= pair[1])
                && operator_status_convergence_verdict_matches_evidence(item)
                && operator_status_convergence_base_branch_matches_verdict(item)
        }
        OperatorStatusMessage::PendingStaleReviewClearance(item) => {
            operator_status_repository_is_valid(&item.repository)
                && item.pull_request_number.value() > 0
                && operator_status_sha_is_valid(&item.current_head_sha)
                && operator_status_text_is_valid(
                    &item.review_node_id,
                    MAX_OPERATOR_STATUS_REVIEW_NODE_ID_UTF8_BYTES,
                )
                && operator_status_reviewer_is_valid(&item.reviewer)
                && operator_status_sha_is_valid(&item.reviewed_head_sha)
                && item.current_head_sha != item.reviewed_head_sha
        }
        OperatorStatusMessage::LifecycleWeek(item) => {
            // Every pair is a rate, so no numerator may exceed its own
            // denominator; the trim only removes members, so the headline's
            // denominator cannot exceed the untrimmed cohort the overflow rate
            // is over; and `failed_unknown` is one arm of the headline's
            // numerator rather than a count beside it.
            operator_status_calendar_date_is_valid(&item.week_start_date)
                && item.completion_failure_numerator.value()
                    <= item.completion_failure_denominator.value()
                && item.failed_unknown_count.value() <= item.completion_failure_numerator.value()
                && item.completion_failure_denominator.value() <= item.overflow_denominator.value()
                && item.overflow_numerator.value() <= item.overflow_denominator.value()
                && item.finish_given_overflow_numerator.value() <= item.overflow_numerator.value()
                && item.wall_numerator.value() <= item.wall_denominator.value()
                && item.classified_terminal_turn_count.value() <= item.terminal_turn_count.value()
                && item.classified_known_failed_call_count.value()
                    <= item.known_failed_call_count.value()
        }
        OperatorStatusMessage::LifecycleDeadlineViolation(item) => {
            // The two report the one fact together: a session with no armed
            // record has no expiry to be past, and a session whose expiry is
            // past has a record.
            item.deadline_missing == item.expired_for_seconds.is_none()
        }
        OperatorStatusMessage::Start {} | OperatorStatusMessage::End(_) => true,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::OperatorStatusShape)
    }
}

/// Accepts exactly a real `YYYY-MM-DD` calendar date.
///
/// A week label is what a reader groups by, and `2026-99-99` has the shape
/// without being a day.
pub(crate) fn operator_status_calendar_date_is_valid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    // Integer parsing accepts a leading sign, so `+026-08-31` has the width
    // without having the shape.
    if !bytes
        .iter()
        .enumerate()
        .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
    {
        return false;
    }
    let Some(Ok(year)) = value.get(0..4).map(str::parse::<i64>) else {
        return false;
    };
    let Some(Ok(month)) = value.get(5..7).map(str::parse::<u32>) else {
        return false;
    };
    let Some(Ok(day)) = value.get(8..10).map(str::parse::<u32>) else {
        return false;
    };
    (1..=12).contains(&month) && day >= 1 && day <= operator_status_days_in_month(year, month)
}

/// Returns how many days one month of one year has.
const fn operator_status_days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

fn operator_status_held_slot_origin_is_valid(
    origin: &OperatorStatusHeldSlotOrigin,
    singleton_scope: OperatorStatusSingletonScope,
) -> bool {
    match origin {
        OperatorStatusHeldSlotOrigin::PullRequest {
            pull_request_number,
        } => pull_request_number.value() > 0,
        // A branch workflow-run completion names no pull request, so the
        // singleton it takes can only be keyed by the rule or the repository.
        // A pull-request- or stack-scoped hold would have to name a pull
        // request the branch fact never carried, so the two fields are only
        // separately admissible and must be validated together.
        OperatorStatusHeldSlotOrigin::Branch { branch } => {
            operator_status_branch_is_valid(branch)
                && matches!(
                    singleton_scope,
                    OperatorStatusSingletonScope::Rule | OperatorStatusSingletonScope::Repo
                )
        }
    }
}

/// Holds the held-slot projection's own identity on the wire.
///
/// The durable projection joins each dispatch batch to the very
/// `repo_watch_event` row it was admitted from, reads the origin pull request
/// from that row, and carries the batch's singleton beside it. That singleton
/// was keyed from the same event, so a pull-request-scoped hold names the very
/// pull request its origin names; the two can never diverge in a row
/// persistence produced.
///
/// A stack-scoped hold carries no such equality. Its singleton names the root
/// of the open pull-request component the origin belongs to, which is a
/// different pull request whenever the origin is not itself that root, so the
/// stack axis is left to the scope shape alone. A branch origin never reaches
/// either pull-request scope, which the adjacent origin validator settles.
fn operator_status_held_slot_origin_matches_singleton(
    origin: &OperatorStatusHeldSlotOrigin,
    singleton_scope: OperatorStatusSingletonScope,
    singleton_pull_request_number: Option<CanonicalU64>,
) -> bool {
    match (origin, singleton_scope) {
        (
            OperatorStatusHeldSlotOrigin::PullRequest {
                pull_request_number,
            },
            OperatorStatusSingletonScope::PullRequest,
        ) => singleton_pull_request_number == Some(*pull_request_number),
        _ => true,
    }
}

/// Holds the durable obligation lineage on the wire.
///
/// Persistence opens an obligation naming one evaluated event as both its first
/// and its latest, with a matched count of one. Every later coalesced
/// evaluation replaces the latest event with a distinct one and increments the
/// count, and an event is evaluated at most once per rule version, so the count
/// stands at one exactly while the two endpoints are the same event. A count of
/// one across differing endpoints, or a larger count across identical ones,
/// names a lineage no obligation row can hold.
fn operator_status_obligation_lineage_is_coherent(
    item: &OperatorStatusQueuedObligationMessage,
) -> bool {
    item.matched_event_count.value() > 0
        && (item.matched_event_count.value() == 1) == (item.first_event_id == item.latest_event_id)
}

/// Holds the durable
/// `repo_watch_convergence_verdict_matches_evidence` constraint on the wire.
/// The stored assessment settles on the unconverged verdict exactly when the
/// pull request carries at least one blocker, so either converged verdict
/// contradicts every blocker the row carries beside it. Exactly one durable
/// disjunct — the unsettled provider snapshot — is not carried on this wire, so
/// the implication is only enforced in the direction the frame can prove: an
/// unconverged verdict stays admissible against wholly clean carried evidence,
/// while a converged verdict requires each carried condition to be clean.
fn operator_status_convergence_verdict_matches_evidence(
    item: &OperatorStatusPullRequestConvergenceMessage,
) -> bool {
    match item.verdict {
        OperatorStatusConvergenceVerdict::NotConverged => true,
        OperatorStatusConvergenceVerdict::InternallyConverged
        | OperatorStatusConvergenceVerdict::MergeReady => {
            item.unresolved_thread_count.value() == 0
                && item.non_green_gating_checks.is_empty()
                && item.mergeable_state == OperatorStatusMergeableState::Mergeable
                && item.gating_check_count.value() > 0
                && item.review_decision != OperatorStatusReviewDecision::ChangesRequested
        }
    }
}

/// Holds the durable base-branch pair on the wire.
///
/// Two constraints sit beside the evidence constraint on the same assessment
/// row, and the status projection reads the verdict and the base branch from
/// that one row: a merge-ready verdict is settled only against `main`, and an
/// internally-converged verdict only against a branch that is not `main`. The
/// pair is what distinguishes the two converged verdicts, so a merge-ready row
/// on a release branch or an internally-converged row on the trunk names an
/// assessment persistence cannot hold.
///
/// The unconverged verdict carries no base-branch constraint, and neither does
/// the seal beside it: a seal is retained from the assessment that earned it
/// and outlives later ones, so a pull request retargeted after it was sealed
/// carries that seal beside its new base branch.
fn operator_status_convergence_base_branch_matches_verdict(
    item: &OperatorStatusPullRequestConvergenceMessage,
) -> bool {
    match item.verdict {
        OperatorStatusConvergenceVerdict::NotConverged => true,
        OperatorStatusConvergenceVerdict::MergeReady => {
            item.base_branch == OPERATOR_STATUS_TRUNK_BASE_BRANCH
        }
        OperatorStatusConvergenceVerdict::InternallyConverged => {
            item.base_branch != OPERATOR_STATUS_TRUNK_BASE_BRANCH
        }
    }
}

/// The singleton axes of one operator-status row, each named at its call site.
///
/// The two numeric axes carry one type and mean different things, so they are
/// supplied by name rather than by position: a pull-request number transposed
/// with a stack-root pull-request number would otherwise compile silently and
/// admit rows the singleton grammar refuses.
struct OperatorStatusSingletonAxes<'a> {
    scope: OperatorStatusSingletonScope,
    repository: Option<&'a str>,
    pull_request_number: Option<CanonicalU64>,
    stack_root_pull_request_number: Option<CanonicalU64>,
}

/// Holds the singleton axes of one row against the row's own identity.
///
/// Every repository-keyed singleton is keyed from the repository of the very
/// event whose row carries it, and an obligation coalesces only across events
/// sharing its singleton key, so a carried singleton repository is that row's
/// own repository rather than an independent slug. The row's repository is
/// checked against the slug grammar by the caller, so the equality carries that
/// grammar onto the singleton axis with it.
fn operator_status_singleton_is_valid(
    row_repository: &str,
    axes: &OperatorStatusSingletonAxes<'_>,
) -> bool {
    let OperatorStatusSingletonAxes {
        scope,
        repository,
        pull_request_number,
        stack_root_pull_request_number,
    } = axes;
    let repository_is_valid = repository.is_none_or(|value| value == row_repository);
    repository_is_valid
        && match scope {
            OperatorStatusSingletonScope::PullRequest => {
                repository.is_some()
                    && pull_request_number.is_some_and(|value| value.value() > 0)
                    && stack_root_pull_request_number.is_none()
            }
            OperatorStatusSingletonScope::Stack => {
                repository.is_some()
                    && pull_request_number.is_none()
                    && stack_root_pull_request_number.is_some_and(|value| value.value() > 0)
            }
            OperatorStatusSingletonScope::Rule => {
                repository.is_none()
                    && pull_request_number.is_none()
                    && stack_root_pull_request_number.is_none()
            }
            OperatorStatusSingletonScope::Repo => {
                repository.is_some()
                    && pull_request_number.is_none()
                    && stack_root_pull_request_number.is_none()
            }
        }
}

fn operator_status_text_is_valid(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && !value.contains('\0')
}

/// Holds the repository-slug grammar on the wire.
///
/// Mirrors the `RepositorySlug` constructor and the durable
/// `repo_watch_repository_is_valid` check: exactly one separator, each segment
/// nonempty and neither `.` nor `..`, and every byte an ASCII letter, digit,
/// hyphen, underscore, or dot. The constructor lowercases what it admits and
/// the durable check refuses anything else, so only the normalized spelling
/// ever reaches this wire and an uppercase byte is refused with the rest.
fn operator_status_repository_is_valid(value: &str) -> bool {
    let mut segments = value.split('/');
    let namespace = segments.next().unwrap_or_default();
    let name = segments.next().unwrap_or_default();
    operator_status_text_is_valid(value, MAX_OPERATOR_STATUS_REPOSITORY_UTF8_BYTES)
        && segments.next().is_none()
        && operator_status_repository_segment_is_valid(namespace)
        && operator_status_repository_segment_is_valid(name)
}

/// Holds one side of a repository slug.
fn operator_status_repository_segment_is_valid(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

/// Holds the rule-identity grammar on the wire.
///
/// Mirrors the `RepoWatchRuleId` constructor and the durable
/// `repo_watch_rule_id_is_valid` check: every byte an ASCII letter, digit,
/// hyphen, underscore, or dot. Unlike the slug and the login, a rule identity
/// is the operator's own spelling and is never case-normalized, so both cases
/// are admitted.
fn operator_status_rule_id_is_valid(value: &str) -> bool {
    operator_status_text_is_valid(value, MAX_OPERATOR_STATUS_RULE_ID_UTF8_BYTES)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Holds the branch-name grammar on the wire.
///
/// Mirrors the `BranchName` constructor and the durable
/// `repo_watch_branch_is_valid` check, which are the same git ref-name rules:
/// the name is not `@`, does not begin with a hyphen, does not end with a dot,
/// carries neither `..` nor `@{`, carries no space, control byte, delete byte,
/// or one of `~^:?*[\`, and every slash-separated component is nonempty, does
/// not begin with a dot, and does not end with `.lock`. Both producers store
/// the name without its `refs/heads/` prefix, so the prefix is not stripped
/// again here.
fn operator_status_branch_is_valid(value: &str) -> bool {
    operator_status_text_is_valid(value, MAX_OPERATOR_STATUS_BRANCH_UTF8_BYTES)
        && value != "@"
        && !value.starts_with('-')
        && !value.ends_with('.')
        && !value.contains("..")
        && !value.contains("@{")
        && !value.bytes().any(|byte| {
            byte <= 0x20
                || byte == 0x7f
                || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
        && value
            .split('/')
            .all(operator_status_branch_component_is_valid)
}

/// Holds one slash-separated component of a branch name.
fn operator_status_branch_component_is_valid(value: &str) -> bool {
    !value.is_empty() && !value.starts_with('.') && !value.ends_with(".lock")
}

/// Holds the reviewer-login grammar on the wire.
///
/// Mirrors the `RepoWatchAuthorLogin` constructor and the durable
/// `repo_watch_login_is_valid` check: an optional literal App-bot suffix is set
/// aside, and the base left behind is nonempty, no wider than its own ceiling,
/// begins and ends with something other than a hyphen, carries no doubled
/// hyphen, and spells itself in ASCII lowercase letters, digits, hyphens, and
/// underscores. Both producers lowercase what they admit, so only the
/// normalized spelling reaches this wire.
fn operator_status_reviewer_is_valid(value: &str) -> bool {
    let base = value
        .strip_suffix(OPERATOR_STATUS_BOT_LOGIN_SUFFIX)
        .unwrap_or(value);
    operator_status_text_is_valid(value, MAX_OPERATOR_STATUS_REVIEWER_UTF8_BYTES)
        && !base.is_empty()
        && base.len() <= MAX_OPERATOR_STATUS_REVIEWER_BASE_UTF8_BYTES
        && !base.starts_with('-')
        && !base.ends_with('-')
        && !base.contains("--")
        && base.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

fn serialize_operator_status_check_names<SerializerT>(
    names: &[String],
    serializer: SerializerT,
) -> Result<SerializerT::Ok, SerializerT::Error>
where
    SerializerT: Serializer,
{
    let mut sequence = serializer.serialize_seq(Some(names.len()))?;
    for name in names {
        sequence.serialize_element(&STANDARD_BASE64.encode(name.as_bytes()))?;
    }
    sequence.end()
}

fn deserialize_operator_status_check_names<'de, DeserializerT>(
    deserializer: DeserializerT,
) -> Result<Vec<String>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer)?
        .into_iter()
        .map(|encoded| {
            let decoded = STANDARD_BASE64.decode(encoded.as_bytes()).map_err(|_| {
                serde::de::Error::custom("operator-status check name is not canonical base64")
            })?;
            if STANDARD_BASE64.encode(&decoded) != encoded {
                return Err(serde::de::Error::custom(
                    "operator-status check name is not canonical base64",
                ));
            }
            String::from_utf8(decoded)
                .map_err(|_| serde::de::Error::custom("operator-status check name is not UTF-8"))
        })
        .collect()
}

fn operator_status_sha_is_valid(value: &str) -> bool {
    value.len() == OPERATOR_STATUS_COMMIT_SHA_LENGTH
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn operator_status_blocker_rank(blocker: OperatorStatusHeldSlotBlocker) -> u8 {
    match blocker {
        OperatorStatusHeldSlotBlocker::UndeliveredAction => 0,
        OperatorStatusHeldSlotBlocker::DeliveryTurnRuntimeRelevant => 1,
        OperatorStatusHeldSlotBlocker::LiveRuntimeTurn => 2,
        OperatorStatusHeldSlotBlocker::PursuingGoal => 3,
    }
}
