use std::num::NonZeroU64;

use serde_json::Value;
use signalbox_session_ownership::{
    BranchName, CheckConclusion, CheckRunName, ChecksOutcome, CommitSha, GitHubObjectId, LabelName,
    MergeableState, PullRequestBody, PullRequestEventContext, PullRequestEventContextInput,
    PullRequestNumber, PullRequestTitle, ReactionContent, ReactionSubject, RepoWatchAuthorLogin,
    RepoWatchBranchHead, RepoWatchCheckCompletionGeneration, RepoWatchCheckRunObservation,
    RepoWatchCheckSuiteObservation, RepoWatchObservation, RepoWatchPullRequestLifecycle,
    RepoWatchPullRequestState, RepoWatchPullRequestStateInput, RepoWatchReactionObservation,
    RepoWatchRepositoryState, RepoWatchRepositoryStateInput, RepoWatchReviewObservation,
    RepoWatchThreadObservation, RepoWatchThreadState, RepoWatchWorkflowRunAttempt,
    RepoWatchWorkflowRunObservation, RepositorySlug, ReviewState, ReviewThreadId, WorkflowName,
};

pub(crate) fn observation(value: &Value) -> Option<RepoWatchObservation> {
    Some(RepoWatchObservation::new(
        array(&value["signal_reviewers"], |v| {
            RepoWatchAuthorLogin::try_new(text(v)?).ok()
        })?,
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: array(&value["pull_requests"], pull_request)?,
            workflow_runs: array(&value["workflow_runs"], |v| {
                Some(RepoWatchWorkflowRunObservation::new(
                    object_id(&v["id"])?,
                    object_id(&v["workflow_id"])?,
                    RepoWatchWorkflowRunAttempt::new(positive(&v["attempt"])?),
                    BranchName::try_new(text(&v["branch"])?).ok()?,
                    WorkflowName::try_new(text(&v["workflow"])?).ok()?,
                    conclusion(&v["conclusion"])?,
                ))
            })?,
            branch_heads: array(&value["branch_heads"], |v| {
                Some(RepoWatchBranchHead::new(
                    BranchName::try_new(text(&v["branch"])?).ok()?,
                    CommitSha::try_new(text(&v["head"])?).ok()?,
                ))
            })?,
        })
        .ok()?,
    ))
}

pub(crate) fn context(v: &Value) -> Option<PullRequestEventContext> {
    Some(PullRequestEventContext::new(PullRequestEventContextInput {
        number: PullRequestNumber::new(positive(&v["number"])?),
        head_sha: CommitSha::try_new(text(&v["head_sha"])?).ok()?,
        head_repository: RepositorySlug::try_new(text(&v["head_repository"])?).ok()?,
        base_branch: BranchName::try_new(text(&v["base_branch"])?).ok()?,
        head_branch: BranchName::try_new(text(&v["head_branch"])?).ok()?,
        title: PullRequestTitle::try_new(text(&v["title"])?).ok()?,
        body: PullRequestBody::try_new(text(&v["body"])?).ok()?,
        labels: array(&v["labels"], |v| LabelName::try_new(text(v)?).ok())?,
        draft: v["draft"].as_bool()?,
        author: optional(v.get("author")?, |v| {
            RepoWatchAuthorLogin::try_new(text(v)?).ok()
        })?,
    }))
}

fn pull_request(v: &Value) -> Option<RepoWatchPullRequestState> {
    RepoWatchPullRequestState::try_new(RepoWatchPullRequestStateInput {
        context: context(&v["context"])?,
        lifecycle: match v["lifecycle"].as_str()? {
            "open" => RepoWatchPullRequestLifecycle::Open,
            "closed" => RepoWatchPullRequestLifecycle::Closed,
            "merged" => RepoWatchPullRequestLifecycle::Merged,
            _ => return None,
        },
        mergeable_state: mergeable(&v["mergeable_state"])?,
        completed_check_suites: array(&v["completed_check_suites"], |v| {
            Some(RepoWatchCheckSuiteObservation::new(
                object_id(&v["id"])?,
                generation(&v["completion_generation"])?,
                match v["outcome"].as_str()? {
                    "success" => ChecksOutcome::Success,
                    "failure" => ChecksOutcome::Failure,
                    _ => return None,
                },
            ))
        })?,
        completed_check_runs: array(&v["completed_check_runs"], |v| {
            Some(RepoWatchCheckRunObservation::new(
                object_id(&v["id"])?,
                generation(&v["completion_generation"])?,
                CheckRunName::try_new(text(&v["name"])?).ok()?,
                conclusion(&v["conclusion"])?,
            ))
        })?,
        reviews: array(&v["reviews"], |v| {
            Some(RepoWatchReviewObservation::new(
                object_id(&v["id"])?,
                RepoWatchAuthorLogin::try_new(text(&v["reviewer"])?).ok()?,
                optional(v.get("state")?, review_state)?,
                CommitSha::try_new(text(&v["commit"])?).ok()?,
            ))
        })?,
        threads: array(&v["threads"], |v| {
            Some(RepoWatchThreadObservation::new(
                ReviewThreadId::try_new(text(&v["thread"])?).ok()?,
                match v["state"].as_str()? {
                    "open" => RepoWatchThreadState::Open,
                    "resolved" => RepoWatchThreadState::Resolved,
                    _ => return None,
                },
            ))
        })?,
        reactions: array(&v["reactions"], |v| {
            Some(RepoWatchReactionObservation::new(
                reaction_subject(&v["subject"])?,
                RepoWatchAuthorLogin::try_new(text(&v["reactor"])?).ok()?,
                ReactionContent::try_new(text(&v["content"])?).ok()?,
            ))
        })?,
    })
    .ok()
}

