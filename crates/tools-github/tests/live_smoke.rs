//! Live read-tool smokes against one long-merged Signalbox pull request.
//!
//! The tests are ignored by default and silently skip when `GITHUB_TOKEN` is
//! absent or empty. They never publish a review. The shared CI smoke job is the
//! automated caller and supplies only the ambient Actions token.

use std::{error::Error, future::Future};

use serde_json::json;
use signalbox_model_runtime::CredentialValue;
use signalbox_tools_github::{
    GitHubApiTransport, GitHubEgressPolicy, GitHubOperation, GitHubResult, GitHubTransport,
    PullRequestArguments,
};

/// Merged PR #94 is the stable public fixture for all three read shapes.
const FIXTURE_REPOSITORY: &str = "KeenWill/signalbox";
const FIXTURE_PULL_REQUEST: u64 = 94;
const FIXTURE_TITLE: &str = "Record owner-ratified interrupt deferral";
const FIXTURE_BODY: &str = r#"## Summary

Record that the matching-Interrupt milestone choice was an owner gate and that the owner ratifies the current nonclaiming behavior for this milestone.

The detailed behavior and first-StopRequested-slice obligation remain owned by the existing Authoritative occupied-slot SubmitInput preparation decision entry; this follow-up links to that source instead of duplicating its normative rule.

## Validation

- repository-wide AGENTS.md validation sequence
- repository-relative documentation links
- GitHub GFM rendering

Meaningfully changed lines: 10 (excluding lockfiles).

Stacked on #93. This replaces #85."#;
const FIXTURE_BASE_REVISION: &str = "6868cd2140708273e0c6879253f3514562eb3db4";
const FIXTURE_HEAD_REVISION: &str = "59af8a3634792a30cfb9480bea08cb04acd17bbf";
const FIXTURE_PATH: &str = "docs/decisions.md";
const FIXTURE_PATCH: &str = concat!(
    r#"@@ -45,6 +45,30 @@ sequence in [AGENTS.md](../AGENTS.md), the documentation checklist in
 [CONTRIBUTING.md](../CONTRIBUTING.md), and `scripts/check_domain_spine.py`.
 Semantics of no document change.
"#,
    " \n",
    r#"+## 2026-07-19 — Owner-ratified matching-interrupt milestone deferral
+
+**Context.** Choosing the milestone outcome for a matching `Interrupt` required
+owner judgment because the current slices cannot construct ADR-0027's
+cancellation, immediate-successor, and applied-proof authority. That choice was
+an owner gate and should have blocked and been reported on the affected
+matching-interrupt track under [goal-mode.md](goal-mode.md), while other
+unblocked work continued, rather than being made within that track.
+
+**Decision.** The owner ratifies the current nonclaiming preparation failure for
+this milestone. The existing “Authoritative occupied-slot SubmitInput
+preparation” entry below remains the single statement of record for that
+behavior and for the required scope of the first `StopRequested` storage slice.
+Future autonomous runs report an equivalent owner gate instead of deciding it.
+
+**Rejected alternatives.** Treating the milestone outcome as permanent would
+conflict with ADR-0027. Repeating its detailed behavior or the `StopRequested`
+obligation here would create a second normative statement. Omitting the gate
+correction would leave the delivery record inaccurate.
+
+**Affects.** The provenance of the existing deferral and future application of
+the blocker rule. It changes no code, schema, ADR, transition, or current
+pull-request behavior.
+
 ## 2026-07-19 — Canonical replay origins include reclassified steering
"#,
    " \n",
    " **Context.** SubmitInput replay used another turn's immutable applied result as",
);
const FIXTURE_THREAD_ID: &str = "PRRT_kwDOTWhy-86SHVJQ";
const FIXTURE_FIRST_COMMENT_ID: &str = "PRRC_kwDOTWhy-87XQNmL";
const FIXTURE_REPLY_ID: &str = "PRRC_kwDOTWhy-87XQXhA";
const FIXTURE_FIRST_COMMENT: &str = r#"**<sub><sub>![P2 Badge](https://img.shields.io/badge/P2-yellow?style=flat)</sub></sub>  Restrict the owner gate to the affected track**

When a goal-mode run has another unblocked track, this incorrectly says the owner gate should have blocked the entire autonomous run, while `docs/goal-mode.md:14-16` requires stopping only the affected track and continuing all other unblocked work. Future runs following this decision record could therefore stop prematurely; describe the gate as blocking and reporting the matching-interrupt track rather than blocking the whole run.

