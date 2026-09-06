use serde_json::{Value, json};
use signalbox_ownership_seam::{
    CheckConclusion, ChecksOutcome, MergeableState, ReactionSubject,
    RepoWatchMergedPullRequestBaselineV1, RepoWatchObservation, RepoWatchPullRequestLifecycle,
    RepoWatchPullRequestState, RepoWatchThreadState, ReviewState,
};

pub(crate) fn observation_payload(
    observation: &RepoWatchObservation,
    merged_baselines: &[RepoWatchMergedPullRequestBaselineV1],
) -> Value {
    let mut merged_baselines = merged_baselines.iter().collect::<Vec<_>>();
    merged_baselines.sort_by_key(|baseline| baseline.number());
    json!({
        "signal_reviewers": observation
            .signal_reviewers()
            .iter()
            .map(|reviewer| reviewer.as_str())
            .collect::<Vec<_>>(),
        "pull_requests": observation
            .state()
            .pull_requests()
            .iter()
            .map(pull_request_payload)
            .collect::<Vec<_>>(),
        "workflow_runs": observation
            .state()
            .workflow_runs()
            .iter()
            .map(|run| json!({
                "id": run.id().get(),
                "workflow_id": run.workflow_id().get(),
                "attempt": run.attempt().get(),
                "branch": run.branch().as_str(),
                "workflow": run.workflow().as_str(),
                "conclusion": check_conclusion_storage(run.conclusion()),
            }))
            .collect::<Vec<_>>(),
        "branch_heads": observation
            .state()
            .branch_heads()
            .iter()
            .map(|head| json!({
                "branch": head.branch().as_str(),
                "head": head.head().as_str(),
            }))
            .collect::<Vec<_>>(),
        "merged_pull_requests": merged_baselines
            .into_iter()
            .map(merged_pull_request_payload)
            .collect::<Vec<_>>(),
    })
}

fn merged_pull_request_payload(baseline: &RepoWatchMergedPullRequestBaselineV1) -> Value {
    json!({
        "number": baseline.number().get(),
        "head_sha": baseline.head_sha().as_str(),
        "signal_reviewers": baseline
            .signal_reviewers()
            .iter()
            .map(|reviewer| reviewer.as_str())
            .collect::<Vec<_>>(),
        "labels": baseline
            .labels()
            .iter()
            .map(|label| label.as_str())
            .collect::<Vec<_>>(),
        "mergeable_state": mergeable_state_storage(baseline.mergeable_state()),
        "completed_check_suites": baseline
            .completed_check_suites()
            .iter()
            .map(|suite| json!({
                "id": suite.id().get(),
                "completion_generation": suite.completion_generation().as_str(),
            }))
            .collect::<Vec<_>>(),
        "completed_check_runs": baseline
            .completed_check_runs()
            .iter()
            .map(|run| json!({
                "id": run.id().get(),
                "completion_generation": run.completion_generation().as_str(),
                "conclusion": check_conclusion_storage(run.conclusion()),
            }))
            .collect::<Vec<_>>(),
        "review_ids": baseline
            .review_ids()
            .iter()
            .map(|id| id.get())
            .collect::<Vec<_>>(),
        "threads": baseline
            .threads()
            .iter()
            .map(|thread| json!({
                "thread": thread.thread().as_str(),
                "state": thread_state_storage(thread.state()),
            }))
            .collect::<Vec<_>>(),
        "reactions": baseline
            .reactions()
            .iter()
            .map(|reaction| json!({
                "subject": reaction_subject_payload(reaction.subject()),
                "reactor": reaction.reactor().as_str(),
                "content": reaction.content().as_str(),
            }))
            .collect::<Vec<_>>(),
    })
}

