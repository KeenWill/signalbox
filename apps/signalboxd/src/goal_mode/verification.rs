//! Pull-request completion reads made between goal turns.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::{Value, json};
use signalbox_domain::{CommitSha, GoalGuidance, SessionId};
use signalbox_module_repo_watch_v2::github::GitHubClient;
use signalbox_persistence::goal::{GoalCompletedTool, GoalCompletionCheck, GoalCompletionResult};

use super::{PostgresGoalPassDisposition, PostgresGoalPassDispositionError};

#[derive(Clone, Debug, Deserialize, sqlx::FromRow)]
struct Target {
    repository: String,
    number: String,
    head_repository: String,
    head_branch: String,
}

#[derive(Debug, Deserialize)]
struct Push {
    branch: String,
    commit: String,
}

#[derive(Debug, Deserialize)]
struct Reply {
    repository: String,
    number: u64,
    thread_id: String,
}

#[derive(Debug)]
struct Work {
    commit: CommitSha,
    threads: BTreeSet<String>,
}

fn completed_work(target: &Target, tools: Vec<GoalCompletedTool>) -> Option<Work> {
    let mut commit = None;
    let mut threads = BTreeSet::new();
    for tool in tools {
        match tool.tool_name.as_str() {
            "git_push_configured" => {
                let push: Push = serde_json::from_str(&tool.result_text).ok()?;
                if push.branch == target.head_branch {
                    commit = Some(CommitSha::try_new(push.commit).ok()?);
                }
            }
            "change_request_thread_reply" => {
                let reply: Reply = serde_json::from_str(&tool.arguments_text).ok()?;
                if reply.repository == target.repository
                    && reply.number.to_string() == target.number
                {
                    threads.insert(reply.thread_id);
                }
            }
            _ => {}
        }
    }
    if threads.is_empty() {
        return None;
    }
    Some(Work {
        commit: commit?,
        threads,
    })
}

fn missing(detail: String) -> Result<GoalCompletionResult, ()> {
    GoalGuidance::try_new(detail)
        .map(GoalCompletionResult::Missing)
        .map_err(|_| ())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PushLanding {
    Confirmed,
    Missing,
}

fn assess(
    work: &Work,
    head: CommitSha,
    push: PushLanding,
    resolved: &BTreeSet<String>,
) -> Result<GoalCompletionResult, ()> {
    let contains_commit = push == PushLanding::Confirmed;
    let open: Vec<_> = work.threads.difference(resolved).cloned().collect();
    if contains_commit && open.is_empty() {
        Ok(GoalCompletionResult::Verified {
            head_sha: head,
            resolved_thread_ids: work
                .threads
                .iter()
                .cloned()
                .map(signalbox_domain::ReviewThreadId::try_new)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| ())?
                .into_boxed_slice(),
        })
    } else {
        missing(format!(
            "Finish the remaining pull-request work. Push {} landed on PR head {}: {}. Threads still unresolved or absent: {}.",
            work.commit.as_str(),
            head.as_str(),
            contains_commit,
            if open.is_empty() {
                "none".to_owned()
            } else {
                open.join(", ")
            },
        ))
    }
}

const THREADS: &str = "query($owner:String!,$name:String!,$number:Int!,$cursor:String){repository(owner:$owner,name:$name){pullRequest(number:$number){headRefOid headRefName headRepository{nameWithOwner} reviewThreads(first:100,after:$cursor){nodes{id isResolved} pageInfo{hasNextPage endCursor}}}}}";

