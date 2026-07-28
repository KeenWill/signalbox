//! Production GitHub REST/GraphQL adapter for the code-host tool suite.

use std::{error::Error, fmt, time::Duration};

use futures_util::StreamExt;
use reqwest::{
    Client, Method, Response, StatusCode, Url,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue, LOCATION, USER_AGENT},
    redirect::Policy,
};
use signalbox_model_runtime::CredentialValue;

use signalbox_tools_basic::{
    PublicDestinationClientError, has_more_response_bytes, public_destination_client,
};

use super::result::absolute_https_url;
use super::review_slog::{
    ReviewerActivity, author_class, authorized_association, disposition_class, finding_title,
    reviewer_verdict_evidence,
};
use super::{
    ChangeRequestCommentResult, ChangeRequestSummaryFields, ChangeRequestSummaryResult,
    ChangedFile, ChangedFilesResult, CheckStatus, ChecksStatusResult, ChildStackState,
    CiJobLogResult, CodeHostChangeRequestNumber, CodeHostCursor, CodeHostOperation,
    CodeHostRepository, CodeHostResult, CodeHostResultCompleteness, CodeHostTransport,
    CodeHostTransportFailure, ConvergenceStateArguments, ConvergenceStateFields,
    ConvergenceStateResult, FilePatchResult, RerunFailedJobsResult, ReviewCheck,
    ReviewDispositionClass, ReviewGateCheckArguments, ReviewGateCheckResult, ReviewThread,
    ReviewThreadComment, ReviewThreadFields, ReviewThreadIdentity, ReviewThreadInventoryFields,
    ReviewThreadInventoryItem, ReviewThreadResolution, ReviewThreadsResult, StackStateArguments,
    StackStateFields, StackStateResult, ThreadInventoryArguments, ThreadInventoryResult,
    ThreadReplyResult, ThreadResolveResult,
};

const REST_BASE_URL: &str = "https://api.github.com/";
const GRAPHQL_URL: &str = "https://api.github.com/graphql";
const USER_AGENT_VALUE: &str = "signalboxd";
const API_VERSION: &str = "2026-03-10";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_JSON_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_JOB_LOG_BYTES: usize = 64 * 1024;
const MAX_REDIRECT_URL_BYTES: usize = 8 * 1024;
const PAGE_SIZE: &str = "100";
const MAX_STACK_COMPARISONS_IN_FLIGHT: usize = 8;

const REVIEW_THREADS_QUERY: &str = r#"
query ReviewThreads($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100) {
        nodes {
          id
          isResolved
          isOutdated
          path
          line
          comments(first: 100) {
            nodes {
              id
              author { login }
              body
              url
            }
            pageInfo { hasNextPage }
          }
        }
        pageInfo { hasNextPage }
      }
    }
  }
}
"#;

const CONVERGENCE_QUERY: &str = r#"
query Convergence($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      headRefOid
      mergeable
      comments(last: 100) {
        nodes { author { login __typename } authorAssociation body createdAt }
        pageInfo { hasPreviousPage startCursor }
      }
      reviews(last: 100) {
        nodes { author { login __typename } authorAssociation body createdAt }
        pageInfo { hasPreviousPage startCursor }
      }
      reviewThreads(first: 100) {
        nodes {
          id isResolved isOutdated path line
          comments(first: 100) {
            nodes { author { login __typename } authorAssociation body }
            pageInfo { hasNextPage }
          }
        }
        pageInfo { hasNextPage endCursor }
      }
      commits(last: 1) {
        nodes {
          commit {
            oid
            statusCheckRollup {
              state
              contexts(first: 100) {
                nodes {
                  __typename
                  ... on CheckRun { name status conclusion }
                  ... on StatusContext { context state }
                }
                pageInfo { hasNextPage endCursor }
              }
            }
          }
        }
      }
    }
  }
}
"#;