fn pull_request_payload(state: &RepoWatchPullRequestState) -> Value {
    let context = state.context();
    json!({
        "context": {
            "number": context.number().get(),
            "head_sha": context.head_sha().as_str(),
            "head_repository": context.head_repository().as_str(),
            "base_branch": context.base_branch().as_str(),
            "head_branch": context.head_branch().as_str(),
            "title": context.title().as_str(),
            "body": context.body().as_str(),
            "labels": context
                .labels()
                .iter()
                .map(|label| label.as_str())
                .collect::<Vec<_>>(),
            "draft": context.draft(),
            "author": context.author().map(|author| author.as_str()),
        },
        "lifecycle": pull_request_lifecycle_storage(state.lifecycle()),
        "mergeable_state": mergeable_state_storage(state.mergeable_state()),
        "completed_check_suites": state
            .completed_check_suites()
            .iter()
            .map(|suite| json!({
                "id": suite.id().get(),
                "completion_generation": suite.completion_generation().as_str(),
                "outcome": checks_outcome_storage(suite.outcome()),
            }))
            .collect::<Vec<_>>(),
        "completed_check_runs": state
            .completed_check_runs()
            .iter()
            .map(|run| json!({
                "id": run.id().get(),
                "completion_generation": run.completion_generation().as_str(),
                "name": run.name().as_str(),
                "conclusion": check_conclusion_storage(run.conclusion()),
            }))
            .collect::<Vec<_>>(),
        "reviews": state
            .reviews()
            .iter()
            .map(|review| json!({
                "id": review.id().get(),
                "reviewer": review.reviewer().as_str(),
                "state": review.state().map(review_state_storage),
                "commit": review.commit().as_str(),
            }))
            .collect::<Vec<_>>(),
        "threads": state
            .threads()
            .iter()
            .map(|thread| json!({
                "thread": thread.thread().as_str(),
                "state": thread_state_storage(thread.state()),
            }))
            .collect::<Vec<_>>(),
        "reactions": state
            .reactions()
            .iter()
            .map(|reaction| json!({
                "subject": reaction_subject_payload(reaction.subject()),
                "reactor": reaction.reactor().as_str(),
                "content": reaction.content().as_str(),
            }))
            .collect::<Vec<_>>(),
    })
}

const fn pull_request_lifecycle_storage(value: RepoWatchPullRequestLifecycle) -> &'static str {
    match value {
        RepoWatchPullRequestLifecycle::Open => "open",
        RepoWatchPullRequestLifecycle::Closed => "closed",
        RepoWatchPullRequestLifecycle::Merged => "merged",
    }
}

const fn mergeable_state_storage(value: MergeableState) -> &'static str {
    match value {
        MergeableState::Mergeable => "mergeable",
        MergeableState::Conflicting => "conflicting",
        MergeableState::Unknown => "unknown",
    }
}

const fn checks_outcome_storage(value: ChecksOutcome) -> &'static str {
    match value {
        ChecksOutcome::Success => "success",
        ChecksOutcome::Failure => "failure",
    }
}

const fn check_conclusion_storage(value: CheckConclusion) -> &'static str {
    match value {
        CheckConclusion::Success => "success",
        CheckConclusion::Failure => "failure",
        CheckConclusion::Neutral => "neutral",
        CheckConclusion::Cancelled => "cancelled",
        CheckConclusion::Skipped => "skipped",
        CheckConclusion::TimedOut => "timed_out",
        CheckConclusion::ActionRequired => "action_required",
        CheckConclusion::Stale => "stale",
        CheckConclusion::StartupFailure => "startup_failure",
    }
}

const fn review_state_storage(value: ReviewState) -> &'static str {
    match value {
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "changes_requested",
        ReviewState::Commented => "commented",
    }
}

const fn thread_state_storage(value: RepoWatchThreadState) -> &'static str {
    match value {
        RepoWatchThreadState::Open => "open",
        RepoWatchThreadState::Resolved => "resolved",
    }
}

fn reaction_subject_payload(value: ReactionSubject) -> Value {
    match value {
        ReactionSubject::PullRequestBody => json!({ "kind": "pull_request_body" }),
        ReactionSubject::IssueComment { id } => {
            json!({ "kind": "issue_comment", "id": id.get() })
        }
        ReactionSubject::ReviewComment { id } => {
            json!({ "kind": "review_comment", "id": id.get() })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use signalbox_ownership_seam::{
        CommitSha, MergeableState, PullRequestNumber, RepoWatchMergedPullRequestBaselineInputV1,
        RepoWatchMergedPullRequestBaselineV1, RepoWatchObservation, RepoWatchRepositoryState,
    };

    use super::observation_payload;

    fn merged_baseline(number: u64) -> RepoWatchMergedPullRequestBaselineV1 {
        RepoWatchMergedPullRequestBaselineV1::try_new(RepoWatchMergedPullRequestBaselineInputV1 {
            number: PullRequestNumber::new(
                NonZeroU64::new(number).expect("fixture number is positive"),
            ),
            head_sha: CommitSha::try_new(format!("{number:040x}"))
                .expect("fixture commit is valid"),
            signal_reviewers: Vec::new(),
            labels: Vec::new(),
            mergeable_state: MergeableState::Mergeable,
            completed_check_suites: Vec::new(),
            completed_check_runs: Vec::new(),
            review_ids: Vec::new(),
            threads: Vec::new(),
            reactions: Vec::new(),
        })
        .expect("fixture baseline is valid")
    }

    #[test]
    fn merged_baseline_collection_order_is_not_projection_identity() {
        let observation =
            RepoWatchObservation::new(Vec::new(), RepoWatchRepositoryState::default());
        let first = merged_baseline(1);
        let second = merged_baseline(2);

        assert_eq!(
            observation_payload(&observation, &[first.clone(), second.clone()]),
            observation_payload(&observation, &[second, first])
        );
    }
}