Useful? React with 👍 / 👎."#;
const FIXTURE_REPLY: &str = "Fixed in commit `802400b`. The ratification now says the owner gate blocks and is reported on the affected matching-interrupt track while other unblocked work continues, matching goal-mode scope.";

type SmokeResult = Result<(), Box<dyn Error + Send + Sync>>;

fn fixture_arguments() -> Result<PullRequestArguments, serde_json::Error> {
    serde_json::from_value(json!({
        "repository": FIXTURE_REPOSITORY,
        "number": FIXTURE_PULL_REQUEST,
    }))
}

async fn with_github_token<Smoke, SmokeFuture>(smoke: Smoke) -> SmokeResult
where
    Smoke: FnOnce(CredentialValue) -> SmokeFuture,
    SmokeFuture: Future<Output = SmokeResult>,
{
    let Some(token) = std::env::var_os("GITHUB_TOKEN")
        .and_then(|value| value.into_string().ok())
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    smoke(CredentialValue::new(token.into_bytes())).await
}

async fn execute_read(
    operation: GitHubOperation,
    credential: &CredentialValue,
) -> Result<GitHubResult, Box<dyn Error + Send + Sync>> {
    let mut transport = GitHubApiTransport::try_new()?;
    let result = transport
        .execute(
            operation,
            credential,
            &GitHubEgressPolicy::github_api_only(),
        )
        .await?;
    Ok(result)
}

#[tokio::test]
#[ignore = "uses one real GitHub REST request when GITHUB_TOKEN is present"]
async fn pull_request_metadata_matches_the_merged_fixture() -> SmokeResult {
    with_github_token(|credential| async move {
        let actual =
            execute_read(GitHubOperation::Metadata(fixture_arguments()?), &credential).await?;
        let expected = GitHubResult::metadata(json!({
            "number": FIXTURE_PULL_REQUEST,
            "title": FIXTURE_TITLE,
            "body": FIXTURE_BODY,
            "state": "closed",
            "draft": false,
            "author": "KeenWill",
            "base_ref": "main",
            "base_revision": FIXTURE_BASE_REVISION,
            "head_ref": "agent/interrupt-milestone-ratification-v2",
            "head_revision": FIXTURE_HEAD_REVISION,
            "url": "https://github.com/KeenWill/signalbox/pull/94",
        }));

        assert_eq!(actual, expected);
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "uses three real GitHub REST requests when GITHUB_TOKEN is present"]
async fn pull_request_diff_preserves_the_merged_patch() -> SmokeResult {
    with_github_token(|credential| async move {
        let actual = execute_read(GitHubOperation::Diff(fixture_arguments()?), &credential).await?;
        let expected = GitHubResult::diff(json!({
            "base_revision": FIXTURE_BASE_REVISION,
            "head_revision": FIXTURE_HEAD_REVISION,
            "files": [{
                "path": FIXTURE_PATH,
                "previous_path": null,
                "status": "modified",
                "additions": 24,
                "deletions": 0,
                "changes": 24,
                "patch": FIXTURE_PATCH,
            }],
            "truncated": false,
        }));

        assert_eq!(actual, expected);
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "uses one real GitHub GraphQL request when GITHUB_TOKEN is present"]
async fn pull_request_review_threads_preserve_resolution_and_comments() -> SmokeResult {
    with_github_token(|credential| async move {
        let actual = execute_read(
            GitHubOperation::ReviewThreads(fixture_arguments()?),
            &credential,
        )
        .await?;
        let expected = GitHubResult::review_threads(json!({
            "threads": [{
                "id": FIXTURE_THREAD_ID,
                "resolved": true,
                "outdated": true,
                "path": FIXTURE_PATH,
                "line": null,
                "comments": [{
                    "id": FIXTURE_FIRST_COMMENT_ID,
                    "author": "chatgpt-codex-connector",
                    "body": FIXTURE_FIRST_COMMENT,
                    "created_at": "2026-07-19T21:01:16Z",
                    "url": "https://github.com/KeenWill/signalbox/pull/94#discussion_r3611351435",
                }, {
                    "id": FIXTURE_REPLY_ID,
                    "author": "KeenWill",
                    "body": FIXTURE_REPLY,
                    "created_at": "2026-07-19T21:31:51Z",
                    "url": "https://github.com/KeenWill/signalbox/pull/94#discussion_r3611392064",
                }],
                "comments_truncated": false,
            }],
            "truncated": false,
        }));

        assert_eq!(actual, expected);
        Ok(())
    })
    .await
}