const THREAD_INVENTORY_QUERY: &str = r#"
query ThreadInventory($owner: String!, $name: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      headRefOid
      reviewThreads(first: 100, after: $cursor) {
        nodes {
          id isResolved isOutdated path line
          comments(first: 100) {
            nodes { author { login __typename } authorAssociation body }
            pageInfo { hasNextPage }
          }
        }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}
"#;

const STACK_COMPARISON_QUERY: &str = r#"
query StackComparison(
  $owner: String!
  $name: String!
  $baseRef: String!
  $headRevision: String!
) {
  repository(owner: $owner, name: $name) {
    ref(qualifiedName: $baseRef) {
      target { oid }
      compare(headRef: $headRevision) { behindBy }
    }
  }
}
"#;

const STACK_CHILDREN_QUERY: &str = r#"
query StackChildren(
  $owner: String!
  $name: String!
  $baseRef: String!
  $cursor: String
) {
  repository(owner: $owner, name: $name) {
    pullRequests(first: 100, after: $cursor, states: OPEN, baseRefName: $baseRef) {
      nodes { number baseRefName baseRefOid headRefName headRefOid }
      pageInfo { hasNextPage endCursor }
    }
  }
}
"#;

const THREAD_REPLY_MUTATION: &str = r#"
mutation ThreadReply($thread: ID!, $body: String!) {
  addPullRequestReviewThreadReply(input: {pullRequestReviewThreadId: $thread, body: $body}) {
    comment { id url }
  }
}
"#;

const THREAD_RESOLVE_MUTATION: &str = r#"
mutation ThreadResolve($thread: ID!) {
  resolveReviewThread(input: {threadId: $thread}) {
    thread { id isResolved }
  }
}
"#;

/// Production GitHub transport with fixed endpoint and bounded exchange policy.
#[derive(Clone, Debug)]
pub struct GitHubCodeHostTransport {
    client: Client,
    rest_base: Url,
    graphql_url: Url,
}

impl GitHubCodeHostTransport {
    /// Builds the fixed production GitHub transport.
    pub fn try_new() -> Result<Self, GitHubCodeHostConstructionError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = Client::builder()
            .tls_backend_rustls()
            .tls_version_min(reqwest::tls::Version::TLS_1_2)
            .tls_danger_accept_invalid_certs(false)
            .tls_danger_accept_invalid_hostnames(false)
            .no_proxy()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .pool_max_idle_per_host(0)
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(|_| GitHubCodeHostConstructionError)?;
        let rest_base = Url::parse(REST_BASE_URL).map_err(|_| GitHubCodeHostConstructionError)?;
        let graphql_url = Url::parse(GRAPHQL_URL).map_err(|_| GitHubCodeHostConstructionError)?;
        Ok(Self {
            client,
            rest_base,
            graphql_url,
        })
    }

    async fn summary(
        &self,
        arguments: super::ChangeRequestSummaryArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        let url = self.repository_url(
            arguments.repository(),
            &["pulls", &arguments.number().get().to_string()],
            None,
        )?;
        let response = self
            .send_authenticated(Method::GET, url, None, credential)
            .await?;
        let value = self.json_response(response, StatusCode::OK).await?;
        let object = required_object(&value)?;
        let base = required_object(required(object, "base")?)?;
        let head = required_object(required(object, "head")?)?;
        let number: u32 = required_u64(object, "number")?
            .try_into()
            .map_err(|_| CodeHostTransportFailure::InvalidResponse)?;
        if number != arguments.number().get() {
            return Err(CodeHostTransportFailure::InvalidResponse);
        }
        let result = ChangeRequestSummaryResult::try_new(ChangeRequestSummaryFields {
            number,
            title: required_string(object, "title")?,
            body: optional_string(object, "body")?,
            state: required_string(object, "state")?,
            draft: required_bool(object, "draft")?,
            author: optional_object_string(object, "user", "login")?,
            base_ref: required_string(base, "ref")?,
            head_ref: required_string(head, "ref")?,
            head_revision: required_string(head, "sha")?,
            url: required_string(object, "html_url")?,
        })
        .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        Ok(CodeHostResult::Summary(result))
    }

    async fn changed_files(
        &self,
        arguments: super::ChangedFilesArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        let url = self.repository_url(
            arguments.repository(),
            &["pulls", &arguments.number().get().to_string(), "files"],
            Some(&[("per_page", PAGE_SIZE), ("page", "1")]),
        )?;
        let response = self
            .send_authenticated(Method::GET, url, None, credential)
            .await?;
        let (value, completeness) = self.json_page(response, StatusCode::OK).await?;
        let array = value
            .as_array()
            .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        let files = array
            .iter()
            .map(parse_changed_file)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|(file, _patch)| file)
            .collect();
        let result = ChangedFilesResult::try_new(files, completeness)
            .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        Ok(CodeHostResult::ChangedFiles(result))
    }

    async fn file_patch(
        &self,
        arguments: super::FilePatchArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        let url = self.repository_url(
            arguments.repository(),
            &["pulls", &arguments.number().get().to_string(), "files"],
            Some(&[("per_page", PAGE_SIZE), ("page", "1")]),
        )?;
        let response = self
            .send_authenticated(Method::GET, url, None, credential)
            .await?;
        let (value, _completeness) = self.json_page(response, StatusCode::OK).await?;
        let array = value
            .as_array()
            .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        let parsed = array
            .iter()
            .map(parse_changed_file)
            .collect::<Result<Vec<_>, _>>()?;
        let (file, patch) = parsed
            .into_iter()
            .find(|(file, _patch)| file.path() == arguments.path().as_str())
            .ok_or(CodeHostTransportFailure::Rejected)?;
        let result = FilePatchResult::try_new(file, patch)
            .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        Ok(CodeHostResult::FilePatch(result))
    }

    async fn checks_status(
        &self,
        arguments: super::ChecksStatusArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        let url = self.repository_url(
            arguments.repository(),
            &["commits", arguments.revision().as_str(), "check-runs"],
            Some(&[("per_page", PAGE_SIZE), ("page", "1")]),
        )?;
        let response = self
            .send_authenticated(Method::GET, url, None, credential)
            .await?;
        let (value, completeness) = self.json_page(response, StatusCode::OK).await?;
        let object = required_object(&value)?;
        let runs = required(object, "check_runs")?
            .as_array()
            .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        let checks = runs
            .iter()
            .map(parse_check_status)
            .collect::<Result<Vec<_>, _>>()?;
        let result = ChecksStatusResult::try_new(
            arguments.revision().as_str().to_owned(),
            checks,
            completeness,
        )
        .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        Ok(CodeHostResult::ChecksStatus(result))
    }

    async fn comment(
        &self,
        arguments: super::ChangeRequestCommentArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        let url = self.repository_url(
            arguments.repository(),
            &["issues", &arguments.number().get().to_string(), "comments"],
            None,
        )?;
        let body = serde_json::to_vec(&serde_json::json!({"body": arguments.body().as_str()}))
            .map_err(|_| CodeHostTransportFailure::InvalidResponse)?;
        let response = self
            .send_authenticated(Method::POST, url, Some(body), credential)
            .await?;
        let value = self
            .mutation_json_response(response, StatusCode::CREATED)
            .await?;
        let result = (|| {
            let object = required_object(&value)?;
            ChangeRequestCommentResult::try_new(
                required_u64(object, "id")?,
                required_string(object, "html_url")?,
            )
            .ok_or(CodeHostTransportFailure::InvalidResponse)
        })()
        .map_err(|_| CodeHostTransportFailure::DispatchUnknown)?;
        Ok(CodeHostResult::Comment(result))
    }

    async fn review_threads(
        &self,
        arguments: super::ReviewThreadsArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        let body = serde_json::to_vec(&serde_json::json!({
            "query": REVIEW_THREADS_QUERY,
            "variables": {
                "name": arguments.repository().name(),
                "number": arguments.number().get(),
                "owner": arguments.repository().owner(),
            }
        }))
        .map_err(|_| CodeHostTransportFailure::InvalidResponse)?;
        let response = self
            .send_authenticated(
                Method::POST,
                self.graphql_url.clone(),
                Some(body),
                credential,
            )
            .await?;
        let value = self.json_response(response, StatusCode::OK).await?;
        reject_graphql_errors(&value)?;
        let threads = nested(
            &value,
            &["data", "repository", "pullRequest", "reviewThreads"],
        )?;
        let nodes = required_object(threads)?
            .get("nodes")
            .and_then(serde_json::Value::as_array)
            .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        let parsed = nodes
            .iter()
            .map(parse_review_thread)
            .collect::<Result<Vec<_>, _>>()?;
        let completeness = if nested_bool(threads, &["pageInfo", "hasNextPage"])? {
            CodeHostResultCompleteness::Truncated
        } else {
            CodeHostResultCompleteness::Complete
        };
        let result = ReviewThreadsResult::try_new(parsed, completeness)
            .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        Ok(CodeHostResult::ReviewThreads(result))
    }

    async fn convergence_state(
        &self,
        arguments: ConvergenceStateArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        self.convergence_state_for(arguments.repository(), arguments.number(), credential)
            .await
            .map(CodeHostResult::ConvergenceState)
    }

    async fn convergence_state_for(
        &self,
        repository: &CodeHostRepository,
        number: CodeHostChangeRequestNumber,
        credential: &CredentialValue,
    ) -> Result<ConvergenceStateResult, CodeHostTransportFailure> {
        let value = self
            .graphql_read(
                CONVERGENCE_QUERY,
                serde_json::json!({
                    "name": repository.name(),
                    "number": number.get(),
                    "owner": repository.owner(),
                }),
                credential,
            )
            .await?;
        let request = nested(&value, &["data", "repository", "pullRequest"])?;
        let request = required_object(request)?;
        let head_revision = required_string(request, "headRefOid")?;
        let mergeable_state = required_string(request, "mergeable")?;

        let comments = required_object(required(request, "comments")?)?;
        let reviews = required_object(required(request, "reviews")?)?;
        let mut activities = parse_reviewer_activities(comments)?;
        activities.extend(parse_reviewer_activities(reviews)?);
        let (comments_truncated, comments_previous_cursor) = previous_page(comments)?;
        let (reviews_truncated, reviews_previous_cursor) = previous_page(reviews)?;
        let reviewer = reviewer_verdict_evidence(
            &head_revision,
            activities,
            comments_truncated || reviews_truncated,
            comments_previous_cursor,
            reviews_previous_cursor,
        )
        .ok_or(CodeHostTransportFailure::InvalidResponse)?;

        let thread_connection = required_object(required(request, "reviewThreads")?)?;
        let thread_nodes = required(thread_connection, "nodes")?
            .as_array()
            .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        let parsed_threads = thread_nodes
            .iter()
            .map(parse_slog_thread)
            .collect::<Result<Vec<_>, _>>()?;
        let mut unresolved_threads = Vec::new();
        let mut open_escalations = Vec::new();
        let mut buried_escalations = Vec::new();
        let mut undispositioned_threads = Vec::new();
        for thread in parsed_threads {
            if thread.inventory.disposition() == ReviewDispositionClass::Undispositioned {
                undispositioned_threads.push(thread.identity.clone());
            }
            if !thread.resolved {
                unresolved_threads.push(thread.identity.clone());
            }
            if thread.escalated && thread.resolved {
                buried_escalations.push(thread.identity);
            } else if thread.escalated {
                open_escalations.push(thread.identity);
            }
        }
        let (threads_truncated, threads_next_cursor) = next_page(thread_connection)?;

        let commits = required_object(required(request, "commits")?)?;
        let commit_nodes = required(commits, "nodes")?
            .as_array()
            .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        let commit = commit_nodes
            .last()
            .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        let commit = required_object(
            required_object(commit)?
                .get("commit")
                .ok_or(CodeHostTransportFailure::InvalidResponse)?,
        )?;
        if required_string(commit, "oid")? != head_revision {
            return Err(CodeHostTransportFailure::InvalidResponse);
        }
        let (ci_rollup_state, checks, checks_truncated, checks_next_cursor) =
            parse_check_rollup(commit)?;

        ConvergenceStateResult::try_new(ConvergenceStateFields {
            head_revision,
            mergeable_state,
            ci_rollup_state,
            checks,
            checks_truncated,
            checks_next_cursor,
            unresolved_threads,
            open_escalations,
            buried_escalations,
            threads_truncated,
            undispositioned_threads,
            threads_next_cursor,
            reviewer,
        })
        .ok_or(CodeHostTransportFailure::InvalidResponse)
    }

    async fn thread_inventory(
        &self,
        arguments: ThreadInventoryArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        self.thread_inventory_for(
            arguments.repository(),
            arguments.number(),
            arguments.cursor(),
            credential,
        )
        .await
        .map(CodeHostResult::ThreadInventory)
    }

    async fn thread_inventory_for(
        &self,
        repository: &CodeHostRepository,
        number: CodeHostChangeRequestNumber,
        cursor: Option<&CodeHostCursor>,
        credential: &CredentialValue,
    ) -> Result<ThreadInventoryResult, CodeHostTransportFailure> {
        let value = self
            .graphql_read(
                THREAD_INVENTORY_QUERY,
                serde_json::json!({
                    "cursor": cursor.map(CodeHostCursor::as_str),
                    "name": repository.name(),
                    "number": number.get(),
                    "owner": repository.owner(),
                }),
                credential,
            )
            .await?;
        let request = required_object(nested(&value, &["data", "repository", "pullRequest"])?)?;
        let head_revision = required_string(request, "headRefOid")?;
        let connection = required_object(required(request, "reviewThreads")?)?;
        let nodes = required(connection, "nodes")?
            .as_array()
            .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        let threads = nodes
            .iter()
            .map(parse_slog_thread)
            .map(|parsed| parsed.map(|thread| thread.inventory))
            .collect::<Result<Vec<_>, _>>()?;
        let (truncated, next_cursor) = next_page(connection)?;
        ThreadInventoryResult::try_new(head_revision, threads, truncated, next_cursor)
            .ok_or(CodeHostTransportFailure::InvalidResponse)
    }

    async fn stack_state(
        &self,
        arguments: StackStateArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        self.stack_state_for(
            arguments.repository(),
            arguments.number(),
            arguments.cursor(),
            credential,
        )
        .await
        .map(CodeHostResult::StackState)
    }

    async fn stack_state_for(
        &self,
        repository: &CodeHostRepository,
        number: CodeHostChangeRequestNumber,
        child_cursor: Option<&CodeHostCursor>,
        credential: &CredentialValue,
    ) -> Result<StackStateResult, CodeHostTransportFailure> {
        tokio::time::timeout(
            DEFAULT_TIMEOUT,
            self.stack_state_transaction(repository, number, child_cursor, credential),
        )
        .await
        .map_err(|_| CodeHostTransportFailure::DispatchUnknown)?
    }

    async fn stack_state_transaction(
        &self,
        repository: &CodeHostRepository,
        number: CodeHostChangeRequestNumber,
        child_cursor: Option<&CodeHostCursor>,
        credential: &CredentialValue,
    ) -> Result<StackStateResult, CodeHostTransportFailure> {
        let request_url =
            self.repository_url(repository, &["pulls", &number.get().to_string()], None)?;
        let request_value = self.get_json(request_url.clone(), credential).await?;
        let request = parse_stack_request(&request_value, number.get())?;

        let base_branch_url =
            self.repository_url(repository, &["branches", request.base_ref.as_str()], None)?;
        let default_branch_url = self.repository_url(
            repository,
            &["branches", request.default_ref.as_str()],
            None,
        )?;
        let (base_branch_value, default_branch_value) = tokio::try_join!(
            self.get_json(base_branch_url.clone(), credential),
            self.get_json(default_branch_url.clone(), credential),
        )?;
        let base_revision = parse_branch_revision(&base_branch_value)?;
        let default_revision = parse_branch_revision(&default_branch_value)?;

        let (base_commits_not_in_head, main_commits_not_in_base, main_commits_not_in_child_base) =
            tokio::try_join!(
                self.compare_behind_by(
                    repository,
                    request.base_ref.as_str(),
                    base_revision.as_str(),
                    request.head_revision.as_str(),
                    credential,
                ),
                self.compare_behind_by(
                    repository,
                    request.default_ref.as_str(),
                    default_revision.as_str(),
                    base_revision.as_str(),
                    credential,
                ),
                self.compare_behind_by(
                    repository,
                    request.default_ref.as_str(),
                    default_revision.as_str(),
                    request.head_revision.as_str(),
                    credential,
                ),
            )?;

        let (child_snapshot, children_truncated, children_next_cursor) = self
            .stack_children_page(
                &request.head_repository,
                request.head_ref.as_str(),
                child_cursor,
                credential,
            )
            .await?;
        let parent_head_ref = request.head_ref.as_str();
        let parent_head_revision = request.head_revision.as_str();
        let head_repository = &request.head_repository;
        let mut indexed_children =
            futures_util::stream::iter(child_snapshot.iter().cloned().enumerate())
                .map(|(index, child)| async move {
                    if child.base_ref != parent_head_ref {
                        return Err(CodeHostTransportFailure::InvalidResponse);
                    }
                    let child_base_commits_not_in_head = self
                        .compare_behind_by(
                            head_repository,
                            parent_head_ref,
                            parent_head_revision,
                            child.head_revision.as_str(),
                            credential,
                        )
                        .await?;
                    let state = ChildStackState::try_new(
                        child.number,
                        child.head_ref,
                        child.head_revision,
                        child_base_commits_not_in_head,
                        main_commits_not_in_child_base,
                    )
                    .ok_or(CodeHostTransportFailure::InvalidResponse)?;
                    Ok((index, state))
                })
                .buffer_unordered(MAX_STACK_COMPARISONS_IN_FLIGHT)
                .collect::<Vec<Result<_, _>>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;
        indexed_children.sort_by_key(|(index, _)| *index);
        let children = indexed_children
            .into_iter()
            .map(|(_, child)| child)
            .collect();

        let (
            current_request_value,
            current_base_branch_value,
            current_default_branch_value,
            current_children,
        ) = tokio::try_join!(
            self.get_json(request_url, credential),
            self.get_json(base_branch_url, credential),
            self.get_json(default_branch_url, credential),
            self.stack_children_page(
                &request.head_repository,
                request.head_ref.as_str(),
                child_cursor,
                credential,
            ),
        )?;
        let current_request = parse_stack_request(&current_request_value, number.get())?;
        let current_base_revision = parse_branch_revision(&current_base_branch_value)?;
        let current_default_revision = parse_branch_revision(&current_default_branch_value)?;
        let (current_child_snapshot, current_children_truncated, current_children_next_cursor) =
            current_children;
        ensure_stack_snapshot_unchanged(
            StackSnapshot {
                request: &request,
                base_revision: base_revision.as_str(),
                default_revision: default_revision.as_str(),
                children: &child_snapshot,
                children_truncated,
                children_next_cursor: children_next_cursor.as_deref(),
            },
            StackSnapshot {
                request: &current_request,
                base_revision: current_base_revision.as_str(),
                default_revision: current_default_revision.as_str(),
                children: &current_child_snapshot,
                children_truncated: current_children_truncated,
                children_next_cursor: current_children_next_cursor.as_deref(),
            },
        )?;
        StackStateResult::try_new(StackStateFields {
            number: number.get(),
            base_ref: request.base_ref,
            base_revision,
            head_ref: request.head_ref,
            head_revision: request.head_revision,
            default_ref: request.default_ref,
            default_revision,
            base_commits_not_in_head,
            main_commits_not_in_base,
            children,
            children_truncated,
            children_next_cursor,
        })
        .ok_or(CodeHostTransportFailure::InvalidResponse)
    }

    async fn stack_children_page(
        &self,
        repository: &CodeHostRepository,
        base_ref: &str,
        cursor: Option<&CodeHostCursor>,
        credential: &CredentialValue,
    ) -> Result<(Vec<StackChildFacts>, bool, Option<String>), CodeHostTransportFailure> {
        let (owner, name) = repository
            .as_str()
            .split_once('/')
            .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        let value = self
            .graphql_read(
                STACK_CHILDREN_QUERY,
                serde_json::json!({
                    "baseRef": base_ref,
                    "cursor": cursor.map(CodeHostCursor::as_str),
                    "name": name,
                    "owner": owner,
                }),
                credential,
            )
            .await?;
        let connection = required_object(nested(&value, &["data", "repository", "pullRequests"])?)?;
        let children = required(connection, "nodes")?
            .as_array()
            .ok_or(CodeHostTransportFailure::InvalidResponse)?
            .iter()
            .map(parse_stack_child)
            .collect::<Result<Vec<_>, _>>()?;
        let (truncated, next_cursor) = next_page(connection)?;
        Ok((children, truncated, next_cursor))
    }

    async fn review_gate_check(
        &self,
        arguments: ReviewGateCheckArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        tokio::time::timeout(
            DEFAULT_TIMEOUT,
            self.review_gate_transaction(arguments, credential),
        )
        .await
        .map_err(|_| CodeHostTransportFailure::DispatchUnknown)?
    }

    async fn review_gate_transaction(
        &self,
        arguments: ReviewGateCheckArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        let initial_stack = self
            .stack_state_for(arguments.repository(), arguments.number(), None, credential)
            .await?;
        let inventory = self
            .thread_inventory_for(arguments.repository(), arguments.number(), None, credential)
            .await?;
        let convergence = self
            .convergence_state_for(arguments.repository(), arguments.number(), credential)
            .await?;
        let stack = self
            .stack_state_for(arguments.repository(), arguments.number(), None, credential)
            .await?;
        if initial_stack != stack {
            return Err(CodeHostTransportFailure::InvalidResponse);
        }
        Ok(CodeHostResult::ReviewGateCheck(
            ReviewGateCheckResult::compose(arguments.purpose(), &convergence, &stack, &inventory),
        ))
    }

    async fn compare_behind_by(
        &self,
        repository: &CodeHostRepository,
        base_ref: &str,
        base_revision: &str,
        head_revision: &str,
        credential: &CredentialValue,
    ) -> Result<u64, CodeHostTransportFailure> {
        let (owner, name) = repository
            .as_str()
            .split_once('/')
            .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        let value = self
            .graphql_read(
                STACK_COMPARISON_QUERY,
                serde_json::json!({
                    "baseRef": format!("refs/heads/{base_ref}"),
                    "headRevision": head_revision,
                    "name": name,
                    "owner": owner,
                }),
                credential,
            )
            .await?;
        parse_stack_comparison(&value, base_revision)
    }

    async fn get_json(
        &self,
        url: Url,
        credential: &CredentialValue,
    ) -> Result<serde_json::Value, CodeHostTransportFailure> {
        let response = self
            .send_authenticated(Method::GET, url, None, credential)
            .await?;
        self.json_response(response, StatusCode::OK).await
    }

    async fn graphql_read(
        &self,
        query: &str,
        variables: serde_json::Value,
        credential: &CredentialValue,
    ) -> Result<serde_json::Value, CodeHostTransportFailure> {
        let body = serde_json::to_vec(&serde_json::json!({
            "query": query,
            "variables": variables,
        }))
        .map_err(|_| CodeHostTransportFailure::InvalidResponse)?;
        let response = self
            .send_authenticated(
                Method::POST,
                self.graphql_url.clone(),
                Some(body),
                credential,
            )
            .await?;
        let value = self.json_response(response, StatusCode::OK).await?;
        reject_graphql_errors(&value)?;
        Ok(value)
    }

    async fn thread_reply(
        &self,
        arguments: super::ThreadReplyArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        let body = serde_json::to_vec(&serde_json::json!({
            "query": THREAD_REPLY_MUTATION,
            "variables": {
                "body": arguments.body().as_str(),
                "thread": arguments.thread_id().as_str(),
            }
        }))
        .map_err(|_| CodeHostTransportFailure::InvalidResponse)?;
        let response = self
            .send_authenticated(
                Method::POST,
                self.graphql_url.clone(),
                Some(body),
                credential,
            )
            .await?;
        let value = self
            .mutation_json_response(response, StatusCode::OK)
            .await?;
        reject_graphql_mutation_errors(&value)?;
        let result = (|| {
            let comment = nested(
                &value,
                &["data", "addPullRequestReviewThreadReply", "comment"],
            )?;
            let object = required_object(comment)?;
            ThreadReplyResult::try_new(
                required_string(object, "id")?,
                required_string(object, "url")?,
            )
            .ok_or(CodeHostTransportFailure::InvalidResponse)
        })()
        .map_err(|_| CodeHostTransportFailure::DispatchUnknown)?;
        Ok(CodeHostResult::ThreadReply(result))
    }

    async fn thread_resolve(
        &self,
        arguments: super::ThreadResolveArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        let body = serde_json::to_vec(&serde_json::json!({
            "query": THREAD_RESOLVE_MUTATION,
            "variables": {"thread": arguments.thread_id().as_str()}
        }))
        .map_err(|_| CodeHostTransportFailure::InvalidResponse)?;
        let response = self
            .send_authenticated(
                Method::POST,
                self.graphql_url.clone(),
                Some(body),
                credential,
            )
            .await?;
        let value = self
            .mutation_json_response(response, StatusCode::OK)
            .await?;
        reject_graphql_mutation_errors(&value)?;
        let result = (|| {
            let thread = nested(&value, &["data", "resolveReviewThread", "thread"])?;
            let object = required_object(thread)?;
            let returned_id = required_string(object, "id")?;
            if returned_id != arguments.thread_id().as_str() {
                return Err(CodeHostTransportFailure::InvalidResponse);
            }
            let resolution = if required_bool(object, "isResolved")? {
                ReviewThreadResolution::Resolved
            } else {
                ReviewThreadResolution::Open
            };
            ThreadResolveResult::try_new(returned_id, resolution)
                .ok_or(CodeHostTransportFailure::InvalidResponse)
        })()
        .map_err(|_| CodeHostTransportFailure::DispatchUnknown)?;
        Ok(CodeHostResult::ThreadResolve(result))
    }

    async fn ci_job_log(
        &self,
        arguments: super::CiJobLogArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        let started = tokio::time::Instant::now();
        let url = self.repository_url(
            arguments.repository(),
            &["actions", "jobs", &arguments.job_id().to_string(), "logs"],
            None,
        )?;
        let response = self
            .send_authenticated(Method::GET, url, None, credential)
            .await?;
        ensure_expected_status(response.status(), StatusCode::FOUND)?;
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.len() <= MAX_REDIRECT_URL_BYTES)
            .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        let redirect =
            Url::parse(location).map_err(|_| CodeHostTransportFailure::InvalidResponse)?;
        if !absolute_https_url(&redirect) || redirect.fragment().is_some() {
            return Err(CodeHostTransportFailure::InvalidResponse);
        }
        let remaining = remaining_exchange_timeout(started.elapsed())?;
        let redirect_client = public_destination_client(&redirect, remaining)
            .await
            .map_err(classify_public_destination_error)?;
        let response = redirect_client
            .get(redirect)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .send()
            .await
            .map_err(|_| CodeHostTransportFailure::DispatchUnknown)?;
        ensure_expected_status(response.status(), StatusCode::OK)?;
        let (bytes, completeness) =
            read_bounded(response.bytes_stream(), MAX_JOB_LOG_BYTES).await?;
        let (text, completeness) = bounded_lossy_text(&bytes, completeness);
        let result = CiJobLogResult::try_new(arguments.job_id(), text, completeness)
            .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        Ok(CodeHostResult::CiJobLog(result))
    }

    async fn rerun_failed_jobs(
        &self,
        arguments: super::RerunFailedJobsArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        let url = self.repository_url(
            arguments.repository(),
            &[
                "actions",
                "runs",
                &arguments.run_id().to_string(),
                "rerun-failed-jobs",
            ],
            None,
        )?;
        let response = self
            .send_authenticated(Method::POST, url, None, credential)
            .await?;
        if response.status() == StatusCode::CREATED {
            let result = RerunFailedJobsResult::try_new(arguments.run_id())
                .ok_or(CodeHostTransportFailure::InvalidResponse)?;
            return Ok(CodeHostResult::RerunFailedJobs(result));
        }
        if response.status().is_client_error() {
            Err(CodeHostTransportFailure::Rejected)
        } else {
            Err(CodeHostTransportFailure::DispatchUnknown)
        }
    }

    fn repository_url(
        &self,
        repository: &super::CodeHostRepository,
        suffix: &[&str],
        query: Option<&[(&str, &str)]>,
    ) -> Result<Url, CodeHostTransportFailure> {
        let mut url = self.rest_base.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| CodeHostTransportFailure::InvalidResponse)?;
            segments.pop_if_empty();
            segments.push("repos");
            segments.push(repository.owner());
            segments.push(repository.name());
            segments.extend(suffix.iter().copied());
        }
        if let Some(query) = query {
            url.query_pairs_mut().extend_pairs(query.iter().copied());
        }
        Ok(url)
    }

    async fn send_authenticated(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        credential: &CredentialValue,
    ) -> Result<Response, CodeHostTransportFailure> {
        let mut authentication = Vec::with_capacity(7 + credential.expose_bytes().len());
        authentication.extend_from_slice(b"Bearer ");
        authentication.extend_from_slice(credential.expose_bytes());
        let mut authentication = HeaderValue::from_bytes(&authentication)
            .map_err(|_| CodeHostTransportFailure::InvalidCredential)?;
        authentication.set_sensitive(true);
        let mut request = self
            .client
            .request(method, url)
            .header(AUTHORIZATION, authentication)
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header(USER_AGENT, USER_AGENT_VALUE);
        if let Some(body) = body {
            request = request.header(CONTENT_TYPE, "application/json").body(body);
        }
        request
            .send()
            .await
            .map_err(|_| CodeHostTransportFailure::DispatchUnknown)
    }

    async fn json_response(
        &self,
        response: Response,
        expected: StatusCode,
    ) -> Result<serde_json::Value, CodeHostTransportFailure> {
        self.json_page(response, expected)
            .await
            .map(|(value, _completeness)| value)
    }

    async fn mutation_json_response(
        &self,
        response: Response,
        expected: StatusCode,
    ) -> Result<serde_json::Value, CodeHostTransportFailure> {
        ensure_expected_status(response.status(), expected)?;
        self.json_page(response, expected)
            .await
            .map(|(value, _completeness)| value)
            .map_err(|failure| match failure {
                CodeHostTransportFailure::InvalidCredential
                | CodeHostTransportFailure::Rejected => failure,
                CodeHostTransportFailure::InvalidResponse
                | CodeHostTransportFailure::ResponseTooLarge
                | CodeHostTransportFailure::DispatchUnknown => {
                    CodeHostTransportFailure::DispatchUnknown
                }
            })
    }

    async fn json_page(
        &self,
        response: Response,
        expected: StatusCode,
    ) -> Result<(serde_json::Value, CodeHostResultCompleteness), CodeHostTransportFailure> {
        ensure_expected_status(response.status(), expected)?;
        let completeness = if response
            .headers()
            .get(reqwest::header::LINK)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.split(',').any(|link| link.contains("rel=\"next\"")))
        {
            CodeHostResultCompleteness::Truncated
        } else {
            CodeHostResultCompleteness::Complete
        };
        let (body, body_completeness) =
            read_bounded(response.bytes_stream(), MAX_JSON_RESPONSE_BYTES).await?;
        if body_completeness == CodeHostResultCompleteness::Truncated {
            return Err(CodeHostTransportFailure::ResponseTooLarge);
        }
        let value =
            serde_json::from_slice(&body).map_err(|_| CodeHostTransportFailure::InvalidResponse)?;
        Ok((value, completeness))
    }
}

