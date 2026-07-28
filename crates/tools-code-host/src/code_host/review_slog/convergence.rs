//! Convergence evidence, verdict derivation, and reviewer-activity scanning.

use serde_json::{Value, json};

use crate::code_host::{
    arguments::{valid_cursor, valid_opaque_id, valid_revision},
    result::{MAX_RESULT_ITEMS, valid_path, valid_required_text, valid_text},
};

const STARVATION_MARKER: &str = "reached your Codex usage limits";
const REVIEWED_COMMIT_LABEL: &str = "Reviewed commit:";

/// Deterministic convergence verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConvergenceVerdict {
    /// Every bounded source was complete and every convergence condition held.
    Converged,
    /// Only unresolved escalation-marker threads remain.
    ConvergedWithEscalations,
    /// Complete evidence establishes at least one convergence failure.
    NotConverged,
    /// A bounded source was incomplete, so no final verdict is claimed.
    Indeterminate,
}

impl ConvergenceVerdict {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Converged => "converged",
            Self::ConvergedWithEscalations => "converged_with_escalations",
            Self::NotConverged => "not_converged",
            Self::Indeterminate => "indeterminate",
        }
    }
}

/// Whether the latest observed reviewer verdict covers the current head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewerVerdictStatus {
    /// The latest actual verdict names the current head.
    CurrentHead,
    /// An actual verdict exists, but it names another revision.
    StaleHead,
    /// No actual reviewer verdict was observed.
    Missing,
}

impl ReviewerVerdictStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::CurrentHead => "current_head",
            Self::StaleHead => "stale_head",
            Self::Missing => "missing",
        }
    }
}

/// One exact review-thread identity used by convergence blockers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewThreadIdentity {
    id: String,
    path: String,
    finding_title: String,
}

impl ReviewThreadIdentity {
    /// Validates one bounded thread identity and its display evidence.
    pub fn try_new(id: String, path: String, finding_title: String) -> Option<Self> {
        (valid_opaque_id(&id) && valid_path(&path) && valid_text(&finding_title)).then_some(Self {
            id,
            path,
            finding_title,
        })
    }

    /// Borrows the opaque thread identity.
    pub fn id(&self) -> &str {
        &self.id
    }

    fn into_value(self) -> Value {
        json!({"finding_title": self.finding_title, "id": self.id, "path": self.path})
    }
}

/// One bounded check-rollup context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewCheck {
    name: String,
    status: String,
    conclusion: Option<String>,
}

impl ReviewCheck {
    /// Validates exact code-host check evidence.
    pub fn try_new(name: String, status: String, conclusion: Option<String>) -> Option<Self> {
        (valid_required_text(&name)
            && valid_required_text(&status)
            && conclusion.as_deref().is_none_or(valid_required_text))
        .then_some(Self {
            name,
            status,
            conclusion,
        })
    }

    fn green(&self) -> bool {
        self.conclusion.as_deref().is_some_and(|conclusion| {
            matches!(
                conclusion.to_ascii_lowercase().as_str(),
                "success" | "neutral" | "skipped"
            )
        })
    }

    fn into_value(self) -> Value {
        json!({"conclusion": self.conclusion, "name": self.name, "status": self.status})
    }
}

/// Time-ordered reviewer coverage derived from issue comments and review bodies.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewerVerdictEvidence {
    status: ReviewerVerdictStatus,
    reviewed_revision: Option<String>,
    reviewed_at: Option<String>,
    starvation_after_verdict: bool,
    latest_starvation_at: Option<String>,
    source_truncated: bool,
    comments_previous_cursor: Option<String>,
    reviews_previous_cursor: Option<String>,
}

