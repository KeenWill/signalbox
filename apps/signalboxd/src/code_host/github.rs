//! Production GitHub REST/GraphQL adapter for the code-host tool suite.

use std::{error::Error, fmt, time::Duration};

use futures_util::StreamExt;
use reqwest::{
    Client, Method, Response, StatusCode, Url,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue, LOCATION, USER_AGENT},
    redirect::Policy,
};
use signalbox_model_runtime::CredentialValue;

use super::{
    ChangeRequestCommentResult, ChangeRequestSummaryFields, ChangeRequestSummaryResult,
    ChangedFile, ChangedFilesResult, CheckStatus, ChecksStatusResult, CiJobLogResult,
    CodeHostOperation, CodeHostResult, CodeHostResultCompleteness, CodeHostTransport,
    CodeHostTransportFailure, FilePatchResult, RerunFailedJobsResult, ReviewThread,
    ReviewThreadComment, ReviewThreadFields, ReviewThreadResolution, ReviewThreadsResult,
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
        let result = ChangeRequestSummaryResult::try_new(ChangeRequestSummaryFields {
            number: required_u64(object, "number")?
                .try_into()
                .map_err(|_| CodeHostTransportFailure::InvalidResponse)?,
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
        let value = self
            .mutation_json_response(response, StatusCode::OK)
            .await?;
        reject_graphql_mutation_errors(&value)?;
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
        let value = self.json_response(response, StatusCode::OK).await?;
        reject_graphql_errors(&value)?;
        let result = (|| {
            let thread = nested(&value, &["data", "resolveReviewThread", "thread"])?;
            let object = required_object(thread)?;
            let resolution = if required_bool(object, "isResolved")? {
                ReviewThreadResolution::Resolved
            } else {
                ReviewThreadResolution::Open
            };
            ThreadResolveResult::try_new(required_string(object, "id")?, resolution)
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
        let url = self.repository_url(
            arguments.repository(),
            &["actions", "jobs", &arguments.job_id().to_string(), "logs"],
            None,
        )?;
        let response = self
            .send_authenticated(Method::GET, url, None, credential)
            .await?;
        if response.status() != StatusCode::FOUND {
            return Err(CodeHostTransportFailure::Rejected);
        }
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.len() <= MAX_REDIRECT_URL_BYTES)
            .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        let redirect =
            Url::parse(location).map_err(|_| CodeHostTransportFailure::InvalidResponse)?;
        if redirect.scheme() != "https"
            || redirect.host_str().is_none()
            || !redirect.username().is_empty()
            || redirect.password().is_some()
            || redirect.fragment().is_some()
        {
            return Err(CodeHostTransportFailure::InvalidResponse);
        }
        let response = self
            .client
            .get(redirect)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .send()
            .await
            .map_err(|_| CodeHostTransportFailure::DispatchUnknown)?;
        if response.status() != StatusCode::OK {
            return Err(CodeHostTransportFailure::Rejected);
        }
        let (bytes, completeness) = read_bounded(response, MAX_JOB_LOG_BYTES).await?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
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
        if response.status().is_client_error() {
            return Err(CodeHostTransportFailure::Rejected);
        }
        if response.status() != expected {
            return Err(CodeHostTransportFailure::DispatchUnknown);
        }
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
        if response.status() != expected {
            return Err(CodeHostTransportFailure::Rejected);
        }
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
        let (body, body_completeness) = read_bounded(response, MAX_JSON_RESPONSE_BYTES).await?;
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
            CodeHostOperation::ReviewThreads(arguments) => {
                self.review_threads(arguments, credential).await
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

async fn read_bounded(
    response: Response,
    limit: usize,
) -> Result<(Vec<u8>, CodeHostResultCompleteness), CodeHostTransportFailure> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| CodeHostTransportFailure::DispatchUnknown)?;
        let remaining = limit.saturating_sub(body.len());
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            return Ok((body, CodeHostResultCompleteness::Truncated));
        }
        body.extend_from_slice(&chunk);
        if body.len() == limit {
            let completeness = if stream
                .next()
                .await
                .transpose()
                .map_err(|_| CodeHostTransportFailure::DispatchUnknown)?
                .is_some()
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
    let patch = optional_string(object, "patch")?;
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

fn reject_graphql_errors(value: &serde_json::Value) -> Result<(), CodeHostTransportFailure> {
    if value
        .get("errors")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|errors| !errors.is_empty())
    {
        Err(CodeHostTransportFailure::Rejected)
    } else {
        Ok(())
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
}