impl CodeHostTransport for GitHubCodeHostTransport {
    async fn execute(
        &mut self,
        operation: CodeHostOperation,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        match operation {
            CodeHostOperation::Summary(arguments) => self.summary(arguments, credential).await,
            CodeHostOperation::ChangedFiles(arguments) => {
                self.changed_files(arguments, credential).await
            }
            CodeHostOperation::FilePatch(arguments) => self.file_patch(arguments, credential).await,
            CodeHostOperation::ChecksStatus(arguments) => {
                self.checks_status(arguments, credential).await
            }
            CodeHostOperation::Comment(arguments) => self.comment(arguments, credential).await,
            CodeHostOperation::ConvergenceState(arguments) => {
                self.convergence_state(arguments, credential).await
            }
            CodeHostOperation::ReviewThreads(arguments) => {
                self.review_threads(arguments, credential).await
            }
            CodeHostOperation::StackState(arguments) => {
                self.stack_state(arguments, credential).await
            }
            CodeHostOperation::ThreadInventory(arguments) => {
                self.thread_inventory(arguments, credential).await
            }
            CodeHostOperation::ThreadReply(arguments) => {
                self.thread_reply(arguments, credential).await
            }
            CodeHostOperation::ThreadResolve(arguments) => {
                self.thread_resolve(arguments, credential).await
            }
            CodeHostOperation::CiJobLog(arguments) => self.ci_job_log(arguments, credential).await,
            CodeHostOperation::RerunFailedJobs(arguments) => {
                self.rerun_failed_jobs(arguments, credential).await
            }
            CodeHostOperation::ReviewGateCheck(arguments) => {
                self.review_gate_check(arguments, credential).await
            }
        }
    }
}

