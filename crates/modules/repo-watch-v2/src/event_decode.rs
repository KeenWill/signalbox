use crate::observation_decode::{
    conclusion, context, mergeable, reaction_subject, review_state, text,
};
use serde_json::Value;
use signalbox_session_ownership::{
    BranchName, CheckRunName, ChecksOutcome, CommitSha, LabelName, ReactionChange, ReactionContent,
    RepoWatchAuthorLogin, RepoWatchEvent, RepoWatchEventId, RepoWatchEventKindV1, RepositorySlug,
    ReviewThreadId, WorkflowName,
};

pub(crate) fn event(id: RepoWatchEventId, bytes: &[u8]) -> Option<RepoWatchEvent> {
    let v: Value = serde_json::from_slice(bytes).ok()?;
    let repository = RepositorySlug::try_new(text(&v["repository"])?).ok()?;
    let k = &v["kind"];
    let kind = match k["name"].as_str()? {
        "pull_request_opened" => RepoWatchEventKindV1::PullRequestOpened,
        "pull_request_closed" => RepoWatchEventKindV1::PullRequestClosed,
        "pull_request_merged" => RepoWatchEventKindV1::PullRequestMerged,
        "head_changed" => RepoWatchEventKindV1::HeadChanged {
            previous: CommitSha::try_new(text(&k["previous"])?).ok()?,
            current: CommitSha::try_new(text(&k["current"])?).ok()?,
        },
        "mergeable_state_changed" => RepoWatchEventKindV1::MergeableStateChanged {
            current: mergeable(&k["current"])?,
        },
        "checks_completed" => RepoWatchEventKindV1::ChecksCompleted {
            outcome: match k["outcome"].as_str()? {
                "success" => ChecksOutcome::Success,
                "failure" => ChecksOutcome::Failure,
                _ => return None,
            },
        },
        "check_run_completed" => RepoWatchEventKindV1::CheckRunCompleted {
            name: CheckRunName::try_new(text(&k["check_run"])?).ok()?,
            conclusion: conclusion(&k["conclusion"])?,
        },
        "branch_workflow_run_completed" => RepoWatchEventKindV1::BranchWorkflowRunCompleted {
            branch: BranchName::try_new(text(&k["branch"])?).ok()?,
            workflow: WorkflowName::try_new(text(&k["workflow"])?).ok()?,
            conclusion: conclusion(&k["conclusion"])?,
        },
        "review_submitted" => RepoWatchEventKindV1::ReviewSubmitted {
            reviewer: RepoWatchAuthorLogin::try_new(text(&k["reviewer"])?).ok()?,
            state: review_state(&k["state"])?,
            commit: CommitSha::try_new(text(&k["commit"])?).ok()?,
        },
        "thread_opened" => RepoWatchEventKindV1::ThreadOpened {
            thread: ReviewThreadId::try_new(text(&k["thread"])?).ok()?,
            author: RepoWatchAuthorLogin::try_new(text(&k["author"])?).ok()?,
        },
        "thread_resolved" => RepoWatchEventKindV1::ThreadResolved {
            thread: ReviewThreadId::try_new(text(&k["thread"])?).ok()?,
            author: RepoWatchAuthorLogin::try_new(text(&k["author"])?).ok()?,
        },
        "labeled" => RepoWatchEventKindV1::Labeled {
            label: LabelName::try_new(text(&k["label"])?).ok()?,
        },
        "unlabeled" => RepoWatchEventKindV1::Unlabeled {
            label: LabelName::try_new(text(&k["label"])?).ok()?,
        },
        "base_advanced" => RepoWatchEventKindV1::BaseAdvanced {
            branch: BranchName::try_new(text(&k["branch"])?).ok()?,
        },
        "reaction_changed" => RepoWatchEventKindV1::ReactionChanged {
            subject: reaction_subject(&k["subject"])?,
            reactor: RepoWatchAuthorLogin::try_new(text(&k["reactor"])?).ok()?,
            content: ReactionContent::try_new(text(&k["content"])?).ok()?,
            change: match k["change"].as_str()? {
                "added" => ReactionChange::Added,
                "removed" => ReactionChange::Removed,
                _ => return None,
            },
        },
        _ => return None,
    };
    match v["target"]["kind"].as_str()? {
        "pull_request" => {
            RepoWatchEvent::try_pull_request(id, repository, context(&v["target"])?, kind).ok()
        }
        "branch" => match kind {
            RepoWatchEventKindV1::BranchWorkflowRunCompleted {
                branch,
                workflow,
                conclusion,
            } => Some(RepoWatchEvent::branch_workflow(
                id, repository, branch, workflow, conclusion,
            )),
            _ => None,
        },
        _ => None,
    }
}
