use serde::{Deserialize, Serialize};

use crate::{
    CanonicalU64, CanonicalUuid, FrameValidationError, ServerMessage, deserialize_required_nullable,
};

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

/// Terminal counts for one coherent operator-status snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorStatusEndMessage {
    pub repository_ingestion_count: CanonicalU64,
    pub lifecycle_week_count: CanonicalU64,
    /// The `nonterminal_past_deadline` alarm value, target zero.
    pub lifecycle_deadline_violation_count: CanonicalU64,
}

/// One member of a coherent operator-status snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperatorStatusMessage {
    /// Begins the snapshot.
    Start {},
    /// One calendar week of session-lifecycle metrics.
    LifecycleWeek(Box<OperatorStatusLifecycleWeekMessage>),
    /// One owned non-terminal session past its armed-deadline obligation.
    LifecycleDeadlineViolation(Box<OperatorStatusLifecycleDeadlineViolationMessage>),
    /// Process-local ingestion evidence for one watched repository.
    RepositoryIngestion(Box<OperatorStatusRepositoryIngestion>),
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
        OperatorStatusMessage::Start {}
        | OperatorStatusMessage::End(_)
        | OperatorStatusMessage::RepositoryIngestion(_) => true,
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

/// Outcome of the most recently started periodic repository poll.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryPollOutcome {
    InProgress,
    Succeeded,
    ClientFailed,
    ObservationFailed,
    StoreFailed,
    FrontierConflict,
    Cancelled,
}

/// One periodic poll's UTC start and outcome.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryPollAttempt {
    pub attempted_at: String,
    pub outcome: RepositoryPollOutcome,
}

/// Ingestion measurements since the current daemon process started.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorStatusRepositoryIngestion {
    pub repository: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub last_successful_observation: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub last_poll: Option<RepositoryPollAttempt>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub last_accepted_webhook: Option<String>,
    pub events_recorded: CanonicalU64,
}