/// The fixed GitHub client or endpoint could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GitHubCodeHostConstructionError;

impl fmt::Display for GitHubCodeHostConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GitHub code-host transport construction failed")
    }
}

impl Error for GitHubCodeHostConstructionError {}

async fn read_bounded<S, B, E>(
    mut stream: S,
    limit: usize,
) -> Result<(Vec<u8>, CodeHostResultCompleteness), CodeHostTransportFailure>
where
    S: futures_util::Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
{
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| CodeHostTransportFailure::DispatchUnknown)?;
        let chunk = chunk.as_ref();
        let remaining = limit.saturating_sub(body.len());
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            return Ok((body, CodeHostResultCompleteness::Truncated));
        }
        body.extend_from_slice(chunk);
        if body.len() == limit {
            let completeness = if has_more_response_bytes(&mut stream)
                .await
                .map_err(|_| CodeHostTransportFailure::DispatchUnknown)?
            {
                CodeHostResultCompleteness::Truncated
            } else {
                CodeHostResultCompleteness::Complete
            };
            return Ok((body, completeness));
        }
    }
    Ok((body, CodeHostResultCompleteness::Complete))
}

fn ensure_expected_status(
    status: StatusCode,
    expected: StatusCode,
) -> Result<(), CodeHostTransportFailure> {
    if status == expected {
        Ok(())
    } else if status.is_client_error() {
        Err(CodeHostTransportFailure::Rejected)
    } else {
        Err(CodeHostTransportFailure::DispatchUnknown)
    }
}