/// Checked fields for reviewer verdict evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewerVerdictFields {
    /// Coverage status for the current head.
    pub status: ReviewerVerdictStatus,
    /// Exact abbreviated or full revision named by the latest verdict.
    pub reviewed_revision: Option<String>,
    /// Exact code-host timestamp of the latest verdict.
    pub reviewed_at: Option<String>,
    /// Whether a usage-limit message followed the latest actual verdict.
    pub starvation_after_verdict: bool,
    /// Exact timestamp of the latest observed usage-limit message.
    pub latest_starvation_at: Option<String>,
    /// Whether either bounded activity source omitted older items.
    pub source_truncated: bool,
    /// Cursor for the preceding issue-comment page when truncated.
    pub comments_previous_cursor: Option<String>,
    /// Cursor for the preceding review page when truncated.
    pub reviews_previous_cursor: Option<String>,
}

impl ReviewerVerdictEvidence {
    /// Validates one complete bounded reviewer-evidence projection.
    pub fn try_new(fields: ReviewerVerdictFields) -> Option<Self> {
        let revision_valid = fields.reviewed_revision.as_deref().is_none_or(|revision| {
            (7..=40).contains(&revision.len())
                && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
        let timestamps_valid = fields
            .reviewed_at
            .as_deref()
            .is_none_or(valid_required_text)
            && fields
                .latest_starvation_at
                .as_deref()
                .is_none_or(valid_required_text);
        let cursors_valid = fields
            .comments_previous_cursor
            .as_deref()
            .is_none_or(valid_cursor)
            && fields
                .reviews_previous_cursor
                .as_deref()
                .is_none_or(valid_cursor);
        let cursor_shape = fields.source_truncated
            || (fields.comments_previous_cursor.is_none()
                && fields.reviews_previous_cursor.is_none());
        let verdict_shape = match fields.status {
            ReviewerVerdictStatus::Missing => {
                fields.reviewed_revision.is_none() && fields.reviewed_at.is_none()
            }
            ReviewerVerdictStatus::CurrentHead | ReviewerVerdictStatus::StaleHead => {
                fields.reviewed_revision.is_some() && fields.reviewed_at.is_some()
            }
        };
        (revision_valid && timestamps_valid && cursors_valid && cursor_shape && verdict_shape)
            .then_some(Self {
                status: fields.status,
                reviewed_revision: fields.reviewed_revision,
                reviewed_at: fields.reviewed_at,
                starvation_after_verdict: fields.starvation_after_verdict,
                latest_starvation_at: fields.latest_starvation_at,
                source_truncated: fields.source_truncated,
                comments_previous_cursor: fields.comments_previous_cursor,
                reviews_previous_cursor: fields.reviews_previous_cursor,
            })
    }

    pub(super) const fn status(&self) -> ReviewerVerdictStatus {
        self.status
    }

    pub(super) const fn starved(&self) -> bool {
        self.starvation_after_verdict
    }

    fn into_value(self) -> Value {
        json!({
            "comments_previous_cursor": self.comments_previous_cursor,
            "latest_starvation_at": self.latest_starvation_at,
            "reviewed_at": self.reviewed_at,
            "reviewed_revision": self.reviewed_revision,
            "reviews_previous_cursor": self.reviews_previous_cursor,
            "source_truncated": self.source_truncated,
            "starvation_after_verdict": self.starvation_after_verdict,
            "status": self.status.as_str(),
        })
    }
}

/// Typed result of `change_request_convergence_state`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConvergenceStateResult {
    head_revision: String,
    mergeable_state: String,
    ci_rollup_state: Option<String>,
    checks: Vec<ReviewCheck>,
    checks_truncated: bool,
    checks_next_cursor: Option<String>,
    unresolved_threads: Vec<ReviewThreadIdentity>,
    open_escalations: Vec<ReviewThreadIdentity>,
    buried_escalations: Vec<ReviewThreadIdentity>,
    threads_truncated: bool,
    threads_next_cursor: Option<String>,
    reviewer: ReviewerVerdictEvidence,
    verdict: ConvergenceVerdict,
}

/// Complete checked fields for convergence-state construction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConvergenceStateFields {
    /// Exact current head revision.
    pub head_revision: String,
    /// Exact code-host mergeable-state spelling.
    pub mergeable_state: String,
    /// Optional exact check-rollup state; absence means no checks exist.
    pub ci_rollup_state: Option<String>,
    /// First bounded status-context page.
    pub checks: Vec<ReviewCheck>,
    /// Whether more status contexts exist.
    pub checks_truncated: bool,
    /// Opaque next status-context cursor.
    pub checks_next_cursor: Option<String>,
    /// Unresolved threads observed in the bounded page.
    pub unresolved_threads: Vec<ReviewThreadIdentity>,
    /// Unresolved threads whose last comment carries the escalation marker.
    pub open_escalations: Vec<ReviewThreadIdentity>,
    /// Resolved threads whose last comment carries the escalation marker.
    pub buried_escalations: Vec<ReviewThreadIdentity>,
    /// Whether more review threads exist.
    pub threads_truncated: bool,
    /// Opaque next review-thread cursor.
    pub threads_next_cursor: Option<String>,
    /// Reviewer verdict evidence merged in code-host timestamp order.
    pub reviewer: ReviewerVerdictEvidence,
}