async fn observe(
    client: &GitHubClient,
    target: &Target,
    work: &Work,
) -> Result<GoalCompletionResult, ()> {
    let (owner, name) = target.repository.split_once('/').ok_or(())?;
    let number: u64 = target.number.parse().map_err(|_| ())?;
    let mut cursor: Option<String> = None;
    let mut head: Option<CommitSha> = None;
    let mut resolved = BTreeSet::new();
    loop {
        let body = client
            .graphql(
                serde_json::to_vec(&json!({"query": THREADS, "variables": {
                    "owner": owner, "name": name, "number": number, "cursor": cursor
                }}))
                .map_err(|_| ())?,
            )
            .await
            .map_err(|_| ())?;
        let response: Value = serde_json::from_slice(&body).map_err(|_| ())?;
        if response.get("errors").is_some() {
            return Err(());
        }
        let pr = &response["data"]["repository"]["pullRequest"];
        if pr["headRefName"].as_str() != Some(&target.head_branch)
            || pr["headRepository"]["nameWithOwner"].as_str() != Some(&target.head_repository)
        {
            return Err(());
        }
        let observed =
            CommitSha::try_new(pr["headRefOid"].as_str().ok_or(())?.to_owned()).map_err(|_| ())?;
        if head.as_ref().is_some_and(|head| *head != observed) {
            return missing("The pull-request head changed during verification; verify the pushed commit and resolved threads against the new head.".to_owned());
        }
        head = Some(observed);
        for thread in pr["reviewThreads"]["nodes"].as_array().ok_or(())? {
            let id = thread["id"].as_str().ok_or(())?;
            if thread["isResolved"].as_bool().ok_or(())? && work.threads.contains(id) {
                resolved.insert(id.to_owned());
            }
        }
        let page = &pr["reviewThreads"]["pageInfo"];
        if !page["hasNextPage"].as_bool().ok_or(())? {
            break;
        }
        let next = page["endCursor"].as_str().ok_or(())?.to_owned();
        if cursor.as_ref() == Some(&next) {
            return Err(());
        }
        cursor = Some(next);
    }
    let head = head.ok_or(())?;
    let contains = if head == work.commit {
        true
    } else {
        let path = format!(
            "/repos/{}/compare/{}...{}",
            target.head_repository,
            work.commit.as_str(),
            head.as_str()
        );
        let body = client.get(&path).await.map_err(|_| ())?;
        let comparison: Value = serde_json::from_slice(&body).map_err(|_| ())?;
        matches!(comparison["status"].as_str(), Some("ahead" | "identical"))
            && comparison["merge_base_commit"]["sha"].as_str() == Some(work.commit.as_str())
    };
    let body = client
        .get(&format!(
            "/repos/{}/pulls/{}",
            target.repository, target.number
        ))
        .await
        .map_err(|_| ())?;
    let current: Value = serde_json::from_slice(&body).map_err(|_| ())?;
    if current["head"]["sha"].as_str() != Some(head.as_str()) {
        return missing("The pull-request head changed during verification; verify the push and resolved threads against the new head.".to_owned());
    }
    assess(
        work,
        head,
        if contains {
            PushLanding::Confirmed
        } else {
            PushLanding::Missing
        },
        &resolved,
    )
}