const fn classify_public_destination_error(
    failure: PublicDestinationClientError,
) -> CodeHostTransportFailure {
    match failure {
        PublicDestinationClientError::DestinationRejected => {
            CodeHostTransportFailure::InvalidResponse
        }
        PublicDestinationClientError::Infrastructure => CodeHostTransportFailure::DispatchUnknown,
    }
}

fn bounded_lossy_text(
    bytes: &[u8],
    completeness: CodeHostResultCompleteness,
) -> (String, CodeHostResultCompleteness) {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if text.len() <= MAX_JOB_LOG_BYTES {
        return (text, completeness);
    }
    let mut boundary = MAX_JOB_LOG_BYTES;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
    (text, CodeHostResultCompleteness::Truncated)
}

fn parse_changed_file(
    value: &serde_json::Value,
) -> Result<(ChangedFile, Option<String>), CodeHostTransportFailure> {
    let object = required_object(value)?;
    let file = ChangedFile::try_new(
        required_string(object, "filename")?,
        required_string(object, "status")?,
        required_u64(object, "additions")?,
        required_u64(object, "deletions")?,
    )
    .ok_or(CodeHostTransportFailure::InvalidResponse)?;
    let patch = omitted_optional_string(object, "patch")?;
    Ok((file, patch))
}

fn parse_check_status(value: &serde_json::Value) -> Result<CheckStatus, CodeHostTransportFailure> {
    let object = required_object(value)?;
    CheckStatus::try_new(
        required_u64(object, "id")?,
        required_string(object, "name")?,
        required_string(object, "status")?,
        optional_string(object, "conclusion")?,
        required_string(object, "html_url")?,
    )
    .ok_or(CodeHostTransportFailure::InvalidResponse)
}

fn parse_review_thread(
    value: &serde_json::Value,
) -> Result<ReviewThread, CodeHostTransportFailure> {
    let object = required_object(value)?;
    let comments = required_object(required(object, "comments")?)?;
    let comment_nodes = required(comments, "nodes")?
        .as_array()
        .ok_or(CodeHostTransportFailure::InvalidResponse)?;
    let comments = comment_nodes
        .iter()
        .map(parse_review_thread_comment)
        .collect::<Result<Vec<_>, _>>()?;
    ReviewThread::try_new(ReviewThreadFields {
        id: required_string(object, "id")?,
        resolved: required_bool(object, "isResolved")?,
        outdated: required_bool(object, "isOutdated")?,
        path: required_string(object, "path")?,
        line: optional_u64(object, "line")?,
        comments,
        comments_truncated: nested_bool(
            required(object, "comments")?,
            &["pageInfo", "hasNextPage"],
        )?,
    })
    .ok_or(CodeHostTransportFailure::InvalidResponse)
}