impl ConvergenceStateResult {
    /// Validates bounded evidence and derives its deterministic verdict.
    pub fn try_new(fields: ConvergenceStateFields) -> Option<Self> {
        let lists_valid = fields.checks.len() <= MAX_RESULT_ITEMS
            && fields.unresolved_threads.len() <= MAX_RESULT_ITEMS
            && fields.open_escalations.len() <= MAX_RESULT_ITEMS
            && fields.buried_escalations.len() <= MAX_RESULT_ITEMS;
        let text_valid = valid_revision(&fields.head_revision)
            && valid_required_text(&fields.mergeable_state)
            && fields
                .ci_rollup_state
                .as_deref()
                .is_none_or(valid_required_text);
        let cursors_valid = fields
            .checks_next_cursor
            .as_deref()
            .is_none_or(valid_cursor)
            && fields
                .threads_next_cursor
                .as_deref()
                .is_none_or(valid_cursor)
            && fields.checks_truncated == fields.checks_next_cursor.is_some()
            && fields.threads_truncated == fields.threads_next_cursor.is_some();
        if !(lists_valid && text_valid && cursors_valid) {
            return None;
        }
        let incomplete =
            fields.checks_truncated || fields.threads_truncated || fields.reviewer.source_truncated;
        let ci_green = fields
            .ci_rollup_state
            .as_deref()
            .is_none_or(|state| state.eq_ignore_ascii_case("success"));
        let undispositioned = fields
            .unresolved_threads
            .len()
            .saturating_sub(fields.open_escalations.len());
        let verdict = if incomplete {
            ConvergenceVerdict::Indeterminate
        } else if !ci_green
            || !fields.buried_escalations.is_empty()
            || undispositioned > 0
            || fields.mergeable_state.eq_ignore_ascii_case("conflicting")
            || fields.reviewer.status != ReviewerVerdictStatus::CurrentHead
        {
            ConvergenceVerdict::NotConverged
        } else if fields.open_escalations.is_empty() {
            ConvergenceVerdict::Converged
        } else {
            ConvergenceVerdict::ConvergedWithEscalations
        };
        Some(Self {
            head_revision: fields.head_revision,
            mergeable_state: fields.mergeable_state,
            ci_rollup_state: fields.ci_rollup_state,
            checks: fields.checks,
            checks_truncated: fields.checks_truncated,
            checks_next_cursor: fields.checks_next_cursor,
            unresolved_threads: fields.unresolved_threads,
            open_escalations: fields.open_escalations,
            buried_escalations: fields.buried_escalations,
            threads_truncated: fields.threads_truncated,
            threads_next_cursor: fields.threads_next_cursor,
            reviewer: fields.reviewer,
            verdict,
        })
    }

    /// Returns the deterministic verdict.
    pub const fn verdict(&self) -> ConvergenceVerdict {
        self.verdict
    }

    /// Borrows the exact current head revision.
    pub fn head_revision(&self) -> &str {
        &self.head_revision
    }

