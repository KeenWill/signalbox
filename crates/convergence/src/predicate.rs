use crate::{ConvergencePolicy, array, text};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Every final predicate failure distinguished by the reference reconciler.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reason {
    PullRequestIsDraft,
    ReviewChangesRequested,
    CheckInventoryUnsettled,
    UnresolvedReviewThreads { count: usize },
    UndispositionedReviewThreads { count: usize },
    QuietReviewNotCompletedForCurrentHead,
    DescriptionExceeds350Words,
    ChecksNotForCurrentHead,
    CheckRollupMissing,
    CheckNotGreen { name: String, state: String },
    BaseConflict,
    Mergeability { state: String },
    BaseAncestryUnknown,
    BaseCommitsNotInHead { count: u64 },
}
impl Reason {
    pub fn reference_reason(&self) -> String {
        match self {
            Self::PullRequestIsDraft => "pull-request-is-draft".into(),
            Self::ReviewChangesRequested => "review-changes-requested".into(),
            Self::CheckInventoryUnsettled => "check-inventory-unsettled".into(),
            Self::UnresolvedReviewThreads { count } => format!("unresolved-review-threads:{count}"),
            Self::UndispositionedReviewThreads { count } => {
                format!("undispositioned-review-threads:{count}")
            }
            Self::QuietReviewNotCompletedForCurrentHead => {
                "quiet-review-not-completed-for-current-head".into()
            }
            Self::DescriptionExceeds350Words => "description-exceeds-350-words".into(),
            Self::ChecksNotForCurrentHead => "checks-not-for-current-head".into(),
            Self::CheckRollupMissing => "check-rollup-missing".into(),
            Self::CheckNotGreen { name, state } => format!("check-not-green:{name}:{state}"),
            Self::BaseConflict => "base-conflict".into(),
            Self::Mergeability { state } => format!("mergeability-{state}"),
            Self::BaseAncestryUnknown => "base-ancestry-unknown".into(),
            Self::BaseCommitsNotInHead { count } => format!("base-commits-not-in-head:{count}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict")]
pub enum Verdict {
    Converged,
    NotConverged { reasons: Vec<Reason> },
}
impl Verdict {
    pub fn is_converged(&self) -> bool {
        matches!(self, Self::Converged)
    }
    pub fn reasons(&self) -> &[Reason] {
        match self {
            Self::Converged => &[],
            Self::NotConverged { reasons } => reasons,
        }
    }
}

/// Evidence projection consumed by the final predicate and the Python driver.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Facts {
    pub head_oid: String,
    pub checked_head_oid: Option<String>,
    #[serde(default)]
    pub is_draft: bool,
    pub review_decision: Option<String>,
    pub check_inventory_stable: Option<bool>,
    pub review_threads: Vec<Thread>,
    pub quiet_review_head_oids: Vec<String>,
    #[serde(default)]
    pub planning_only: bool,
    #[serde(default)]
    pub review_exempt_since_quiet_review: bool,
    pub body: Option<String>,
    pub check_rollup_state: Option<String>,
    pub checks: Vec<Value>,
    pub mergeable: String,
    pub base_commits_not_in_head: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub id: Option<String>,
    pub is_resolved: bool,
    pub is_dispositioned: bool,
    #[serde(default)]
    pub is_escalated: bool,
    #[serde(default)]
    pub is_informational: bool,
    pub latest_reviewer_at: Option<String>,
    pub disposition_at: Option<String>,
    pub disposition_kind: Option<String>,
    pub fixing_commit: Option<String>,
    #[serde(default)]
    pub review_ids: Vec<String>,
    pub resolution_observed_at: Option<String>,
}

pub(crate) fn check_name(check: &Value) -> &str {
    if text(&check["__typename"]) == "CheckRun" {
        text(&check["name"])
    } else {
        text(&check["context"])
    }
}
pub(crate) fn check_green(check: &Value) -> bool {
    if text(&check["__typename"]) == "CheckRun" {
        text(&check["status"]) == "COMPLETED"
            && matches!(
                text(&check["conclusion"]),
                "SUCCESS" | "NEUTRAL" | "SKIPPED"
            )
    } else {
        text(&check["state"]) == "SUCCESS"
    }
}
pub(crate) fn check_state(check: &Value) -> &str {
    if text(&check["__typename"]) == "CheckRun" {
        check["conclusion"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| check["status"].as_str())
            .unwrap_or("UNKNOWN")
    } else {
        check["state"].as_str().unwrap_or("UNKNOWN")
    }
}
pub(crate) fn inventory(checks: &[Value]) -> Vec<String> {
    let mut result: Vec<_> = checks
        .iter()
        .map(|c| format!("{}:{}", text(&c["__typename"]), check_name(c)))
        .collect();
    result.sort();
    result
}
pub(crate) fn checks(node: &Value) -> &[Value] {
    array(&node["headRef"]["target"]["statusCheckRollup"]["contexts"]["nodes"])
}

pub fn evaluate_facts(facts: &Facts, policy: &ConvergencePolicy) -> Verdict {
    let mut reasons = Vec::new();
    if facts.is_draft {
        reasons.push(Reason::PullRequestIsDraft);
    }
    if facts.review_decision.as_deref() == Some("CHANGES_REQUESTED") {
        reasons.push(Reason::ReviewChangesRequested);
    }
    if facts.check_inventory_stable == Some(false) {
        reasons.push(Reason::CheckInventoryUnsettled);
    }
    let unresolved = facts
        .review_threads
        .iter()
        .filter(|t| !t.is_resolved && !t.is_escalated)
        .count();
    let undispositioned = facts
        .review_threads
        .iter()
        .filter(|t| !t.is_dispositioned)
        .count();
    if unresolved > 0 {
        reasons.push(Reason::UnresolvedReviewThreads { count: unresolved });
    }
    if undispositioned > 0 {
        reasons.push(Reason::UndispositionedReviewThreads {
            count: undispositioned,
        });
    }
    if !facts.planning_only
        && !facts.quiet_review_head_oids.contains(&facts.head_oid)
        && !facts.review_exempt_since_quiet_review
    {
        reasons.push(Reason::QuietReviewNotCompletedForCurrentHead);
    }
    if facts
        .body
        .as_ref()
        .is_some_and(|body| word_count(body) > 350)
    {
        reasons.push(Reason::DescriptionExceeds350Words);
    }
    if facts.checked_head_oid.as_ref() != Some(&facts.head_oid) {
        reasons.push(Reason::ChecksNotForCurrentHead);
    }
    if facts.check_rollup_state.is_none() {
        reasons.push(Reason::CheckRollupMissing);
    }
    for check in &facts.checks {
        if !policy.is_non_gating(check_name(check)) && !check_green(check) {
            reasons.push(Reason::CheckNotGreen {
                name: check_name(check).into(),
                state: check_state(check).into(),
            });
        }
    }
    match facts.mergeable.as_str() {
        "MERGEABLE" => {}
        "CONFLICTING" => reasons.push(Reason::BaseConflict),
        state => reasons.push(Reason::Mergeability {
            state: state.to_lowercase(),
        }),
    }
    match facts.base_commits_not_in_head {
        None => reasons.push(Reason::BaseAncestryUnknown),
        Some(0) => {}
        Some(count) => reasons.push(Reason::BaseCommitsNotInHead { count }),
    }
    if reasons.is_empty() {
        Verdict::Converged
    } else {
        Verdict::NotConverged { reasons }
    }
}

fn word_count(body: &str) -> usize {
    // A run of word characters with internal apostrophes or hyphens is one word.
    body.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '\'' && c != '-')
        .filter(|part| part.chars().any(|c| c.is_alphanumeric() || c == '_'))
        .count()
}