pub(crate) fn reaction_subject(v: &Value) -> Option<ReactionSubject> {
    Some(match v["kind"].as_str()? {
        "pull_request_body" => ReactionSubject::PullRequestBody,
        "issue_comment" => ReactionSubject::IssueComment {
            id: object_id(&v["id"])?,
        },
        "review_comment" => ReactionSubject::ReviewComment {
            id: object_id(&v["id"])?,
        },
        _ => return None,
    })
}

pub(crate) fn mergeable(v: &Value) -> Option<MergeableState> {
    Some(match v.as_str()? {
        "mergeable" => MergeableState::Mergeable,
        "conflicting" => MergeableState::Conflicting,
        "unknown" => MergeableState::Unknown,
        _ => return None,
    })
}

pub(crate) fn conclusion(v: &Value) -> Option<CheckConclusion> {
    Some(match v.as_str()? {
        "success" => CheckConclusion::Success,
        "failure" => CheckConclusion::Failure,
        "neutral" => CheckConclusion::Neutral,
        "cancelled" => CheckConclusion::Cancelled,
        "skipped" => CheckConclusion::Skipped,
        "timed_out" => CheckConclusion::TimedOut,
        "action_required" => CheckConclusion::ActionRequired,
        "stale" => CheckConclusion::Stale,
        "startup_failure" => CheckConclusion::StartupFailure,
        _ => return None,
    })
}

pub(crate) fn review_state(v: &Value) -> Option<ReviewState> {
    Some(match v.as_str()? {
        "approved" => ReviewState::Approved,
        "changes_requested" => ReviewState::ChangesRequested,
        "commented" => ReviewState::Commented,
        _ => return None,
    })
}

fn generation(v: &Value) -> Option<RepoWatchCheckCompletionGeneration> {
    RepoWatchCheckCompletionGeneration::try_new(text(v)?).ok()
}

pub(crate) fn object_id(v: &Value) -> Option<GitHubObjectId> {
    Some(GitHubObjectId::new(positive(v)?))
}

pub(crate) fn positive(v: &Value) -> Option<NonZeroU64> {
    NonZeroU64::new(v.as_u64()?)
}

pub(crate) fn text(v: &Value) -> Option<String> {
    Some(v.as_str()?.to_owned())
}

pub(crate) fn array<T>(v: &Value, decode: impl Fn(&Value) -> Option<T>) -> Option<Vec<T>> {
    v.as_array()?.iter().map(decode).collect()
}

fn optional<T>(v: &Value, decode: impl FnOnce(&Value) -> Option<T>) -> Option<Option<T>> {
    if v.is_null() {
        Some(None)
    } else {
        Some(Some(decode(v)?))
    }
}

pub(crate) fn merged_baselines(
    value: &Value,
) -> Option<Vec<crate::ingest::MergedPullRequestBaseline>> {
    use signalbox_session_ownership::{
        RepoWatchMergedCheckRunBaselineV1, RepoWatchMergedCheckSuiteBaselineV1,
        RepoWatchMergedPullRequestBaselineInputV1, RepoWatchMergedPullRequestBaselineV1,
    };
    array(&value["merged_pull_requests"], |v| {
        let state = RepoWatchMergedPullRequestBaselineV1::try_new(
            RepoWatchMergedPullRequestBaselineInputV1 {
                head_repository: RepositorySlug::try_new(text(&v["head_repository"])?).ok()?,
                number: PullRequestNumber::new(positive(&v["number"])?),
                head_sha: CommitSha::try_new(text(&v["head_sha"])?).ok()?,
                signal_reviewers: array(&v["signal_reviewers"], |v| {
                    RepoWatchAuthorLogin::try_new(text(v)?).ok()
                })?,
                labels: array(&v["labels"], |v| LabelName::try_new(text(v)?).ok())?,
                mergeable_state: mergeable(&v["mergeable_state"])?,
                completed_check_suites: array(&v["completed_check_suites"], |v| {
                    Some(RepoWatchMergedCheckSuiteBaselineV1::new(
                        object_id(&v["id"])?,
                        generation(&v["completion_generation"])?,
                    ))
                })?,
                completed_check_runs: array(&v["completed_check_runs"], |v| {
                    Some(RepoWatchMergedCheckRunBaselineV1::new(
                        object_id(&v["id"])?,
                        generation(&v["completion_generation"])?,
                        conclusion(&v["conclusion"])?,
                    ))
                })?,
                review_ids: array(&v["review_ids"], object_id)?,
                threads: array(&v["threads"], |v| {
                    Some(RepoWatchThreadObservation::new(
                        ReviewThreadId::try_new(text(&v["thread"])?).ok()?,
                        match v["state"].as_str()? {
                            "open" => RepoWatchThreadState::Open,
                            "resolved" => RepoWatchThreadState::Resolved,
                            _ => return None,
                        },
                    ))
                })?,
                reactions: array(&v["reactions"], |v| {
                    Some(RepoWatchReactionObservation::new(
                        reaction_subject(&v["subject"])?,
                        RepoWatchAuthorLogin::try_new(text(&v["reactor"])?).ok()?,
                        ReactionContent::try_new(text(&v["content"])?).ok()?,
                    ))
                })?,
            },
        )
        .ok()?;
        Some(crate::ingest::MergedPullRequestBaseline {
            state,
            merged_at: signalbox_session_ownership::OffsetDateTime::from_unix_timestamp(
                v["merged_at"].as_i64()?,
            )
            .ok()?,
        })
    })
}