impl PostgresGoalPassDisposition {
    pub(super) async fn completion_check(
        &self,
        session: SessionId,
    ) -> Result<Option<GoalCompletionCheck>, PostgresGoalPassDispositionError> {
        let Some(goal) = self.repository.load_goal(session).await? else {
            return Ok(None);
        };
        if !matches!(
            goal.current().state(),
            signalbox_domain::GoalState::Pursuing
        ) || goal.current().generation().get() != 1
        {
            return Ok(None);
        }
        let generation = goal.current().generation();
        let Some(turn) = self
            .repository
            .load_current_goal_turn(session, generation)
            .await?
        else {
            return Ok(None);
        };
        let mut target: Option<Target> = sqlx::query_as(
            "SELECT repository, pull_request_number::text AS number, head_repository, head_branch FROM commissioned_dispatch WHERE session_id = $1 AND target_kind = 'pull_request'"
        ).bind(session.into_uuid()).fetch_optional(&self.pool).await
            .map_err(signalbox_persistence::goal::GoalRepositoryError::Database)?;
        if target.is_none()
            && let Some(reload) = &self.configuration_reload
        {
            let authority = reload
                .goal_dispatch_authority(session)
                .await
                .map_err(|error| match error {
                    signalbox_module_repo_watch_v2::StoreError::Database(error) => {
                        signalbox_persistence::goal::GoalRepositoryError::Database(error)
                    }
                    _ => signalbox_persistence::goal::GoalCorruption::Inconsistent(
                        "goal dispatch authority",
                    )
                    .into(),
                })?;
            if let Some(signalbox_application::ApprovalJudgeDispatchAuthority::PullRequest(pr)) =
                authority
            {
                target = Some(Target {
                    repository: pr.repository().as_str().to_owned(),
                    number: pr.pull_request().get().to_string(),
                    head_repository: pr.head_repository().as_str().to_owned(),
                    head_branch: pr.head_branch().as_str().to_owned(),
                });
            }
        }
        let Some(target) = target else {
            return Ok(None);
        };
        let Some(work) = completed_work(
            &target,
            self.repository
                .completed_goal_tools(session, generation)
                .await?,
        ) else {
            return Ok(None);
        };
        let client = match &self.configuration_reload {
            Some(reload) => reload.goal_github_client(&target.repository).await,
            None => Err(()),
        };
        let observation = match client {
            Ok(client) => observe(&client, &target, &work).await,
            Err(()) => Err(()),
        };
        let result = observation.or_else(|()| missing(format!("GitHub verification unavailable. Confirm pushed commit {} is contained in the pull-request head and resolve these replied threads: {}.", work.commit.as_str(), work.threads.iter().cloned().collect::<Vec<_>>().join(", "))))
            .map_err(|()| PostgresGoalPassDispositionError::InvalidStaticNeed)?;
        Ok(Some(GoalCompletionCheck {
            generation,
            turn,
            result,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Distinct immutable revisions make a stale-head success observable.
    const PUSHED: &str = "1111111111111111111111111111111111111111";
    const NEW_HEAD: &str = "2222222222222222222222222222222222222222";

    fn target() -> Target {
        Target {
            repository: "example/project".to_owned(),
            number: "7".to_owned(),
            head_repository: "example/project".to_owned(),
            head_branch: "fix".to_owned(),
        }
    }

    fn push() -> GoalCompletedTool {
        GoalCompletedTool {
            tool_name: "git_push_configured".to_owned(),
            arguments_text: r#"{"branch":"fix"}"#.to_owned(),
            result_text: json!({"branch":"fix", "commit":PUSHED}).to_string(),
        }
    }

    fn reply() -> GoalCompletedTool {
        GoalCompletedTool { tool_name: "change_request_thread_reply".to_owned(), arguments_text: r#"{"repository":"example/project","number":7,"thread_id":"thread-A","body":"Fixed"}"#.to_owned(), result_text: r#"{"id":"reply-A","url":"https://github.com/example/project/pull/7#discussion_r1"}"#.to_owned() }
    }

    #[test]
    fn goal_achieves_after_a_push_and_resolved_reply() {
        let work = completed_work(&target(), vec![push(), reply()])
            .expect("push and reply trigger verification");
        let head = CommitSha::try_new(NEW_HEAD.to_owned()).unwrap();
        let result = assess(
            &work,
            head.clone(),
            PushLanding::Confirmed,
            &BTreeSet::from(["thread-A".to_owned()]),
        )
        .unwrap();
        assert!(
            matches!(result, GoalCompletionResult::Verified { head_sha, resolved_thread_ids } if head_sha == head && resolved_thread_ids.iter().map(signalbox_domain::ReviewThreadId::as_str).collect::<Vec<_>>() == ["thread-A"])
        );
    }

    #[test]
    fn goal_continuation_names_the_unresolved_thread() {
        let work = completed_work(&target(), vec![push(), reply()]).unwrap();
        let result = assess(
            &work,
            work.commit.clone(),
            PushLanding::Confirmed,
            &BTreeSet::new(),
        )
        .unwrap();
        let GoalCompletionResult::Missing(input) = result else {
            panic!("open thread cannot achieve");
        };
        assert_eq!(
            input.as_str(),
            "Finish the remaining pull-request work. Push 1111111111111111111111111111111111111111 landed on PR head 1111111111111111111111111111111111111111: true. Threads still unresolved or absent: thread-A."
        );
    }

    #[test]
    fn goal_continuation_reports_a_push_missing_from_the_head() {
        let work = completed_work(&target(), vec![push(), reply()]).unwrap();
        let result = assess(
            &work,
            CommitSha::try_new(NEW_HEAD.to_owned()).unwrap(),
            PushLanding::Missing,
            &BTreeSet::from(["thread-A".to_owned()]),
        )
        .unwrap();
        let GoalCompletionResult::Missing(input) = result else {
            panic!("missing push cannot achieve");
        };
        assert_eq!(
            input.as_str(),
            "Finish the remaining pull-request work. Push 1111111111111111111111111111111111111111 landed on PR head 2222222222222222222222222222222222222222: false. Threads still unresolved or absent: none."
        );
    }

    #[test]
    fn goal_without_a_reply_does_not_trigger_verification() {
        assert!(completed_work(&target(), vec![push()]).is_none());
    }

    #[test]
    fn goal_reply_to_another_pull_request_does_not_trigger_verification() {
        let mut other = target();
        other.number = "8".to_owned();
        assert!(completed_work(&other, vec![push(), reply()]).is_none());
    }
}