    pub(super) fn evidence_truncated(&self) -> bool {
        self.checks_truncated || self.threads_truncated || self.reviewer.source_truncated
    }

    pub(super) fn ci_green(&self) -> bool {
        self.ci_rollup_state
            .as_deref()
            .is_none_or(|state| state.eq_ignore_ascii_case("success"))
    }

    pub(super) fn failing_checks(&self) -> Vec<String> {
        self.checks
            .iter()
            .filter(|check| !check.green())
            .map(|check| check.name.clone())
            .collect()
    }

    pub(super) fn merge_conflicting(&self) -> bool {
        self.mergeable_state.eq_ignore_ascii_case("conflicting")
    }

    pub(super) fn buried_ids(&self) -> Vec<String> {
        self.buried_escalations
            .iter()
            .map(|thread| thread.id.clone())
            .collect()
    }

    pub(super) const fn reviewer(&self) -> &ReviewerVerdictEvidence {
        &self.reviewer
    }

    pub(super) fn into_value(self) -> Value {
        let ci_green = self.ci_green();
        json!({
            "buried_escalations": self.buried_escalations.into_iter().map(ReviewThreadIdentity::into_value).collect::<Vec<_>>(),
            "checks": self.checks.into_iter().map(ReviewCheck::into_value).collect::<Vec<_>>(),
            "checks_next_cursor": self.checks_next_cursor,
            "checks_truncated": self.checks_truncated,
            "ci_green": ci_green,
            "ci_rollup_state": self.ci_rollup_state,
            "head_revision": self.head_revision,
            "mergeable_state": self.mergeable_state,
            "open_escalations": self.open_escalations.into_iter().map(ReviewThreadIdentity::into_value).collect::<Vec<_>>(),
            "reviewer_verdict": self.reviewer.into_value(),
            "threads_next_cursor": self.threads_next_cursor,
            "threads_truncated": self.threads_truncated,
            "unresolved_thread_count": self.unresolved_threads.len(),
            "unresolved_threads": self.unresolved_threads.into_iter().map(ReviewThreadIdentity::into_value).collect::<Vec<_>>(),
            "verdict": self.verdict.as_str(),
        })
    }
}

/// One raw code-host activity body used for time-ordered reviewer scanning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReviewerActivity {
    pub(crate) author: Option<String>,
    pub(crate) body: String,
    pub(crate) created_at: String,
}

/// Merges review bodies and issue comments in exact code-host timestamp order.
pub(crate) fn reviewer_verdict_evidence(
    head_revision: &str,
    mut activities: Vec<ReviewerActivity>,
    source_truncated: bool,
    comments_previous_cursor: Option<String>,
    reviews_previous_cursor: Option<String>,
) -> Option<ReviewerVerdictEvidence> {
    activities.sort_by(|left, right| left.created_at.cmp(&right.created_at));
    let mut reviewed_revision = None;
    let mut reviewed_at = None;
    let mut latest_starvation_at = None;
    for activity in activities {
        let reviewer = activity
            .author
            .as_deref()
            .is_some_and(|author| author.to_ascii_lowercase().contains("codex"));
        if !reviewer {
            continue;
        }
        if activity.body.contains(STARVATION_MARKER) {
            latest_starvation_at = Some(activity.created_at.clone());
        }
        if let Some(revision) = reviewed_commit_from_body(&activity.body) {
            reviewed_revision = Some(revision);
            reviewed_at = Some(activity.created_at);
        }
    }
    let status = match reviewed_revision.as_deref() {
        None => ReviewerVerdictStatus::Missing,
        Some(revision)
            if head_revision.starts_with(revision)
                || revision.starts_with(head_revision.get(..12).unwrap_or(head_revision)) =>
        {
            ReviewerVerdictStatus::CurrentHead
        }
        Some(_) => ReviewerVerdictStatus::StaleHead,
    };
    let starvation_after_verdict = match (&latest_starvation_at, &reviewed_at) {
        (Some(starved), Some(reviewed)) => starved > reviewed,
        (Some(_), None) => true,
        (None, _) => false,
    };
    ReviewerVerdictEvidence::try_new(ReviewerVerdictFields {
        status,
        reviewed_revision,
        reviewed_at,
        starvation_after_verdict,
        latest_starvation_at,
        source_truncated,
        comments_previous_cursor,
        reviews_previous_cursor,
    })
}