fn parse_review_thread_comment(
    value: &serde_json::Value,
) -> Result<ReviewThreadComment, CodeHostTransportFailure> {
    let object = required_object(value)?;
    let author = object
        .get("author")
        .filter(|value| !value.is_null())
        .map(required_object)
        .transpose()?
        .map(|author| required_string(author, "login"))
        .transpose()?;
    ReviewThreadComment::try_new(
        required_string(object, "id")?,
        author,
        required_string(object, "body")?,
        required_string(object, "url")?,
    )
    .ok_or(CodeHostTransportFailure::InvalidResponse)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedSlogThread {
    identity: ReviewThreadIdentity,
    inventory: ReviewThreadInventoryItem,
    resolved: bool,
    escalated: bool,
}

fn parse_slog_thread(
    value: &serde_json::Value,
) -> Result<ParsedSlogThread, CodeHostTransportFailure> {
    let object = required_object(value)?;
    let comments = required_object(required(object, "comments")?)?;
    if nested_bool(required(object, "comments")?, &["pageInfo", "hasNextPage"])? {
        return Err(CodeHostTransportFailure::InvalidResponse);
    }
    let comment_nodes = required(comments, "nodes")?
        .as_array()
        .ok_or(CodeHostTransportFailure::InvalidResponse)?;
    let first = comment_nodes
        .first()
        .ok_or(CodeHostTransportFailure::InvalidResponse)?;
    let first = required_object(first)?;
    let first_body = required_string(first, "body")?;
    let reply_evidence = comment_nodes
        .iter()
        .skip(1)
        .map(required_object)
        .map(|comment| {
            comment.and_then(|comment| {
                Ok((
                    required_string(comment, "body")?,
                    authorized_association(&required_string(comment, "authorAssociation")?),
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let reply_evidence = reply_evidence
        .iter()
        .map(|(body, authorized)| (body.as_str(), *authorized))
        .collect::<Vec<_>>();
    let title = finding_title(&first_body);
    let author_value = required(first, "author")?;
    let (author, actor_type) = match author_value {
        serde_json::Value::Null => (None, None),
        serde_json::Value::Object(author) => (
            Some(required_string(author, "login")?),
            Some(required_string(author, "__typename")?),
        ),
        _ => return Err(CodeHostTransportFailure::InvalidResponse),
    };
    let id = required_string(object, "id")?;
    let path = required_string(object, "path")?;
    let resolved = required_bool(object, "isResolved")?;
    let disposition = disposition_class(&reply_evidence);
    let identity = ReviewThreadIdentity::try_new(id.clone(), path.clone(), title.clone())
        .ok_or(CodeHostTransportFailure::InvalidResponse)?;
    let inventory = ReviewThreadInventoryItem::try_new(ReviewThreadInventoryFields {
        id,
        path,
        line: optional_u64(object, "line")?,
        resolved,
        outdated: required_bool(object, "isOutdated")?,
        author,
        author_class: author_class(actor_type.as_deref()),
        finding_title: title,
        disposition,
    })
    .ok_or(CodeHostTransportFailure::InvalidResponse)?;
    Ok(ParsedSlogThread {
        identity,
        inventory,
        resolved,
        escalated: disposition == ReviewDispositionClass::EscalationMarker,
    })
}

fn parse_reviewer_activities(
    connection: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<ReviewerActivity>, CodeHostTransportFailure> {
    required(connection, "nodes")?
        .as_array()
        .ok_or(CodeHostTransportFailure::InvalidResponse)?
        .iter()
        .map(|value| {
            let object = required_object(value)?;
            let (author, actor_type) = match required(object, "author")? {
                serde_json::Value::Null => (None, None),
                serde_json::Value::Object(author) => (
                    Some(required_string(author, "login")?),
                    Some(required_string(author, "__typename")?),
                ),
                _ => return Err(CodeHostTransportFailure::InvalidResponse),
            };
            Ok(ReviewerActivity {
                author,
                author_association: required_string(object, "authorAssociation")?,
                actor_type,
                body: required_string(object, "body")?,
                created_at: required_string(object, "createdAt")?,
            })
        })
        .collect()
}

fn next_page(
    connection: &serde_json::Map<String, serde_json::Value>,
) -> Result<(bool, Option<String>), CodeHostTransportFailure> {
    page(connection, "hasNextPage", "endCursor")
}

fn previous_page(
    connection: &serde_json::Map<String, serde_json::Value>,
) -> Result<(bool, Option<String>), CodeHostTransportFailure> {
    page(connection, "hasPreviousPage", "startCursor")
}

fn page(
    connection: &serde_json::Map<String, serde_json::Value>,
    truncated_member: &str,
    cursor_member: &str,
) -> Result<(bool, Option<String>), CodeHostTransportFailure> {
    let page_info = required_object(required(connection, "pageInfo")?)?;
    let truncated = required_bool(page_info, truncated_member)?;
    let cursor = optional_string(page_info, cursor_member)?;
    if truncated {
        return cursor
            .map(|cursor| (true, Some(cursor)))
            .ok_or(CodeHostTransportFailure::InvalidResponse);
    }
    Ok((false, None))
}

type CheckRollup = (Option<String>, Vec<ReviewCheck>, bool, Option<String>);

fn parse_check_rollup(
    commit: &serde_json::Map<String, serde_json::Value>,
) -> Result<CheckRollup, CodeHostTransportFailure> {
    let Some(rollup) = commit.get("statusCheckRollup") else {
        return Err(CodeHostTransportFailure::InvalidResponse);
    };
    let serde_json::Value::Object(rollup) = rollup else {
        if rollup.is_null() {
            return Ok((None, Vec::new(), false, None));
        }
        return Err(CodeHostTransportFailure::InvalidResponse);
    };
    let state = required_string(rollup, "state")?;
    let contexts = required_object(required(rollup, "contexts")?)?;
    let checks = required(contexts, "nodes")?
        .as_array()
        .ok_or(CodeHostTransportFailure::InvalidResponse)?
        .iter()
        .map(parse_rollup_context)
        .collect::<Result<Vec<_>, _>>()?;
    let (truncated, cursor) = next_page(contexts)?;
    Ok((Some(state), checks, truncated, cursor))
}

fn parse_rollup_context(
    value: &serde_json::Value,
) -> Result<ReviewCheck, CodeHostTransportFailure> {
    let object = required_object(value)?;
    match required_string(object, "__typename")?.as_str() {
        "CheckRun" => ReviewCheck::try_new(
            required_string(object, "name")?,
            required_string(object, "status")?,
            optional_string(object, "conclusion")?,
        ),
        "StatusContext" => ReviewCheck::try_new(
            required_string(object, "context")?,
            String::from("completed"),
            Some(required_string(object, "state")?),
        ),
        _ => None,
    }
    .ok_or(CodeHostTransportFailure::InvalidResponse)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StackRequestFacts {
    base_ref: String,
    base_snapshot_revision: String,
    head_ref: String,
    head_revision: String,
    head_repository: CodeHostRepository,
    default_ref: String,
}

fn parse_stack_request(
    value: &serde_json::Value,
    expected_number: u32,
) -> Result<StackRequestFacts, CodeHostTransportFailure> {
    let object = required_object(value)?;
    let number: u32 = required_u64(object, "number")?
        .try_into()
        .map_err(|_| CodeHostTransportFailure::InvalidResponse)?;
    if number != expected_number {
        return Err(CodeHostTransportFailure::InvalidResponse);
    }
    let base = required_object(required(object, "base")?)?;
    let head = required_object(required(object, "head")?)?;
    let head_repository = required_object(required(head, "repo")?)?;
    let base_repository = required_object(required(base, "repo")?)?;
    Ok(StackRequestFacts {
        base_ref: required_string(base, "ref")?,
        base_snapshot_revision: required_string(base, "sha")?,
        head_ref: required_string(head, "ref")?,
        head_revision: required_string(head, "sha")?,
        head_repository: CodeHostRepository::try_new(required_string(
            head_repository,
            "full_name",
        )?)
        .map_err(|_| CodeHostTransportFailure::InvalidResponse)?,
        default_ref: required_string(base_repository, "default_branch")?,
    })
}

fn parse_branch_revision(value: &serde_json::Value) -> Result<String, CodeHostTransportFailure> {
    required_string(
        required_object(required(required_object(value)?, "commit")?)?,
        "sha",
    )
}

struct StackSnapshot<'a> {
    request: &'a StackRequestFacts,
    base_revision: &'a str,
    default_revision: &'a str,
    children: &'a [StackChildFacts],
    children_truncated: bool,
    children_next_cursor: Option<&'a str>,
}

fn ensure_stack_snapshot_unchanged(
    initial: StackSnapshot<'_>,
    current: StackSnapshot<'_>,
) -> Result<(), CodeHostTransportFailure> {
    if initial.request != current.request
        || initial.base_revision != current.base_revision
        || initial.default_revision != current.default_revision
        || initial.children != current.children
        || initial.children_truncated != current.children_truncated
        || initial.children_next_cursor != current.children_next_cursor
    {
        return Err(CodeHostTransportFailure::InvalidResponse);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StackChildFacts {
    number: u32,
    base_ref: String,
    base_snapshot_revision: String,
    head_ref: String,
    head_revision: String,
}

fn parse_stack_child(
    value: &serde_json::Value,
) -> Result<StackChildFacts, CodeHostTransportFailure> {
    let object = required_object(value)?;
    let number = required_u64(object, "number")?
        .try_into()
        .map_err(|_| CodeHostTransportFailure::InvalidResponse)?;
    Ok(StackChildFacts {
        number,
        base_ref: required_string(object, "baseRefName")?,
        base_snapshot_revision: required_string(object, "baseRefOid")?,
        head_ref: required_string(object, "headRefName")?,
        head_revision: required_string(object, "headRefOid")?,
    })
}

fn parse_stack_comparison(
    value: &serde_json::Value,
    expected_base_revision: &str,
) -> Result<u64, CodeHostTransportFailure> {
    let base_ref = nested(value, &["data", "repository", "ref"])?;
    let base_ref = required_object(base_ref)?;
    let target = required_object(required(base_ref, "target")?)?;
    if required_string(target, "oid")? != expected_base_revision {
        return Err(CodeHostTransportFailure::InvalidResponse);
    }
    let comparison = required_object(required(base_ref, "compare")?)?;
    required_u64(comparison, "behindBy")
}

fn remaining_exchange_timeout(elapsed: Duration) -> Result<Duration, CodeHostTransportFailure> {
    DEFAULT_TIMEOUT
        .checked_sub(elapsed)
        .filter(|remaining| !remaining.is_zero())
        .ok_or(CodeHostTransportFailure::DispatchUnknown)
}

fn reject_graphql_errors(value: &serde_json::Value) -> Result<(), CodeHostTransportFailure> {
    match value.get("errors") {
        None => Ok(()),
        Some(serde_json::Value::Array(errors)) if errors.is_empty() => Ok(()),
        Some(serde_json::Value::Array(_errors)) => Err(CodeHostTransportFailure::DispatchUnknown),
        Some(_) => Err(CodeHostTransportFailure::InvalidResponse),
    }
}

fn reject_graphql_mutation_errors(
    value: &serde_json::Value,
) -> Result<(), CodeHostTransportFailure> {
    reject_graphql_errors(value).map_err(|_| CodeHostTransportFailure::DispatchUnknown)
}

fn nested<'a>(
    value: &'a serde_json::Value,
    path: &[&str],
) -> Result<&'a serde_json::Value, CodeHostTransportFailure> {
    let mut current = value;
    for member in path {
        current = current
            .as_object()
            .and_then(|object| object.get(*member))
            .filter(|value| !value.is_null())
            .ok_or(CodeHostTransportFailure::InvalidResponse)?;
    }
    Ok(current)
}

fn nested_bool(value: &serde_json::Value, path: &[&str]) -> Result<bool, CodeHostTransportFailure> {
    nested(value, path)?
        .as_bool()
        .ok_or(CodeHostTransportFailure::InvalidResponse)
}

fn required_object(
    value: &serde_json::Value,
) -> Result<&serde_json::Map<String, serde_json::Value>, CodeHostTransportFailure> {
    value
        .as_object()
        .ok_or(CodeHostTransportFailure::InvalidResponse)
}

fn required<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    member: &str,
) -> Result<&'a serde_json::Value, CodeHostTransportFailure> {
    object
        .get(member)
        .ok_or(CodeHostTransportFailure::InvalidResponse)
}

fn required_string(
    object: &serde_json::Map<String, serde_json::Value>,
    member: &str,
) -> Result<String, CodeHostTransportFailure> {
    required(object, member)?
        .as_str()
        .map(str::to_owned)
        .ok_or(CodeHostTransportFailure::InvalidResponse)
}

fn optional_string(
    object: &serde_json::Map<String, serde_json::Value>,
    member: &str,
) -> Result<Option<String>, CodeHostTransportFailure> {
    match required(object, member)? {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(value) => Ok(Some(value.clone())),
        _ => Err(CodeHostTransportFailure::InvalidResponse),
    }
}

fn omitted_optional_string(
    object: &serde_json::Map<String, serde_json::Value>,
    member: &str,
) -> Result<Option<String>, CodeHostTransportFailure> {
    match object.get(member) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(CodeHostTransportFailure::InvalidResponse),
    }
}

fn optional_object_string(
    object: &serde_json::Map<String, serde_json::Value>,
    object_member: &str,
    string_member: &str,
) -> Result<Option<String>, CodeHostTransportFailure> {
    match required(object, object_member)? {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Object(nested) => required_string(nested, string_member).map(Some),
        _ => Err(CodeHostTransportFailure::InvalidResponse),
    }
}

fn required_u64(
    object: &serde_json::Map<String, serde_json::Value>,
    member: &str,
) -> Result<u64, CodeHostTransportFailure> {
    required(object, member)?
        .as_u64()
        .ok_or(CodeHostTransportFailure::InvalidResponse)
}

fn optional_u64(
    object: &serde_json::Map<String, serde_json::Value>,
    member: &str,
) -> Result<Option<u64>, CodeHostTransportFailure> {
    match required(object, member)? {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(value) => value
            .as_u64()
            .map(Some)
            .ok_or(CodeHostTransportFailure::InvalidResponse),
        _ => Err(CodeHostTransportFailure::InvalidResponse),
    }
}

fn required_bool(
    object: &serde_json::Map<String, serde_json::Value>,
    member: &str,
) -> Result<bool, CodeHostTransportFailure> {
    required(object, member)?
        .as_bool()
        .ok_or(CodeHostTransportFailure::InvalidResponse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CodeHostRepository;

    fn repository() -> CodeHostRepository {
        CodeHostRepository::try_new(String::from("owner/repository"))
            .expect("fixture repository is admitted")
    }

    /// REST paths and pagination are derived only from checked typed segments.
    #[test]
    fn repository_url_uses_fixed_versioned_github_shape() {
        let transport = GitHubCodeHostTransport::try_new().expect("fixed transport constructs");

        let url = transport
            .repository_url(
                &repository(),
                &["pulls", "17", "files"],
                Some(&[("per_page", PAGE_SIZE), ("page", "1")]),
            )
            .expect("fixture URL constructs");

        assert_eq!(
            url.as_str(),
            "https://api.github.com/repos/owner/repository/pulls/17/files?per_page=100&page=1"
        );
    }

    /// GitHub's changed-file response is projected into the bounded result
    /// type without retaining unneeded response members.
    #[test]
    fn changed_file_parser_projects_exact_bounded_fields() {
        let value = serde_json::json!({
            "additions": 7,
            "deletions": 2,
            "filename": "src/lib.rs",
            "patch": "@@ -1 +1 @@\n-old\n+new",
            "status": "modified",
        });
        let expected_file =
            ChangedFile::try_new(String::from("src/lib.rs"), String::from("modified"), 7, 2)
                .expect("fixture changed file is bounded");

        assert_eq!(
            parse_changed_file(&value),
            Ok((expected_file, Some(String::from("@@ -1 +1 @@\n-old\n+new"))))
        );
    }

    /// GitHub may omit patch text for a binary file while retaining its
    /// complete changed-file summary.
    #[test]
    fn changed_file_parser_accepts_omitted_patch() {
        let value = serde_json::json!({
            "additions": 0,
            "deletions": 0,
            "filename": "assets/image.png",
            "status": "modified",
        });
        let expected_file = ChangedFile::try_new(
            String::from("assets/image.png"),
            String::from("modified"),
            0,
            0,
        )
        .expect("fixture changed file is bounded");

        assert_eq!(parse_changed_file(&value), Ok((expected_file, None)));
    }

    /// Paths returned by GitHub remain repository-relative across both result
    /// shapes that expose them.
    #[test]
    fn returned_paths_reject_absolute_values() {
        let absolute_path = String::from("/src/lib.rs");
        let changed_file =
            ChangedFile::try_new(absolute_path.clone(), String::from("modified"), 1, 0);
        let review_thread = ReviewThread::try_new(ReviewThreadFields {
            id: String::from("PRRT_fixture"),
            resolved: false,
            outdated: false,
            path: absolute_path,
            line: Some(1),
            comments: Vec::new(),
            comments_truncated: false,
        });

        assert_eq!(changed_file, None);
        assert_eq!(review_thread, None);
    }

    /// A GraphQL mutation error cannot be mistaken for a definitive
    /// no-mutation acknowledgement.
    #[test]
    fn graphql_mutation_error_is_commit_ambiguous() {
        let value = serde_json::json!({"errors": [{"message": "fixture rejection"}]});

        assert_eq!(
            reject_graphql_mutation_errors(&value),
            Err(CodeHostTransportFailure::DispatchUnknown)
        );
    }

    /// A malformed GraphQL mutation error member cannot authenticate a
    /// successful acknowledgement.
    #[test]
    fn malformed_graphql_mutation_error_is_commit_ambiguous() {
        let value = serde_json::json!({"errors": "malformed fixture"});

        assert_eq!(
            reject_graphql_mutation_errors(&value),
            Err(CodeHostTransportFailure::DispatchUnknown)
        );
    }

    /// A GraphQL error from a read can report resolver, rate-limit, or server
    /// failure and therefore remains infrastructure failure.
    #[test]
    fn graphql_read_error_is_dispatch_unknown() {
        let value = serde_json::json!({"errors": [{"message": "fixture server failure"}]});

        assert_eq!(
            reject_graphql_errors(&value),
            Err(CodeHostTransportFailure::DispatchUnknown)
        );
    }

    /// The authenticated redirect response consumes the same timeout budget
    /// later used for DNS admission and the credential-free download.
    #[test]
    fn job_log_redirect_uses_remaining_exchange_timeout() {
        const ELAPSED: Duration = Duration::from_secs(7);
        const EXPECTED_REMAINING: Duration = Duration::from_secs(23);

        assert_eq!(remaining_exchange_timeout(ELAPSED), Ok(EXPECTED_REMAINING));
    }

    /// A read-only server failure remains an infrastructure failure rather
    /// than becoming definitive known-failure evidence.
    #[test]
    fn server_status_is_dispatch_unknown() {
        assert_eq!(
            ensure_expected_status(StatusCode::INTERNAL_SERVER_ERROR, StatusCode::OK),
            Err(CodeHostTransportFailure::DispatchUnknown)
        );
    }

    /// A job-log redirect DNS failure remains read-side infrastructure
    /// failure rather than becoming definitive rejection evidence.
    #[test]
    fn redirect_dns_failure_is_dispatch_unknown() {
        assert_eq!(
            classify_public_destination_error(PublicDestinationClientError::Infrastructure),
            CodeHostTransportFailure::DispatchUnknown
        );
    }

    /// An exactly-capped body followed only by an empty frame retained every
    /// byte GitHub sent, so no content was discarded.
    #[tokio::test]
    async fn exact_cap_body_with_empty_trailing_frame_is_complete() {
        const LIMIT: usize = 4;
        let stream = futures_util::stream::iter([
            Ok::<Vec<u8>, std::convert::Infallible>(vec![b'l'; LIMIT]),
            Ok(Vec::new()),
        ]);

        assert_eq!(
            read_bounded(stream, LIMIT).await,
            Ok((vec![b'l'; LIMIT], CodeHostResultCompleteness::Complete))
        );
    }

    /// An exactly-capped body followed by further content is truncated, so the
    /// empty-frame allowance cannot hide discarded bytes.
    #[tokio::test]
    async fn exact_cap_body_with_further_content_is_truncated() {
        const LIMIT: usize = 4;
        let stream = futures_util::stream::iter([
            Ok::<Vec<u8>, std::convert::Infallible>(vec![b'l'; LIMIT]),
            Ok(Vec::new()),
            Ok(vec![b'x']),
        ]);

        assert_eq!(
            read_bounded(stream, LIMIT).await,
            Ok((vec![b'l'; LIMIT], CodeHostResultCompleteness::Truncated))
        );
    }

    /// A nonempty terminal GraphQL page may retain its last node cursor.
    #[test]
    fn terminal_page_discards_noncontinuation_cursor() {
        let value = serde_json::json!({
            "pageInfo": {"endCursor": "last-node", "hasNextPage": false}
        });
        let connection = required_object(&value).expect("fixture connection is an object");

        assert_eq!(next_page(connection), Ok((false, None)));
    }

    /// A genuinely truncated page must identify the continuation boundary.
    #[test]
    fn truncated_page_requires_cursor() {
        let value = serde_json::json!({
            "pageInfo": {"endCursor": null, "hasNextPage": true}
        });
        let connection = required_object(&value).expect("fixture connection is an object");

        assert_eq!(
            next_page(connection),
            Err(CodeHostTransportFailure::InvalidResponse)
        );
    }

    /// Immediate-child discovery retains the pull request's admitted head
    /// repository rather than assuming the base repository owns the branch.
    #[test]
    fn stack_request_preserves_head_repository() {
        const HEAD_REPOSITORY: &str = "contributor/repository";
        let value = serde_json::json!({
            "number": 17,
            "base": {
                "ref": "main",
                "sha": "1111111111111111111111111111111111111111",
                "repo": {"default_branch": "main"}
            },
            "head": {
                "ref": "feature",
                "sha": "2222222222222222222222222222222222222222",
                "repo": {"full_name": HEAD_REPOSITORY}
            }
        });

        let request = parse_stack_request(&value, 17).expect("fixture request is admitted");

        assert_eq!(request.head_repository.as_str(), HEAD_REPOSITORY);
    }

    /// The ancestry query requests no unbounded commit or changed-file
    /// collections alongside its authenticated count.
    #[test]
    fn stack_comparison_query_is_count_only() {
        assert!(STACK_COMPARISON_QUERY.contains("behindBy"));
        assert!(!STACK_COMPARISON_QUERY.contains("commits"));
        assert!(!STACK_COMPARISON_QUERY.contains("files"));
    }

    /// Child discovery projects only the bounded identities needed for ancestry.
    #[test]
    fn stack_children_query_is_field_projected() {
        assert!(STACK_CHILDREN_QUERY.contains("number baseRefName baseRefOid"));
        assert!(STACK_CHILDREN_QUERY.contains("headRefName headRefOid"));
        assert!(!STACK_CHILDREN_QUERY.contains("title"));
        assert!(!STACK_CHILDREN_QUERY.contains("body"));
        assert!(!STACK_CHILDREN_QUERY.contains("commits"));
        assert!(!STACK_CHILDREN_QUERY.contains("files"));
    }

    /// Child parsing preserves the code host's potentially stale base snapshot.
    #[test]
    fn stack_child_parser_preserves_base_snapshot() {
        const BASE_REF: &str = "feature";
        const BASE_SNAPSHOT: &str = "1111111111111111111111111111111111111111";
        const HEAD_REF: &str = "child";
        const HEAD_REVISION: &str = "2222222222222222222222222222222222222222";
        const NUMBER: u32 = 18;
        let value = serde_json::json!({
            "number": NUMBER,
            "baseRefName": BASE_REF,
            "baseRefOid": BASE_SNAPSHOT,
            "headRefName": HEAD_REF,
            "headRefOid": HEAD_REVISION,
        });

        assert_eq!(
            parse_stack_child(&value),
            Ok(StackChildFacts {
                number: NUMBER,
                base_ref: String::from(BASE_REF),
                base_snapshot_revision: String::from(BASE_SNAPSHOT),
                head_ref: String::from(HEAD_REF),
                head_revision: String::from(HEAD_REVISION),
            })
        );
    }

    /// Stack ancestry reads retain only the count projection and authenticate
    /// it against the exact base revision used by the snapshot.
    #[test]
    fn stack_comparison_projects_authenticated_behind_count() {
        const BASE_REVISION: &str = "1111111111111111111111111111111111111111";
        const EXPECTED_BEHIND_BY: u64 = 7;
        let value = serde_json::json!({
            "data": {
                "repository": {
                    "ref": {
                        "target": {"oid": BASE_REVISION},
                        "compare": {"behindBy": EXPECTED_BEHIND_BY}
                    }
                }
            }
        });

        assert_eq!(
            parse_stack_comparison(&value, BASE_REVISION),
            Ok(EXPECTED_BEHIND_BY)
        );
    }

    /// A comparison resolved from a moved base ref cannot be reported as
    /// evidence for the earlier stack snapshot.
    #[test]
    fn stack_comparison_rejects_moved_base_ref() {
        const EXPECTED_BASE_REVISION: &str = "1111111111111111111111111111111111111111";
        const MOVED_BASE_REVISION: &str = "2222222222222222222222222222222222222222";
        const ARBITRARY_BEHIND_BY: u64 = 7;
        let value = serde_json::json!({
            "data": {
                "repository": {
                    "ref": {
                        "target": {"oid": MOVED_BASE_REVISION},
                        "compare": {"behindBy": ARBITRARY_BEHIND_BY}
                    }
                }
            }
        });

        assert_eq!(
            parse_stack_comparison(&value, EXPECTED_BASE_REVISION),
            Err(CodeHostTransportFailure::InvalidResponse)
        );
    }

    /// A changed request-level base snapshot invalidates the surrounding stack
    /// transaction even though comparisons use the separately read branch tip.
    #[test]
    fn stack_snapshot_rejects_changed_request_base_snapshot() {
        const INITIAL_BASE: &str = "1111111111111111111111111111111111111111";
        const CURRENT_BASE: &str = "3333333333333333333333333333333333333333";
        const DEFAULT_REVISION: &str = "1111111111111111111111111111111111111111";
        let initial = parse_stack_request(
            &serde_json::json!({
                "number": 17,
                "base": {
                    "ref": "main",
                    "sha": INITIAL_BASE,
                    "repo": {"default_branch": "main"}
                },
                "head": {
                    "ref": "feature",
                    "sha": "2222222222222222222222222222222222222222",
                    "repo": {"full_name": "owner/repository"}
                }
            }),
            17,
        )
        .expect("initial fixture request is admitted");
        let current = parse_stack_request(
            &serde_json::json!({
                "number": 17,
                "base": {
                    "ref": "main",
                    "sha": CURRENT_BASE,
                    "repo": {"default_branch": "main"}
                },
                "head": {
                    "ref": "feature",
                    "sha": "2222222222222222222222222222222222222222",
                    "repo": {"full_name": "owner/repository"}
                }
            }),
            17,
        )
        .expect("current fixture request is admitted");

        assert_eq!(
            ensure_stack_snapshot_unchanged(
                StackSnapshot {
                    request: &initial,
                    base_revision: DEFAULT_REVISION,
                    default_revision: DEFAULT_REVISION,
                    children: &[],
                    children_truncated: false,
                    children_next_cursor: None,
                },
                StackSnapshot {
                    request: &current,
                    base_revision: DEFAULT_REVISION,
                    default_revision: DEFAULT_REVISION,
                    children: &[],
                    children_truncated: false,
                    children_next_cursor: None,
                },
            ),
            Err(CodeHostTransportFailure::InvalidResponse)
        );
    }

    /// A changed default revision invalidates the default-chain comparisons.
    #[test]
    fn stack_snapshot_rejects_changed_default_revision() {
        const INITIAL_DEFAULT: &str = "1111111111111111111111111111111111111111";
        const CURRENT_DEFAULT: &str = "3333333333333333333333333333333333333333";
        let value = serde_json::json!({
            "number": 17,
            "base": {
                "ref": "main",
                "sha": "1111111111111111111111111111111111111111",
                "repo": {"default_branch": "main"}
            },
            "head": {
                "ref": "feature",
                "sha": "2222222222222222222222222222222222222222",
                "repo": {"full_name": "owner/repository"}
            }
        });
        let request = parse_stack_request(&value, 17).expect("fixture request is admitted");

        assert_eq!(
            ensure_stack_snapshot_unchanged(
                StackSnapshot {
                    request: &request,
                    base_revision: INITIAL_DEFAULT,
                    default_revision: INITIAL_DEFAULT,
                    children: &[],
                    children_truncated: false,
                    children_next_cursor: None,
                },
                StackSnapshot {
                    request: &request,
                    base_revision: INITIAL_DEFAULT,
                    default_revision: CURRENT_DEFAULT,
                    children: &[],
                    children_truncated: false,
                    children_next_cursor: None,
                },
            ),
            Err(CodeHostTransportFailure::InvalidResponse)
        );
    }

    /// A changed child page invalidates comparisons made against the earlier
    /// child inventory.
    #[test]
    fn stack_snapshot_rejects_changed_child_inventory() {
        const DEFAULT_REVISION: &str = "1111111111111111111111111111111111111111";
        const HEAD_REVISION: &str = "2222222222222222222222222222222222222222";
        let request = StackRequestFacts {
            base_ref: String::from("main"),
            base_snapshot_revision: String::from(DEFAULT_REVISION),
            head_ref: String::from("feature"),
            head_revision: String::from(HEAD_REVISION),
            head_repository: CodeHostRepository::try_new(String::from("owner/repository"))
                .expect("fixture repository is admitted"),
            default_ref: String::from("main"),
        };
        let initial = [StackChildFacts {
            number: 18,
            base_ref: String::from("feature"),
            base_snapshot_revision: String::from(HEAD_REVISION),
            head_ref: String::from("child"),
            head_revision: String::from("3333333333333333333333333333333333333333"),
        }];
        let current = [StackChildFacts {
            number: 19,
            base_ref: String::from("feature"),
            base_snapshot_revision: String::from(HEAD_REVISION),
            head_ref: String::from("other-child"),
            head_revision: String::from("4444444444444444444444444444444444444444"),
        }];

        assert_eq!(
            ensure_stack_snapshot_unchanged(
                StackSnapshot {
                    request: &request,
                    base_revision: DEFAULT_REVISION,
                    default_revision: DEFAULT_REVISION,
                    children: &initial,
                    children_truncated: false,
                    children_next_cursor: None,
                },
                StackSnapshot {
                    request: &request,
                    base_revision: DEFAULT_REVISION,
                    default_revision: DEFAULT_REVISION,
                    children: &current,
                    children_truncated: false,
                    children_next_cursor: None,
                },
            ),
            Err(CodeHostTransportFailure::InvalidResponse)
        );
    }

    /// Disposition-shaped text from an unaffiliated reply cannot satisfy the
    /// parsed inventory protocol.
    #[test]
    fn slog_thread_rejects_unauthorized_disposition_evidence() {
        let value = serde_json::json!({
            "id": "PRRT_fixture",
            "isResolved": true,
            "isOutdated": false,
            "path": "src/lib.rs",
            "line": 12,
            "comments": {
                "nodes": [
                    {
                        "author": {"login": "review-bot", "__typename": "Bot"},
                        "authorAssociation": "NONE",
                        "body": "Finding title"
                    },
                    {
                        "author": {"login": "visitor", "__typename": "User"},
                        "authorAssociation": "NONE",
                        "body": "Fixed in commit `0123456789abcdef`"
                    }
                ],
                "pageInfo": {"hasNextPage": false}
            }
        });

        let thread = parse_slog_thread(&value).expect("fixture thread is admitted");

        assert_eq!(
            thread.inventory.disposition(),
            ReviewDispositionClass::Undispositioned
        );
    }

    /// An over-bound comment history cannot be classified from a silently
    /// incomplete prefix.
    #[test]
    fn slog_thread_rejects_truncated_comment_history() {
        let value = serde_json::json!({
            "id": "PRRT_fixture",
            "isResolved": false,
            "isOutdated": false,
            "path": "src/lib.rs",
            "line": 12,
            "comments": {
                "nodes": [{
                    "author": {"login": "review-bot", "__typename": "Bot"},
                    "body": "Finding title"
                }],
                "pageInfo": {"hasNextPage": true}
            }
        });

        assert_eq!(
            parse_slog_thread(&value),
            Err(CodeHostTransportFailure::InvalidResponse)
        );
    }

    /// Lossy decoding cannot expand a retained job-log prefix beyond its
    /// declared byte bound.
    #[test]
    fn lossy_job_log_text_remains_bounded() {
        const INVALID_UTF8: u8 = 0xff;
        const EXPECTED_TEXT_BYTES: usize = MAX_JOB_LOG_BYTES - 1;
        let bytes = vec![INVALID_UTF8; MAX_JOB_LOG_BYTES];

        let (text, completeness) = bounded_lossy_text(&bytes, CodeHostResultCompleteness::Complete);

        assert_eq!(text.len(), EXPECTED_TEXT_BYTES);
        assert_eq!(completeness, CodeHostResultCompleteness::Truncated);
    }
}