fn reviewed_commit_from_body(body: &str) -> Option<String> {
    let suffix = body.split_once(REVIEWED_COMMIT_LABEL)?.1;
    let suffix = suffix.trim_start();
    let suffix = suffix.trim_start_matches('*').trim_start();
    let suffix = suffix.strip_prefix('`').unwrap_or(suffix);
    let revision: String = suffix
        .chars()
        .take_while(|character| character.is_ascii_hexdigit())
        .take(40)
        .collect();
    ((7..=40).contains(&revision.len())).then_some(revision)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD_REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
    const EARLIER: &str = "2026-07-27T10:00:00Z";
    const LATER: &str = "2026-07-27T11:00:00Z";

    /// Reviewer verdict extraction scans review bodies, not only issue
    /// comments, and recognizes the exact current head.
    #[test]
    fn review_body_establishes_current_head_coverage() {
        let evidence = reviewer_verdict_evidence(
            HEAD_REVISION,
            vec![ReviewerActivity {
                author: Some(String::from("chatgpt-codex-connector")),
                body: format!("Reviewed commit: `{HEAD_REVISION}`"),
                created_at: String::from(EARLIER),
            }],
            false,
            None,
            None,
        )
        .expect("fixture reviewer evidence is admitted");

        assert_eq!(evidence.status(), ReviewerVerdictStatus::CurrentHead);
        assert!(!evidence.starved());
    }

    /// Source concatenation order cannot hide a later usage-limit response:
    /// code-host timestamps determine the final starvation posture.
    #[test]
    fn timestamp_order_exposes_starvation_after_actual_verdict() {
        let evidence = reviewer_verdict_evidence(
            HEAD_REVISION,
            vec![
                ReviewerActivity {
                    author: Some(String::from("chatgpt-codex-connector")),
                    body: String::from("You have reached your Codex usage limits"),
                    created_at: String::from(LATER),
                },
                ReviewerActivity {
                    author: Some(String::from("chatgpt-codex-connector")),
                    body: format!("Reviewed commit: **`{HEAD_REVISION}`"),
                    created_at: String::from(EARLIER),
                },
            ],
            false,
            None,
            None,
        )
        .expect("fixture reviewer evidence is admitted");

        assert_eq!(evidence.status(), ReviewerVerdictStatus::CurrentHead);
        assert!(evidence.starved());
    }

    /// A partial check rollup cannot claim convergence even when every
    /// observed check is green.
    #[test]
    fn truncated_checks_make_the_verdict_indeterminate() {
        let reviewer = reviewer_verdict_evidence(
            HEAD_REVISION,
            vec![ReviewerActivity {
                author: Some(String::from("chatgpt-codex-connector")),
                body: format!("Reviewed commit: `{HEAD_REVISION}`"),
                created_at: String::from(EARLIER),
            }],
            false,
            None,
            None,
        )
        .expect("fixture reviewer evidence is admitted");
        let result = ConvergenceStateResult::try_new(ConvergenceStateFields {
            head_revision: String::from(HEAD_REVISION),
            mergeable_state: String::from("MERGEABLE"),
            ci_rollup_state: Some(String::from("SUCCESS")),
            checks: Vec::new(),
            checks_truncated: true,
            checks_next_cursor: Some(String::from("check-cursor")),
            unresolved_threads: Vec::new(),
            open_escalations: Vec::new(),
            buried_escalations: Vec::new(),
            threads_truncated: false,
            threads_next_cursor: None,
            reviewer,
        })
        .expect("fixture convergence result is admitted");

        assert_eq!(result.verdict(), ConvergenceVerdict::Indeterminate);
    }
}
