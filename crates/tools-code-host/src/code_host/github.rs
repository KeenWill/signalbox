//! Production GitHub REST/GraphQL adapter for the code-host tool suite.

use std::{collections::HashSet, error::Error, fmt, future::Future, time::Duration};

use futures_util::StreamExt;
use reqwest::{
    Client, Method, Response, StatusCode, Url,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue, LOCATION, USER_AGENT},
    redirect::Policy,
};
use signalbox_model_runtime::CredentialValue;

use signalbox_egress_transport::{
    PublicDestinationClientError, has_more_response_bytes, public_destination_client,
};

use super::arguments::{MAX_FILE_PATH_BYTES, valid_revision};
use super::repository_result::{
    MAX_OBSERVED_DIRECTORY_ENTRIES, MAX_REPOSITORY_FILE_CONTENT_BYTES,
    MAX_REPOSITORY_FILE_SCAN_BYTES, is_immediate_repository_child,
};
use super::result::{MAX_ENCODED_RESULT_BYTES, MAX_RESULT_ITEMS, absolute_https_url};
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
    ConvergenceStateResult, FilePatchResult, RepositoryDirectoryEntry, RepositoryFileContentFields,
    RepositoryLineRange, RepositoryListDirectoryResult, RepositoryObjectKind,
    RepositoryReadFileResult, RerunFailedJobsResult, ReviewCheck, ReviewDispositionClass,
    ReviewGateCheckArguments, ReviewGateCheckResult, ReviewThread, ReviewThreadComment,
    ReviewThreadFields, ReviewThreadIdentity, ReviewThreadInventoryFields,
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
const MAX_JSON_ESCAPE_BYTES_PER_SOURCE_BYTE: usize = 6;
// A standard contents entry can repeat a path in `name`, `path`, four URL
// fields, and the three `_links` fields. A symlink `target` is blob material,
// and `submodule_git_url` is only a legacy kind marker; admit and budget both
// separately instead of treating either as a path.
const MAX_REPOSITORY_CONTENTS_PATH_FIELDS_PER_ENTRY: usize = 9;
const MAX_REPOSITORY_SYMLINK_TARGET_BYTES: usize = 4 * 1024;
const MAX_REPOSITORY_SUBMODULE_URL_BYTES: usize = 8 * 1024;
const MAX_REPOSITORY_CONTENTS_ENTRY_FIXED_BYTES: usize = 8 * 1024;
const MAX_REPOSITORY_CONTENTS_RESPONSE_BYTES: usize = (MAX_OBSERVED_DIRECTORY_ENTRIES + 1)
    * (MAX_FILE_PATH_BYTES
        * MAX_JSON_ESCAPE_BYTES_PER_SOURCE_BYTE
        * MAX_REPOSITORY_CONTENTS_PATH_FIELDS_PER_ENTRY
        + MAX_REPOSITORY_SYMLINK_TARGET_BYTES * MAX_JSON_ESCAPE_BYTES_PER_SOURCE_BYTE
        + MAX_REPOSITORY_SUBMODULE_URL_BYTES * MAX_JSON_ESCAPE_BYTES_PER_SOURCE_BYTE
        + MAX_REPOSITORY_CONTENTS_ENTRY_FIXED_BYTES);
const DEFAULT_ACCEPT: &str = "application/vnd.github+json";
const COMMIT_SHA_ACCEPT: &str = "application/vnd.github.sha";
const MAX_COMMIT_SHA_RESPONSE_BYTES: usize = 41;
const CONTENTS_OBJECT_ACCEPT: &str = "application/vnd.github.object+json";
const BLOB_RAW_ACCEPT: &str = "application/vnd.github.raw+json";
const MAX_JOB_LOG_BYTES: usize = 64 * 1024;
const MAX_REDIRECT_URL_BYTES: usize = 8 * 1024;
const PAGE_SIZE: &str = "100";
const MAX_CHANGED_FILE_PAGES: u16 = 30;
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

const THREAD_OWNERSHIP_QUERY: &str = r#"
query ThreadOwnership($thread: ID!) {
  node(id: $thread) {
    __typename
    ... on PullRequestReviewThread {
      pullRequest {
        number
        repository { nameWithOwner }
      }
    }
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

    async fn repository_read_file(
        &self,
        arguments: super::RepositoryReadFileArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        with_read_operation_timeout(
            DEFAULT_TIMEOUT,
            self.repository_read_file_transaction(arguments, credential),
        )
        .await
    }

    async fn repository_read_file_transaction(
        &self,
        arguments: super::RepositoryReadFileArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        let lookup = self
            .repository_path_lookup(
                arguments.repository(),
                arguments.path(),
                arguments.revision(),
                credential,
            )
            .await?;
        let result = match lookup {
            RepositoryPathLookup::File { blob, source_bytes } => {
                let scan_limit_bytes = u64::try_from(MAX_REPOSITORY_FILE_SCAN_BYTES)
                    .map_err(|_| CodeHostTransportFailure::InvalidResponse)?;
                if arguments.line_range().is_some() && source_bytes > scan_limit_bytes {
                    RepositoryReadFileResult::try_line_range_unavailable(&arguments, source_bytes)
                } else {
                    let body = self
                        .repository_file_blob(
                            arguments.repository(),
                            blob.as_str(),
                            arguments.line_range(),
                            credential,
                        )
                        .await?;
                    if body.observed_source_bytes > source_bytes
                        || (body.source_exhausted && body.observed_source_bytes != source_bytes)
                    {
                        return Err(CodeHostTransportFailure::InvalidResponse);
                    }
                    let selected_source_bytes = if arguments.line_range().is_some() {
                        body.observed_selected_bytes
                    } else {
                        source_bytes
                    };
                    match body.kind {
                        RepositoryFileBodyKind::Text(selection) => {
                            RepositoryReadFileResult::try_content(
                                &arguments,
                                RepositoryFileContentFields {
                                    source_bytes,
                                    selected_source_bytes,
                                    start_line: selection.start_line,
                                    end_line: selection.end_line,
                                    returned_lines: selection.returned_lines,
                                    last_line_complete: selection.last_line_complete,
                                    content: selection.content,
                                    completeness: selection.completeness,
                                },
                            )
                        }
                        RepositoryFileBodyKind::Binary => {
                            RepositoryReadFileResult::try_binary(&arguments, source_bytes)
                        }
                    }
                }
            }
            RepositoryPathLookup::Directory { .. } => RepositoryReadFileResult::try_not_a_file(
                &arguments,
                RepositoryObjectKind::Directory,
            ),
            RepositoryPathLookup::Other { kind } => {
                RepositoryReadFileResult::try_not_a_file(&arguments, kind)
            }
            RepositoryPathLookup::PathNotFound => {
                RepositoryReadFileResult::try_path_not_found(&arguments)
            }
            RepositoryPathLookup::RevisionNotFound => {
                Some(RepositoryReadFileResult::revision_not_found(&arguments))
            }
        }
        .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        Ok(CodeHostResult::ReadFile(result))
    }

    async fn repository_list_directory(
        &self,
        arguments: super::RepositoryListDirectoryArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        with_read_operation_timeout(
            DEFAULT_TIMEOUT,
            self.repository_list_directory_transaction(arguments, credential),
        )
        .await
    }

    async fn repository_list_directory_transaction(
        &self,
        arguments: super::RepositoryListDirectoryArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        let lookup = self
            .repository_path_lookup(
                arguments.repository(),
                arguments.path(),
                arguments.revision(),
                credential,
            )
            .await?;
        let result = match lookup {
            RepositoryPathLookup::Directory {
                entries,
                completeness: source_completeness,
            } => bounded_repository_directory_result(&arguments, entries, source_completeness),
            RepositoryPathLookup::File { .. } => {
                RepositoryListDirectoryResult::try_not_a_directory(
                    &arguments,
                    RepositoryObjectKind::File,
                )
            }
            RepositoryPathLookup::Other { kind } => {
                RepositoryListDirectoryResult::try_not_a_directory(&arguments, kind)
            }
            RepositoryPathLookup::PathNotFound => {
                RepositoryListDirectoryResult::try_path_not_found(&arguments)
            }
            RepositoryPathLookup::RevisionNotFound => Some(
                RepositoryListDirectoryResult::revision_not_found(&arguments),
            ),
        }
        .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        Ok(CodeHostResult::ListDirectory(result))
    }

    async fn repository_path_lookup(
        &self,
        repository: &CodeHostRepository,
        path: &super::CodeHostFilePath,
        revision: &super::CodeHostRevision,
        credential: &CredentialValue,
    ) -> Result<RepositoryPathLookup, CodeHostTransportFailure> {
        let resolution = self
            .repository_revision_resolution(repository, revision, credential)
            .await?;
        let (resolved_revision, revision_visible) = match resolution {
            RepositoryRevisionResolution::Exact(resolved_revision) => (resolved_revision, true),
            RepositoryRevisionResolution::Missing => (revision.clone(), false),
            RepositoryRevisionResolution::DefinitiveMissing
            | RepositoryRevisionResolution::EmptyRepository => {
                return Ok(RepositoryPathLookup::RevisionNotFound);
            }
        };
        let url = self.repository_contents_url(repository, path, &resolved_revision)?;
        let response = self
            .send_authenticated_with_accept(
                Method::GET,
                url,
                None,
                CONTENTS_OBJECT_ACCEPT,
                credential,
            )
            .await?;
        match response.status() {
            StatusCode::OK if revision_visible => {
                let (value, completeness) = bounded_json_page(
                    response,
                    StatusCode::OK,
                    MAX_REPOSITORY_CONTENTS_RESPONSE_BYTES,
                )
                .await?;
                parse_repository_path_lookup(&value, path.as_str(), completeness)
            }
            StatusCode::OK => Err(CodeHostTransportFailure::InvalidResponse),
            StatusCode::NOT_FOUND if revision_visible => Ok(RepositoryPathLookup::PathNotFound),
            StatusCode::NOT_FOUND => {
                if repository_contents_response_names_missing_revision(response, revision).await? {
                    Ok(RepositoryPathLookup::RevisionNotFound)
                } else {
                    Err(CodeHostTransportFailure::Rejected)
                }
            }
            status if status.is_client_error() => Err(CodeHostTransportFailure::Rejected),
            _ => Err(CodeHostTransportFailure::DispatchUnknown),
        }
    }

    async fn repository_revision_resolution(
        &self,
        repository: &CodeHostRepository,
        revision: &super::CodeHostRevision,
        credential: &CredentialValue,
    ) -> Result<RepositoryRevisionResolution, CodeHostTransportFailure> {
        let url = self.repository_url(repository, &["commits", revision.as_str()], None)?;
        let response = self
            .send_authenticated_with_accept(Method::GET, url, None, COMMIT_SHA_ACCEPT, credential)
            .await?;
        match response.status() {
            StatusCode::OK => exact_repository_revision_from_response(response, revision)
                .await
                .map(RepositoryRevisionResolution::Exact),
            StatusCode::NOT_FOUND => Ok(RepositoryRevisionResolution::Missing),
            StatusCode::CONFLICT => Ok(RepositoryRevisionResolution::EmptyRepository),
            StatusCode::UNPROCESSABLE_ENTITY => {
                if repository_commit_response_names_missing_revision(response, revision).await? {
                    Ok(RepositoryRevisionResolution::DefinitiveMissing)
                } else {
                    Err(CodeHostTransportFailure::Rejected)
                }
            }
            status if status.is_client_error() => Err(CodeHostTransportFailure::Rejected),
            _ => Err(CodeHostTransportFailure::DispatchUnknown),
        }
    }

    async fn repository_file_blob(
        &self,
        repository: &CodeHostRepository,
        blob: &str,
        line_range: Option<RepositoryLineRange>,
        credential: &CredentialValue,
    ) -> Result<RepositoryFileBody, CodeHostTransportFailure> {
        let url = self.repository_url(repository, &["git", "blobs", blob], None)?;
        let response = self
            .send_authenticated_with_accept(Method::GET, url, None, BLOB_RAW_ACCEPT, credential)
            .await?;
        ensure_expected_status(response.status(), StatusCode::OK)?;
        select_repository_file_content(response.bytes_stream(), line_range).await
    }

    fn repository_contents_url(
        &self,
        repository: &CodeHostRepository,
        path: &super::CodeHostFilePath,
        revision: &super::CodeHostRevision,
    ) -> Result<Url, CodeHostTransportFailure> {
        let mut suffix = vec!["contents"];
        if path.as_str() != "." {
            suffix.extend(path.as_str().split('/'));
        }
        self.repository_url(repository, &suffix, Some(&[("ref", revision.as_str())]))
    }

    async fn file_patch(
        &self,
        arguments: super::FilePatchArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        with_read_operation_timeout(
            DEFAULT_TIMEOUT,
            self.file_patch_transaction(arguments, credential),
        )
        .await
    }

    async fn file_patch_transaction(
        &self,
        arguments: super::FilePatchArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        let initial_revision = self
            .change_request_diff_revision(arguments.repository(), arguments.number(), credential)
            .await?;
        let outcome = self.find_file_patch(arguments.clone(), credential).await;
        match &outcome {
            Ok(_) | Err(CodeHostTransportFailure::NotFound) => {}
            Err(
                CodeHostTransportFailure::InvalidCredential
                | CodeHostTransportFailure::Rejected
                | CodeHostTransportFailure::ThreadNotInChangeRequest
                | CodeHostTransportFailure::InvalidResponse
                | CodeHostTransportFailure::ResponseTooLarge
                | CodeHostTransportFailure::ChangeRequestRevisionChanged
                | CodeHostTransportFailure::MutationNotDispatched
                | CodeHostTransportFailure::DispatchUnknown,
            ) => return outcome,
        }
        let current_revision = self
            .change_request_diff_revision(arguments.repository(), arguments.number(), credential)
            .await?;
        if initial_revision != current_revision {
            return Err(CodeHostTransportFailure::ChangeRequestRevisionChanged);
        }
        outcome
    }

    async fn find_file_patch(
        &self,
        arguments: super::FilePatchArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        for page in 1..=MAX_CHANGED_FILE_PAGES {
            let page_text = page.to_string();
            let url = self.repository_url(
                arguments.repository(),
                &["pulls", &arguments.number().get().to_string(), "files"],
                Some(&[("per_page", PAGE_SIZE), ("page", page_text.as_str())]),
            )?;
            let response = self
                .send_authenticated(Method::GET, url, None, credential)
                .await?;
            let (value, completeness) = self.json_page(response, StatusCode::OK).await?;
            match inspect_file_patch_page(&value, completeness, arguments.path().as_str())? {
                FilePatchPageOutcome::Found(result) => {
                    return Ok(CodeHostResult::FilePatch(result));
                }
                FilePatchPageOutcome::NotFound => {
                    return Err(CodeHostTransportFailure::NotFound);
                }
                FilePatchPageOutcome::Continue if page < MAX_CHANGED_FILE_PAGES => {}
                FilePatchPageOutcome::Continue => {
                    return Err(CodeHostTransportFailure::InvalidResponse);
                }
            }
        }
        Err(CodeHostTransportFailure::InvalidResponse)
    }

    async fn change_request_diff_revision(
        &self,
        repository: &super::CodeHostRepository,
        number: CodeHostChangeRequestNumber,
        credential: &CredentialValue,
    ) -> Result<ChangeRequestDiffRevision, CodeHostTransportFailure> {
        let url = self.repository_url(repository, &["pulls", &number.get().to_string()], None)?;
        let response = self
            .send_authenticated(Method::GET, url, None, credential)
            .await?;
        let value = self.json_response(response, StatusCode::OK).await?;
        parse_change_request_diff_revision(&value, number)
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
        let initial_convergence = self
            .convergence_state_for(arguments.repository(), arguments.number(), credential)
            .await?;
        let stack = self
            .stack_state_for(arguments.repository(), arguments.number(), None, credential)
            .await?;
        let convergence = self
            .convergence_state_for(arguments.repository(), arguments.number(), credential)
            .await?;
        ensure_review_gate_snapshot_unchanged(
            &initial_stack,
            &stack,
            &initial_convergence,
            &convergence,
        )?;
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

    /// Confirms the named thread node belongs to the named change request
    /// before any mutation naming it is dispatched. A review thread never
    /// moves between change requests on GitHub, so evidence gathered here
    /// cannot be invalidated between this read and the following mutation.
    ///
    /// Every failure passes through [`ownership_evidence_failure`]: nothing
    /// has been written when this read fails, so its failures keep read
    /// classification and are never presented as commit-ambiguous mutations.
    ///
    /// The response is judged by its `data` member rather than by the shared
    /// GraphQL error rejection: definitive absence evidence for a `node`
    /// lookup arrives as an evaluated `data.node: null` beside an error
    /// entry carrying the code host's typed `NOT_FOUND` classification, so
    /// the evaluated `data` member distinguishes an answered query from a
    /// failed request, and the error classification distinguishes definitive
    /// absence from a transient field failure that proves nothing.
    async fn confirm_thread_in_change_request(
        &self,
        repository: &CodeHostRepository,
        number: CodeHostChangeRequestNumber,
        thread_id: &super::CodeHostOpaqueId,
        credential: &CredentialValue,
    ) -> Result<(), CodeHostTransportFailure> {
        let outcome = async {
            let body = serde_json::to_vec(&serde_json::json!({
                "query": THREAD_OWNERSHIP_QUERY,
                "variables": {"thread": thread_id.as_str()},
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
            let (value, _completeness) = self.json_page(response, StatusCode::OK).await?;
            let evidence = parse_thread_ownership(&value)?;
            if thread_in_change_request(&evidence, repository, number) {
                Ok(())
            } else {
                Err(CodeHostTransportFailure::ThreadNotInChangeRequest)
            }
        }
        .await;
        outcome.map_err(ownership_evidence_failure)
    }

    async fn thread_reply(
        &self,
        arguments: super::ThreadReplyArguments,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        let started = tokio::time::Instant::now();
        self.confirm_thread_in_change_request(
            arguments.repository(),
            arguments.number(),
            arguments.thread_id(),
            credential,
        )
        .await?;
        let remaining = remaining_mutation_budget(started.elapsed())?;
        tokio::time::timeout(remaining, self.dispatch_thread_reply(arguments, credential))
            .await
            .map_err(|_| CodeHostTransportFailure::DispatchUnknown)?
    }

    async fn dispatch_thread_reply(
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
        let started = tokio::time::Instant::now();
        self.confirm_thread_in_change_request(
            arguments.repository(),
            arguments.number(),
            arguments.thread_id(),
            credential,
        )
        .await?;
        let remaining = remaining_mutation_budget(started.elapsed())?;
        tokio::time::timeout(
            remaining,
            self.dispatch_thread_resolve(arguments, credential),
        )
        .await
        .map_err(|_| CodeHostTransportFailure::DispatchUnknown)?
    }

    async fn dispatch_thread_resolve(
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
        self.send_authenticated_with_accept(method, url, body, DEFAULT_ACCEPT, credential)
            .await
    }

    async fn send_authenticated_with_accept(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        accept: &'static str,
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
            .header(ACCEPT, accept)
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
                | CodeHostTransportFailure::Rejected
                | CodeHostTransportFailure::NotFound => failure,
                CodeHostTransportFailure::ThreadNotInChangeRequest
                | CodeHostTransportFailure::InvalidResponse
                | CodeHostTransportFailure::ResponseTooLarge
                | CodeHostTransportFailure::ChangeRequestRevisionChanged
                | CodeHostTransportFailure::MutationNotDispatched
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
            CodeHostOperation::ListDirectory(arguments) => {
                self.repository_list_directory(arguments, credential).await
            }
            CodeHostOperation::ReadFile(arguments) => {
                self.repository_read_file(arguments, credential).await
            }
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

#[derive(Clone, Debug, Eq, PartialEq)]
enum RepositoryRevisionResolution {
    Exact(super::CodeHostRevision),
    Missing,
    DefinitiveMissing,
    EmptyRepository,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RepositoryPathLookup {
    File {
        blob: String,
        source_bytes: u64,
    },
    Directory {
        entries: Vec<RepositoryDirectoryEntry>,
        completeness: CodeHostResultCompleteness,
    },
    Other {
        kind: RepositoryObjectKind,
    },
    PathNotFound,
    RevisionNotFound,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RepositoryFileSelection {
    content: String,
    start_line: Option<u32>,
    end_line: Option<u32>,
    returned_lines: u32,
    last_line_complete: bool,
    completeness: CodeHostResultCompleteness,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RepositoryFileValidationMode {
    RetainedContentOnly,
    EntireSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RepositoryFileBodyKind {
    Text(RepositoryFileSelection),
    Binary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RepositoryFileBody {
    kind: RepositoryFileBodyKind,
    observed_source_bytes: u64,
    observed_selected_bytes: u64,
    source_exhausted: bool,
}

async fn exact_repository_revision_from_response(
    response: Response,
    expected: &super::CodeHostRevision,
) -> Result<super::CodeHostRevision, CodeHostTransportFailure> {
    let (body, completeness) =
        read_bounded(response.bytes_stream(), MAX_COMMIT_SHA_RESPONSE_BYTES).await?;
    if completeness == CodeHostResultCompleteness::Truncated {
        return Err(CodeHostTransportFailure::ResponseTooLarge);
    }
    let body = body.strip_suffix(b"\n").unwrap_or(&body);
    if body != expected.as_str().as_bytes() {
        return Err(CodeHostTransportFailure::InvalidResponse);
    }
    Ok(expected.clone())
}

async fn repository_contents_response_names_missing_revision(
    response: Response,
    revision: &super::CodeHostRevision,
) -> Result<bool, CodeHostTransportFailure> {
    repository_response_message_names_revision(response, revision, "No commit found for the ref ")
        .await
}

async fn repository_commit_response_names_missing_revision(
    response: Response,
    revision: &super::CodeHostRevision,
) -> Result<bool, CodeHostTransportFailure> {
    repository_response_message_names_revision(response, revision, "No commit found for SHA: ")
        .await
}

async fn repository_response_message_names_revision(
    response: Response,
    revision: &super::CodeHostRevision,
    prefix: &str,
) -> Result<bool, CodeHostTransportFailure> {
    let (body, completeness) =
        read_bounded(response.bytes_stream(), MAX_JSON_RESPONSE_BYTES).await?;
    if completeness == CodeHostResultCompleteness::Truncated {
        return Err(CodeHostTransportFailure::ResponseTooLarge);
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return Ok(false);
    };
    let Some(message) = value
        .as_object()
        .and_then(|object| object.get("message"))
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(false);
    };
    Ok(message
        .strip_prefix(prefix)
        .is_some_and(|named_revision| named_revision == revision.as_str()))
}

async fn bounded_json_page(
    response: Response,
    expected: StatusCode,
    limit: usize,
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
    let (body, body_completeness) = read_bounded(response.bytes_stream(), limit).await?;
    if body_completeness == CodeHostResultCompleteness::Truncated {
        return Err(CodeHostTransportFailure::ResponseTooLarge);
    }
    let value =
        serde_json::from_slice(&body).map_err(|_| CodeHostTransportFailure::InvalidResponse)?;
    Ok((value, completeness))
}

fn bounded_repository_directory_result(
    arguments: &super::RepositoryListDirectoryArguments,
    entries: Vec<RepositoryDirectoryEntry>,
    source_completeness: CodeHostResultCompleteness,
) -> Option<RepositoryListDirectoryResult> {
    let observed_entries = entries.len();
    let mut entries = entries
        .into_iter()
        .take(MAX_RESULT_ITEMS)
        .collect::<Vec<_>>();
    let mut completeness = if source_completeness == CodeHostResultCompleteness::Truncated
        || observed_entries > entries.len()
    {
        CodeHostResultCompleteness::Truncated
    } else {
        CodeHostResultCompleteness::Complete
    };
    loop {
        let encoded_len = RepositoryListDirectoryResult::entries_encoded_len(
            arguments,
            entries.clone(),
            observed_entries,
            completeness,
        )?;
        if encoded_len <= MAX_ENCODED_RESULT_BYTES {
            return RepositoryListDirectoryResult::try_entries(
                arguments,
                entries,
                observed_entries,
                completeness,
            );
        }
        entries.pop()?;
        completeness = CodeHostResultCompleteness::Truncated;
    }
}

fn parse_repository_path_lookup(
    value: &serde_json::Value,
    requested_path: &str,
    completeness: CodeHostResultCompleteness,
) -> Result<RepositoryPathLookup, CodeHostTransportFailure> {
    let object = required_object(value)?;
    ensure_repository_response_path(object, requested_path)?;
    let kind = parse_repository_object_kind(object)?;
    match kind {
        RepositoryObjectKind::File => {
            let blob = required_string(object, "sha")?;
            if !valid_revision(&blob) {
                return Err(CodeHostTransportFailure::InvalidResponse);
            }
            Ok(RepositoryPathLookup::File {
                blob,
                source_bytes: required_u64(object, "size")?,
            })
        }
        RepositoryObjectKind::Directory => {
            let entries = required(object, "entries")?
                .as_array()
                .ok_or(CodeHostTransportFailure::InvalidResponse)?;
            if entries.len() > MAX_OBSERVED_DIRECTORY_ENTRIES {
                return Err(CodeHostTransportFailure::InvalidResponse);
            }
            let completeness = if entries.len() == MAX_OBSERVED_DIRECTORY_ENTRIES {
                CodeHostResultCompleteness::Truncated
            } else {
                completeness
            };
            let entries = entries
                .iter()
                .map(|entry| parse_repository_directory_entry(entry, requested_path))
                .collect::<Result<Vec<_>, _>>()?;
            let unique_paths = entries
                .iter()
                .map(|entry| entry.path())
                .collect::<HashSet<_>>()
                .len()
                == entries.len();
            if !unique_paths {
                return Err(CodeHostTransportFailure::InvalidResponse);
            }
            Ok(RepositoryPathLookup::Directory {
                entries,
                completeness,
            })
        }
        RepositoryObjectKind::Symlink | RepositoryObjectKind::Submodule => {
            Ok(RepositoryPathLookup::Other { kind })
        }
    }
}

fn ensure_repository_response_path(
    object: &serde_json::Map<String, serde_json::Value>,
    requested_path: &str,
) -> Result<(), CodeHostTransportFailure> {
    let returned_path = match object.get("path") {
        None => None,
        Some(serde_json::Value::String(path)) => Some(path.as_str()),
        Some(_) => return Err(CodeHostTransportFailure::InvalidResponse),
    };
    let matches = if requested_path == "." {
        returned_path.is_none_or(|path| path.is_empty() || path == ".")
    } else {
        returned_path == Some(requested_path)
    };
    if matches {
        Ok(())
    } else {
        Err(CodeHostTransportFailure::InvalidResponse)
    }
}

fn parse_repository_object_kind(
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<RepositoryObjectKind, CodeHostTransportFailure> {
    let object_type = required_string(object, "type")?;
    let has_submodule_marker = match object.get("submodule_git_url") {
        Some(serde_json::Value::String(url)) if url.len() <= MAX_REPOSITORY_SUBMODULE_URL_BYTES => {
            true
        }
        Some(serde_json::Value::String(_)) => {
            return Err(CodeHostTransportFailure::InvalidResponse);
        }
        None | Some(serde_json::Value::Null) => false,
        Some(_) => return Err(CodeHostTransportFailure::InvalidResponse),
    };
    match (object_type.as_str(), has_submodule_marker) {
        ("file" | "submodule", true) | ("submodule", false) => Ok(RepositoryObjectKind::Submodule),
        ("file", false) => Ok(RepositoryObjectKind::File),
        ("dir", false) => Ok(RepositoryObjectKind::Directory),
        ("symlink", false) => Ok(RepositoryObjectKind::Symlink),
        ("dir" | "symlink", true) | (_, _) => Err(CodeHostTransportFailure::InvalidResponse),
    }
}

fn parse_repository_directory_entry(
    value: &serde_json::Value,
    parent: &str,
) -> Result<RepositoryDirectoryEntry, CodeHostTransportFailure> {
    let object = required_object(value)?;
    let path = required_string(object, "path")?;
    if !is_immediate_repository_child(parent, &path) {
        return Err(CodeHostTransportFailure::InvalidResponse);
    }
    let kind = parse_repository_object_kind(object)?;
    if kind == RepositoryObjectKind::Symlink
        && required_string(object, "target")?.len() > MAX_REPOSITORY_SYMLINK_TARGET_BYTES
    {
        return Err(CodeHostTransportFailure::InvalidResponse);
    }
    RepositoryDirectoryEntry::try_new(path, kind, omitted_optional_u64(object, "size")?)
        .ok_or(CodeHostTransportFailure::InvalidResponse)
}

async fn select_repository_file_content<S, B, E>(
    mut stream: S,
    line_range: Option<RepositoryLineRange>,
) -> Result<RepositoryFileBody, CodeHostTransportFailure>
where
    S: futures_util::Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
{
    let requested_start = line_range.map_or(1, RepositoryLineRange::start);
    let requested_end = line_range.map(RepositoryLineRange::end);
    let mut current_line = 1_u32;
    let mut retained = Vec::new();
    let mut validated = Vec::new();
    let mut observed_source_bytes = 0_u64;
    let mut observed_selected_bytes = 0_u64;
    let mut completeness = CodeHostResultCompleteness::Complete;
    let mut source_exhausted = true;

    'source: while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| CodeHostTransportFailure::DispatchUnknown)?;
        for byte in chunk.as_ref() {
            observed_source_bytes = observed_source_bytes
                .checked_add(1)
                .ok_or(CodeHostTransportFailure::InvalidResponse)?;
            if line_range.is_some() {
                if validated.len() == MAX_REPOSITORY_FILE_SCAN_BYTES {
                    return Err(CodeHostTransportFailure::ResponseTooLarge);
                }
                validated.push(*byte);
            }
            if current_line >= requested_start
                && requested_end.is_none_or(|end| current_line <= end)
            {
                observed_selected_bytes = observed_selected_bytes
                    .checked_add(1)
                    .ok_or(CodeHostTransportFailure::InvalidResponse)?;
                if retained.len() == MAX_REPOSITORY_FILE_CONTENT_BYTES {
                    completeness = CodeHostResultCompleteness::Truncated;
                    if line_range.is_none() {
                        source_exhausted = false;
                        break 'source;
                    }
                } else {
                    retained.push(*byte);
                }
            }
            if *byte == b'\n' {
                current_line = current_line
                    .checked_add(1)
                    .ok_or(CodeHostTransportFailure::InvalidResponse)?;
            }
        }
    }

    if line_range.is_none() {
        validated.clone_from(&retained);
    }
    let validation_mode = match line_range {
        Some(_) => RepositoryFileValidationMode::EntireSource,
        None => RepositoryFileValidationMode::RetainedContentOnly,
    };
    let kind = repository_file_body_kind(
        &validated,
        retained,
        requested_start,
        completeness,
        validation_mode,
    )?;
    Ok(RepositoryFileBody {
        kind,
        observed_source_bytes,
        observed_selected_bytes,
        source_exhausted,
    })
}

fn repository_file_body_kind(
    validated: &[u8],
    mut retained: Vec<u8>,
    requested_start: u32,
    completeness: CodeHostResultCompleteness,
    validation_mode: RepositoryFileValidationMode,
) -> Result<RepositoryFileBodyKind, CodeHostTransportFailure> {
    let validated_as_binary = match validation_mode {
        RepositoryFileValidationMode::RetainedContentOnly => validated.contains(&b'\0'),
        RepositoryFileValidationMode::EntireSource => {
            validated.contains(&b'\0') || std::str::from_utf8(validated).is_err()
        }
    };
    if validated_as_binary {
        return Ok(RepositoryFileBodyKind::Binary);
    }
    let content = match std::str::from_utf8(&retained) {
        Ok(content) => content.to_owned(),
        Err(error)
            if completeness == CodeHostResultCompleteness::Truncated
                && error.error_len().is_none() =>
        {
            retained.truncate(error.valid_up_to());
            std::str::from_utf8(&retained)
                .map(str::to_owned)
                .map_err(|_| CodeHostTransportFailure::InvalidResponse)?
        }
        Err(_) => return Ok(RepositoryFileBodyKind::Binary),
    };
    let returned_lines = repository_content_line_count(&content)?;
    let start_line = (!content.is_empty()).then_some(requested_start);
    let end_line = start_line
        .zip(returned_lines.checked_sub(1))
        .and_then(|(start, span)| start.checked_add(span));
    let last_line_complete = content.is_empty()
        || content.ends_with('\n')
        || completeness == CodeHostResultCompleteness::Complete;
    Ok(RepositoryFileBodyKind::Text(RepositoryFileSelection {
        content,
        start_line,
        end_line,
        returned_lines,
        last_line_complete,
        completeness,
    }))
}

fn repository_content_line_count(content: &str) -> Result<u32, CodeHostTransportFailure> {
    if content.is_empty() {
        return Ok(0);
    }
    let terminated_lines = content.bytes().filter(|byte| *byte == b'\n').count();
    let lines = terminated_lines + usize::from(!content.ends_with('\n'));
    lines
        .try_into()
        .map_err(|_| CodeHostTransportFailure::InvalidResponse)
}

fn omitted_optional_u64(
    object: &serde_json::Map<String, serde_json::Value>,
    member: &str,
) -> Result<Option<u64>, CodeHostTransportFailure> {
    match object.get(member) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(value)) => value
            .as_u64()
            .map(Some)
            .ok_or(CodeHostTransportFailure::InvalidResponse),
        Some(_) => Err(CodeHostTransportFailure::InvalidResponse),
    }
}

fn ensure_review_gate_snapshot_unchanged(
    initial_stack: &StackStateResult,
    current_stack: &StackStateResult,
    initial_convergence: &ConvergenceStateResult,
    current_convergence: &ConvergenceStateResult,
) -> Result<(), CodeHostTransportFailure> {
    if initial_stack != current_stack || initial_convergence != current_convergence {
        return Err(CodeHostTransportFailure::InvalidResponse);
    }
    Ok(())
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

#[derive(Clone, Debug, Eq, PartialEq)]
enum FilePatchPageOutcome {
    Found(FilePatchResult),
    Continue,
    NotFound,
}

fn inspect_file_patch_page(
    value: &serde_json::Value,
    completeness: CodeHostResultCompleteness,
    path: &str,
) -> Result<FilePatchPageOutcome, CodeHostTransportFailure> {
    let array = value
        .as_array()
        .ok_or(CodeHostTransportFailure::InvalidResponse)?;
    let parsed = array
        .iter()
        .map(parse_changed_file)
        .collect::<Result<Vec<_>, _>>()?;
    if let Some((file, patch)) = parsed
        .into_iter()
        .find(|(file, _patch)| file.path() == path)
    {
        let result = FilePatchResult::try_new(file, patch)
            .ok_or(CodeHostTransportFailure::InvalidResponse)?;
        return Ok(FilePatchPageOutcome::Found(result));
    }
    Ok(match completeness {
        CodeHostResultCompleteness::Truncated => FilePatchPageOutcome::Continue,
        CodeHostResultCompleteness::Complete => FilePatchPageOutcome::NotFound,
    })
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

async fn with_read_operation_timeout<T>(
    timeout: Duration,
    operation: impl Future<Output = Result<T, CodeHostTransportFailure>>,
) -> Result<T, CodeHostTransportFailure> {
    tokio::time::timeout(timeout, operation)
        .await
        .map_err(|_| CodeHostTransportFailure::DispatchUnknown)?
}

fn remaining_exchange_timeout(elapsed: Duration) -> Result<Duration, CodeHostTransportFailure> {
    DEFAULT_TIMEOUT
        .checked_sub(elapsed)
        .filter(|remaining| !remaining.is_zero())
        .ok_or(CodeHostTransportFailure::DispatchUnknown)
}

/// Bounds a mutation phase to the whole-exchange budget its ownership
/// confirmation has not consumed, so confirmation and mutation together
/// respect the transport's single 30-second exchange timeout. Exhaustion
/// here proves the mutation was never dispatched; a timeout after dispatch
/// is transport loss with dispatch unknown, which the executor classifies as
/// commit-ambiguous for a mutating declaration.
fn remaining_mutation_budget(elapsed: Duration) -> Result<Duration, CodeHostTransportFailure> {
    DEFAULT_TIMEOUT
        .checked_sub(elapsed)
        .filter(|remaining| !remaining.is_zero())
        .ok_or(CodeHostTransportFailure::MutationNotDispatched)
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

/// What the ownership query proved about the node a thread mutation names.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ThreadOwnershipEvidence {
    /// The node is a review thread owned by exactly this change request.
    Thread { number: u64, repository: String },
    /// The identity resolved to no node, or to a node that is not a review
    /// thread, so it cannot belong to any change request.
    NotAThread,
}

/// Reads ownership evidence from the bounded `ThreadOwnership` response.
///
/// A `node` lookup nulls its field for definitive absence and for transient
/// resolver failures alike, so an evaluated `data.node: null` is definitive
/// absence evidence only when every accompanying error entry carries the code
/// host's typed `NOT_FOUND` classification (or no error accompanies it); any
/// other error beside the null proves nothing about the thread and reports
/// only that the mutation was not dispatched. A node of another type carries
/// absence weight itself: it cannot belong to any change request as a review
/// thread. A response without an evaluated `data` member never ran the query,
/// so it likewise proves only that the mutation was not dispatched; a `data`
/// member whose shape violates the query is a malformed bounded response.
fn parse_thread_ownership(
    value: &serde_json::Value,
) -> Result<ThreadOwnershipEvidence, CodeHostTransportFailure> {
    let data = match value.get("data") {
        Some(data) if !data.is_null() => data,
        Some(_) | None => return Err(CodeHostTransportFailure::MutationNotDispatched),
    };
    let node = match required(required_object(data)?, "node")? {
        serde_json::Value::Null => {
            return if node_errors_prove_absence(value) {
                Ok(ThreadOwnershipEvidence::NotAThread)
            } else {
                Err(CodeHostTransportFailure::MutationNotDispatched)
            };
        }
        node => required_object(node)?,
    };
    if required_string(node, "__typename")? != "PullRequestReviewThread" {
        return Ok(ThreadOwnershipEvidence::NotAThread);
    }
    let change_request = required_object(required(node, "pullRequest")?)?;
    let repository = required_object(required(change_request, "repository")?)?;
    Ok(ThreadOwnershipEvidence::Thread {
        number: required_u64(change_request, "number")?,
        repository: required_string(repository, "nameWithOwner")?,
    })
}

/// Whether the error entries beside an evaluated `data.node: null` prove the
/// identity names no node. Reading the closed `type` classification field
/// parallels the REST absence proof in
/// [`repository_commit_response_names_missing_revision`]; no error text
/// enters a result or a sanitized detail. An empty or absent error array
/// beside the evaluated null is the query's own answer that nothing
/// resolved, and an entry without the classification field proves nothing.
fn node_errors_prove_absence(value: &serde_json::Value) -> bool {
    match value.get("errors") {
        None => true,
        Some(serde_json::Value::Array(errors)) => errors.iter().all(|error| {
            error.get("type").and_then(serde_json::Value::as_str) == Some("NOT_FOUND")
        }),
        Some(_) => false,
    }
}

/// Whether complete ownership evidence places the thread inside the named
/// change request. GitHub resolves repository spellings case-insensitively,
/// so the spelling comparison ignores ASCII case rather than inventing a
/// stricter repository identity than the code host enforces; the
/// change-request number has no such latitude and must match exactly.
fn thread_in_change_request(
    evidence: &ThreadOwnershipEvidence,
    repository: &CodeHostRepository,
    number: CodeHostChangeRequestNumber,
) -> bool {
    match evidence {
        ThreadOwnershipEvidence::NotAThread => false,
        ThreadOwnershipEvidence::Thread {
            number: owning_number,
            repository: owning_repository,
        } => {
            *owning_number == u64::from(number.get())
                && owning_repository.eq_ignore_ascii_case(repository.as_str())
        }
    }
}

/// Shapes a failure observed while establishing thread ownership, before the
/// mutation existed as a request. The ownership check is a read, so its
/// failures keep the classification a read would receive: a definitive
/// answer keeps its meaning, a bounded response the contract refuses is the
/// code host's answer and ends the attempt as a known failure, and
/// transport loss proves only that the mutation was never dispatched — it is
/// never presented as a commit-ambiguous mutation.
const fn ownership_evidence_failure(failure: CodeHostTransportFailure) -> CodeHostTransportFailure {
    match failure {
        CodeHostTransportFailure::InvalidCredential
        | CodeHostTransportFailure::Rejected
        | CodeHostTransportFailure::ThreadNotInChangeRequest
        | CodeHostTransportFailure::MutationNotDispatched => failure,
        CodeHostTransportFailure::InvalidResponse | CodeHostTransportFailure::ResponseTooLarge => {
            CodeHostTransportFailure::Rejected
        }
        CodeHostTransportFailure::NotFound
        | CodeHostTransportFailure::ChangeRequestRevisionChanged
        | CodeHostTransportFailure::DispatchUnknown => {
            CodeHostTransportFailure::MutationNotDispatched
        }
    }
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

#[cfg_attr(test, derive(Clone))]
#[derive(Debug, Eq, PartialEq)]
struct ChangeRequestDiffRevision {
    base: String,
    head: String,
}

fn parse_change_request_diff_revision(
    value: &serde_json::Value,
    expected_number: CodeHostChangeRequestNumber,
) -> Result<ChangeRequestDiffRevision, CodeHostTransportFailure> {
    let object = required_object(value)?;
    let number: u32 = required_u64(object, "number")?
        .try_into()
        .map_err(|_| CodeHostTransportFailure::InvalidResponse)?;
    if number != expected_number.get() {
        return Err(CodeHostTransportFailure::InvalidResponse);
    }
    let base = required_object(required(object, "base")?)?;
    let base_revision = required_string(base, "sha")?;
    let head = required_object(required(object, "head")?)?;
    let head_revision = required_string(head, "sha")?;
    if !valid_revision(&base_revision) || !valid_revision(&head_revision) {
        return Err(CodeHostTransportFailure::InvalidResponse);
    }
    Ok(ChangeRequestDiffRevision {
        base: base_revision,
        head: head_revision,
    })
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
    use crate::{
        CodeHostRepository, ReviewerVerdictEvidence, ReviewerVerdictFields, ReviewerVerdictStatus,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    const FILE_PATCH_REPOSITORY: &str = "owner/repository";
    const FILE_PATCH_NUMBER: u32 = 17;
    const FILE_PATCH_TARGET_PATH: &str = "src/target.rs";
    const FILE_PATCH_BASE_REVISION: &str = "1111111111111111111111111111111111111111";
    const FILE_PATCH_HEAD_REVISION: &str = "2222222222222222222222222222222222222222";
    const FILE_PATCH_MOVED_REVISION: &str = "3333333333333333333333333333333333333333";
    const FILE_PATCH_SERVER_TIMEOUT: Duration = Duration::from_secs(5);
    const REPOSITORY_REVISION: &str = "4444444444444444444444444444444444444444";
    const REPOSITORY_BLOB: &str = "5555555555555555555555555555555555555555";
    fn missing_revision_body() -> Vec<u8> {
        serde_json::json!({
            "message": format!("No commit found for the ref {REPOSITORY_REVISION}"),
        })
        .to_string()
        .into_bytes()
    }

    fn missing_revision_sha_body() -> Vec<u8> {
        serde_json::json!({
            "message": format!("No commit found for SHA: {REPOSITORY_REVISION}"),
        })
        .to_string()
        .into_bytes()
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FilePatchRevisionTransitionInput {
        before: ChangeRequestDiffRevision,
        after: ChangeRequestDiffRevision,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct FilePatchRevisionChangeServerObservation {
        transition: FilePatchRevisionTransitionInput,
        requests: [String; 4],
    }

    fn repository() -> CodeHostRepository {
        CodeHostRepository::try_new(String::from("owner/repository"))
            .expect("fixture repository is admitted")
    }

    fn gate_stack_state() -> StackStateResult {
        const BASE_REVISION: &str = "1111111111111111111111111111111111111111";
        const HEAD_REVISION: &str = "2222222222222222222222222222222222222222";
        StackStateResult::try_new(StackStateFields {
            number: 17,
            base_ref: String::from("main"),
            base_revision: String::from(BASE_REVISION),
            head_ref: String::from("feature"),
            head_revision: String::from(HEAD_REVISION),
            default_ref: String::from("main"),
            default_revision: String::from(BASE_REVISION),
            base_commits_not_in_head: 0,
            main_commits_not_in_base: 0,
            children: Vec::new(),
            children_truncated: false,
            children_next_cursor: None,
        })
        .expect("fixture stack evidence is admitted")
    }

    fn gate_convergence_state(thread: Option<ReviewThreadIdentity>) -> ConvergenceStateResult {
        let reviewer = ReviewerVerdictEvidence::try_new(ReviewerVerdictFields {
            status: ReviewerVerdictStatus::Missing,
            reviewed_revision: None,
            reviewed_at: None,
            starvation_after_verdict: false,
            latest_starvation_at: None,
            latest_review_request_at: None,
            review_request_in_flight: false,
            source_truncated: false,
            comments_previous_cursor: None,
            reviews_previous_cursor: None,
        })
        .expect("fixture reviewer evidence is admitted");
        ConvergenceStateResult::try_new(ConvergenceStateFields {
            head_revision: String::from("2222222222222222222222222222222222222222"),
            mergeable_state: String::from("MERGEABLE"),
            ci_rollup_state: Some(String::from("SUCCESS")),
            checks: Vec::new(),
            checks_truncated: false,
            checks_next_cursor: None,
            unresolved_threads: thread.clone().into_iter().collect(),
            open_escalations: Vec::new(),
            buried_escalations: Vec::new(),
            undispositioned_threads: thread.into_iter().collect(),
            threads_truncated: false,
            threads_next_cursor: None,
            reviewer,
        })
        .expect("fixture convergence evidence is admitted")
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

    /// A forty-hex ref name cannot impersonate the exact commit argument when
    /// GitHub resolves it to a different object identity.
    #[tokio::test]
    async fn repository_hex_named_ref_cannot_impersonate_exact_commit() {
        let (transport, listener) = repository_test_transport().await;
        let server = tokio::spawn(async move {
            serve_test_response(
                &listener,
                TestHttpResponse::Raw {
                    body: FILE_PATCH_MOVED_REVISION.as_bytes(),
                },
            )
            .await
        });
        let failure = transport
            .repository_read_file(
                repository_read_arguments("src/lib.rs", None),
                &test_credential(),
            )
            .await
            .expect_err("a differently resolved ref cannot produce repository evidence");
        let request = repository_server_result(server).await;

        assert_eq!(failure, CodeHostTransportFailure::InvalidResponse);
        assert_eq!(request, revision_request());
    }

    /// A path present only at a moving head is still absent from the exact
    /// reviewed revision, and both requests remain pinned to that revision.
    #[tokio::test]
    async fn repository_file_at_head_does_not_replace_requested_revision() {
        let (transport, listener) = repository_test_transport().await;
        let server = tokio::spawn(async move {
            let revision_request = serve_exact_revision(&listener).await;
            let path_request = serve_test_response(
                &listener,
                TestHttpResponse::Json {
                    status: "404 Not Found",
                    body: b"{}",
                },
            )
            .await;
            [revision_request, path_request]
        });
        let arguments = repository_read_arguments("src/lib.rs", None);
        let result = transport
            .repository_read_file(arguments.clone(), &test_credential())
            .await
            .expect("an absent exact-revision path is a typed result");
        let requests = repository_server_result(server).await;

        assert_eq!(
            result.into_json_value(),
            serde_json::json!({
                "outcome": "path_not_found",
                "path": arguments.path().as_str(),
                "revision": REPOSITORY_REVISION,
                "truncated": false,
            })
        );
        assert_eq!(requests, path_lookup_requests(arguments.path().as_str()));
    }

    /// A revision the host does not recognize is distinct from a path absent at
    /// a revision the host does recognize.
    #[tokio::test]
    async fn repository_missing_revision_is_typed_separately() {
        let (transport, listener) = repository_test_transport().await;
        let response_body = missing_revision_body();
        let server = tokio::spawn(async move {
            let revision_request = serve_test_response(
                &listener,
                TestHttpResponse::Json {
                    status: "404 Not Found",
                    body: b"{}",
                },
            )
            .await;
            let path_request = serve_test_response(
                &listener,
                TestHttpResponse::Json {
                    status: "404 Not Found",
                    body: &response_body,
                },
            )
            .await;
            [revision_request, path_request]
        });
        let arguments = repository_read_arguments("src/lib.rs", None);
        let result = transport
            .repository_read_file(arguments.clone(), &test_credential())
            .await
            .expect("an absent exact revision is a typed result");
        let requests = repository_server_result(server).await;

        assert_eq!(
            result.into_json_value(),
            serde_json::json!({
                "outcome": "revision_not_found",
                "path": arguments.path().as_str(),
                "revision": arguments.revision().as_str(),
                "truncated": false,
            })
        );
        assert_eq!(requests, path_lookup_requests(arguments.path().as_str()));
    }

    /// GitHub commit lookup can report an unknown well-formed SHA as a
    /// bounded 422 whose message names that exact SHA.
    #[tokio::test]
    async fn repository_commit_unprocessable_missing_sha_is_typed_separately() {
        let (transport, listener) = repository_test_transport().await;
        let response_body = missing_revision_sha_body();
        let server = tokio::spawn(async move {
            serve_test_response(
                &listener,
                TestHttpResponse::Json {
                    status: "422 Unprocessable Entity",
                    body: &response_body,
                },
            )
            .await
        });
        let arguments = repository_read_arguments("src/lib.rs", None);
        let result = transport
            .repository_read_file(arguments.clone(), &test_credential())
            .await
            .expect("an exactly named absent commit is a typed result");
        let request = repository_server_result(server).await;

        assert_eq!(
            result.into_json_value(),
            serde_json::json!({
                "outcome": "revision_not_found",
                "path": arguments.path().as_str(),
                "revision": arguments.revision().as_str(),
                "truncated": false,
            })
        );
        assert_eq!(request, revision_request());
    }

    /// An unrelated validation response remains a host rejection rather than
    /// being converted into evidence that the requested revision is absent.
    #[tokio::test]
    async fn repository_unrelated_commit_unprocessable_is_rejected() {
        let (transport, listener) = repository_test_transport().await;
        let server = tokio::spawn(async move {
            serve_test_response(
                &listener,
                TestHttpResponse::Json {
                    status: "422 Unprocessable Entity",
                    body: br#"{"message":"Validation Failed"}"#,
                },
            )
            .await
        });
        let failure = transport
            .repository_read_file(
                repository_read_arguments("src/lib.rs", None),
                &test_credential(),
            )
            .await
            .expect_err("an unrelated validation failure cannot prove revision absence");
        let request = repository_server_result(server).await;

        assert_eq!(failure, CodeHostTransportFailure::Rejected);
        assert_eq!(request, revision_request());
    }

    /// An empty repository's commit probe returns the distinct conflict that
    /// proves the requested revision is absent.
    #[tokio::test]
    async fn repository_empty_conflict_is_a_missing_revision_result() {
        let (transport, listener) = repository_test_transport().await;
        let server = tokio::spawn(async move {
            serve_test_response(
                &listener,
                TestHttpResponse::Json {
                    status: "409 Conflict",
                    body: b"{}",
                },
            )
            .await
        });
        let arguments = repository_read_arguments("src/lib.rs", None);
        let result = transport
            .repository_read_file(arguments.clone(), &test_credential())
            .await
            .expect("a revision in an empty visible repository is absent");
        let requests = repository_server_result(server).await;

        assert_eq!(
            result.into_json_value(),
            serde_json::json!({
                "outcome": "revision_not_found",
                "path": arguments.path().as_str(),
                "revision": arguments.revision().as_str(),
                "truncated": false,
            })
        );
        assert_eq!(requests, revision_request());
    }

    /// Metadata-only access leaves both content-bearing probes ambiguous, so a
    /// generic 404 cannot prove revision absence.
    #[tokio::test]
    async fn repository_metadata_only_access_is_not_a_missing_revision_result() {
        let (transport, listener) = repository_test_transport().await;
        let server = tokio::spawn(async move {
            let revision_request = serve_test_response(
                &listener,
                TestHttpResponse::Json {
                    status: "404 Not Found",
                    body: b"{}",
                },
            )
            .await;
            let path_request = serve_test_response(
                &listener,
                TestHttpResponse::Json {
                    status: "404 Not Found",
                    body: b"{}",
                },
            )
            .await;
            [revision_request, path_request]
        });
        let failure = transport
            .repository_read_file(
                repository_read_arguments("src/lib.rs", None),
                &test_credential(),
            )
            .await
            .expect_err("metadata-only access cannot prove revision absence");
        let requests = repository_server_result(server).await;

        assert_eq!(failure, CodeHostTransportFailure::Rejected);
        assert_eq!(requests, path_lookup_requests("src/lib.rs"));
    }

    /// A definitive host rejection remains failed execution rather than being
    /// mislabeled as either path or revision absence.
    #[tokio::test]
    async fn repository_rejected_request_is_not_a_not_found_result() {
        let (transport, listener) = repository_test_transport().await;
        let server = tokio::spawn(async move {
            serve_test_response(
                &listener,
                TestHttpResponse::Json {
                    status: "403 Forbidden",
                    body: b"{}",
                },
            )
            .await
        });
        let failure = transport
            .repository_read_file(
                repository_read_arguments("src/lib.rs", None),
                &test_credential(),
            )
            .await
            .expect_err("a rejected request cannot produce absence evidence");
        let request = repository_server_result(server).await;

        assert_eq!(failure, CodeHostTransportFailure::Rejected);
        assert_eq!(request, revision_request());
    }

    /// Reading a directory as a file yields its repository object kind without
    /// issuing a blob request.
    #[tokio::test]
    async fn repository_file_read_reports_directory_path() {
        let arguments = repository_read_arguments("src", None);
        let (transport, listener) = repository_test_transport().await;
        let directory =
            repository_directory_value(arguments.path().as_str(), Vec::new()).to_string();
        let server = tokio::spawn(async move {
            serve_exact_revision_then(
                &listener,
                TestHttpResponse::Json {
                    status: "200 OK",
                    body: directory.as_bytes(),
                },
            )
            .await
        });
        let result = transport
            .repository_read_file(arguments.clone(), &test_credential())
            .await
            .expect("a directory path is a typed non-file result");
        let request = repository_server_result(server).await;

        assert_eq!(
            result.into_json_value(),
            serde_json::json!({
                "object_type": "directory",
                "outcome": "not_a_file",
                "path": arguments.path().as_str(),
                "revision": arguments.revision().as_str(),
                "truncated": false,
            })
        );
        assert_eq!(request, path_lookup_requests(arguments.path().as_str()));
    }

    /// Invalid UTF-8 or NUL-bearing blob content is identified as binary and
    /// never lossy-decoded into review evidence.
    #[tokio::test]
    async fn repository_file_read_reports_binary_blob() {
        let source = vec![0xff, b'\0', 0xfe];
        let arguments = repository_read_arguments("assets/image.bin", None);
        let path = String::from(arguments.path().as_str());
        let (transport, listener) = repository_test_transport().await;
        let metadata = repository_file_value(path.as_str(), source.len()).to_string();
        let source_for_server = source.clone();
        let server = tokio::spawn(async move {
            let revision_request = serve_exact_revision(&listener).await;
            let path_request = serve_test_response(
                &listener,
                TestHttpResponse::Json {
                    status: "200 OK",
                    body: metadata.as_bytes(),
                },
            )
            .await;
            let blob_request = serve_test_response(
                &listener,
                TestHttpResponse::Raw {
                    body: &source_for_server,
                },
            )
            .await;
            [revision_request, path_request, blob_request]
        });
        let result = transport
            .repository_read_file(arguments, &test_credential())
            .await
            .expect("binary content is a typed result");
        let requests = repository_server_result(server).await;

        assert_eq!(
            result.into_json_value(),
            serde_json::json!({
                "outcome": "binary_file",
                "path": path.as_str(),
                "revision": REPOSITORY_REVISION,
                "source_bytes": source.len(),
                "truncated": false,
            })
        );
        assert_eq!(requests, file_read_requests(path.as_str()));
    }

    /// An inclusive line range returns only those complete lines while every
    /// physical request remains pinned to immutable object identities.
    #[tokio::test]
    async fn repository_file_read_returns_exact_line_range() {
        const REQUESTED_START: u32 = 2;
        const REQUESTED_END: u32 = 2;
        let source = b"first\nsecond\nthird\n".to_vec();
        let (transport, listener) = repository_test_transport().await;
        let metadata = repository_file_value("src/lines.rs", source.len()).to_string();
        let source_for_server = source.clone();
        let server = tokio::spawn(async move {
            let revision_request = serve_exact_revision(&listener).await;
            let path_request = serve_test_response(
                &listener,
                TestHttpResponse::Json {
                    status: "200 OK",
                    body: metadata.as_bytes(),
                },
            )
            .await;
            let blob_request = serve_test_response(
                &listener,
                TestHttpResponse::Raw {
                    body: &source_for_server,
                },
            )
            .await;
            [revision_request, path_request, blob_request]
        });
        let result = transport
            .repository_read_file(
                repository_read_arguments(
                    "src/lines.rs",
                    Some(serde_json::json!({
                        "end": REQUESTED_END,
                        "start": REQUESTED_START,
                    })),
                ),
                &test_credential(),
            )
            .await
            .expect("an exact line range returns typed text")
            .into_json_value();
        let requests = repository_server_result(server).await;

        assert_eq!(result["outcome"], "content");
        assert_eq!(result["content"], "second\n");
        assert_eq!(result["source_bytes"], source.len());
        assert_eq!(result["returned_bytes"], "second\n".len());
        assert_eq!(result["returned_lines"], 1);
        assert_eq!(result["start_line"], REQUESTED_START);
        assert_eq!(result["end_line"], REQUESTED_END);
        assert_eq!(result["last_line_complete"], true);
        assert_eq!(result["truncated"], false);
        assert_eq!(
            result["requested_line_range"],
            serde_json::json!({
                "end": REQUESTED_END,
                "start": REQUESTED_START,
            })
        );
        assert_eq!(requests, file_read_requests("src/lines.rs"));
    }

    /// Ranged truncation is backed by source bytes observed inside the
    /// requested selection rather than unrelated bytes before it.
    #[tokio::test]
    async fn repository_file_range_truncation_is_witnessed_inside_selection() {
        const REQUESTED_START: u32 = 2;
        const REQUESTED_END: u32 = 3;
        let selected_prefix = format!("{}\n", "x".repeat(MAX_REPOSITORY_FILE_CONTENT_BYTES - 1));
        let source = format!("first\n{selected_prefix}y").into_bytes();
        let (transport, listener) = repository_test_transport().await;
        let metadata = repository_file_value("src/lines.rs", source.len()).to_string();
        let source_for_server = source.clone();
        let server = tokio::spawn(async move {
            let revision_request = serve_exact_revision(&listener).await;
            let path_request = serve_test_response(
                &listener,
                TestHttpResponse::Json {
                    status: "200 OK",
                    body: metadata.as_bytes(),
                },
            )
            .await;
            let blob_request = serve_test_response(
                &listener,
                TestHttpResponse::Raw {
                    body: &source_for_server,
                },
            )
            .await;
            [revision_request, path_request, blob_request]
        });
        let result = transport
            .repository_read_file(
                repository_read_arguments(
                    "src/lines.rs",
                    Some(serde_json::json!({
                        "end": REQUESTED_END,
                        "start": REQUESTED_START,
                    })),
                ),
                &test_credential(),
            )
            .await
            .expect("selected source excess produces honest truncation")
            .into_json_value();
        let requests = repository_server_result(server).await;

        assert_eq!(result["source_bytes"], source.len());
        assert_eq!(result["returned_bytes"], MAX_REPOSITORY_FILE_CONTENT_BYTES);
        assert_eq!(result["returned_lines"], 1);
        assert_eq!(result["start_line"], REQUESTED_START);
        assert_eq!(result["end_line"], REQUESTED_START);
        assert_eq!(result["last_line_complete"], true);
        assert_eq!(result["truncated"], true);
        assert_eq!(requests, file_read_requests("src/lines.rs"));
    }

    /// Ranged text classification scans the complete bounded blob so binary
    /// bytes outside the selected lines cannot be hidden by a text prefix.
    #[tokio::test]
    async fn repository_file_line_range_validates_the_whole_blob_as_binary() {
        const REQUESTED_START: u32 = 1;
        const REQUESTED_END: u32 = 1;
        let source = b"first\nsecond\nthird\xff".to_vec();
        let (transport, listener) = repository_test_transport().await;
        let metadata = repository_file_value("src/mixed.rs", source.len()).to_string();
        let source_for_server = source.clone();
        let server = tokio::spawn(async move {
            let revision_request = serve_exact_revision(&listener).await;
            let path_request = serve_test_response(
                &listener,
                TestHttpResponse::Json {
                    status: "200 OK",
                    body: metadata.as_bytes(),
                },
            )
            .await;
            let blob_request = serve_test_response(
                &listener,
                TestHttpResponse::Raw {
                    body: &source_for_server,
                },
            )
            .await;
            [revision_request, path_request, blob_request]
        });
        let result = transport
            .repository_read_file(
                repository_read_arguments(
                    "src/mixed.rs",
                    Some(serde_json::json!({
                        "end": REQUESTED_END,
                        "start": REQUESTED_START,
                    })),
                ),
                &test_credential(),
            )
            .await
            .expect("binary content outside the selection is still typed")
            .into_json_value();
        let requests = repository_server_result(server).await;

        assert_eq!(
            result,
            serde_json::json!({
                "outcome": "binary_file",
                "path": "src/mixed.rs",
                "revision": REPOSITORY_REVISION,
                "source_bytes": source.len(),
                "truncated": false,
            })
        );
        assert_eq!(requests, file_read_requests("src/mixed.rs"));
    }

    /// GitHub cannot serve line-addressed blob ranges, so an auto-approved
    /// request refuses oversized ingress before issuing the blob request.
    #[tokio::test]
    async fn repository_file_oversized_line_range_is_typed_without_blob_download() {
        const REQUESTED_START: u32 = 10;
        const REQUESTED_END: u32 = 10;
        const SOURCE_BYTES: usize = MAX_REPOSITORY_FILE_SCAN_BYTES + 1;
        let (transport, listener) = repository_test_transport().await;
        let metadata = repository_file_value("src/huge.rs", SOURCE_BYTES).to_string();
        let server = tokio::spawn(async move {
            serve_exact_revision_then(
                &listener,
                TestHttpResponse::Json {
                    status: "200 OK",
                    body: metadata.as_bytes(),
                },
            )
            .await
        });
        let result = transport
            .repository_read_file(
                repository_read_arguments(
                    "src/huge.rs",
                    Some(serde_json::json!({
                        "end": REQUESTED_END,
                        "start": REQUESTED_START,
                    })),
                ),
                &test_credential(),
            )
            .await
            .expect("oversized ranged ingress is an honest typed outcome")
            .into_json_value();
        let request = repository_server_result(server).await;

        assert_eq!(
            result,
            serde_json::json!({
                "outcome": "line_range_unavailable",
                "path": "src/huge.rs",
                "requested_line_range": {
                    "end": REQUESTED_END,
                    "start": REQUESTED_START,
                },
                "revision": REPOSITORY_REVISION,
                "scan_limit_bytes": MAX_REPOSITORY_FILE_SCAN_BYTES,
                "source_bytes": SOURCE_BYTES,
                "truncated": true,
            })
        );
        assert_eq!(request, path_lookup_requests("src/huge.rs"));
    }

    /// A source larger than the retained text bound exposes every count and the
    /// partial final-line posture alongside explicit truncation.
    #[tokio::test]
    async fn repository_file_read_reports_honest_truncation() {
        let source = vec![b'x'; MAX_REPOSITORY_FILE_CONTENT_BYTES + 1];
        let (transport, listener) = repository_test_transport().await;
        let metadata = repository_file_value("src/large.rs", source.len()).to_string();
        let source_for_server = source.clone();
        let server = tokio::spawn(async move {
            let revision_request = serve_exact_revision(&listener).await;
            let path_request = serve_test_response(
                &listener,
                TestHttpResponse::Json {
                    status: "200 OK",
                    body: metadata.as_bytes(),
                },
            )
            .await;
            let blob_request = serve_test_response(
                &listener,
                TestHttpResponse::Raw {
                    body: &source_for_server,
                },
            )
            .await;
            [revision_request, path_request, blob_request]
        });
        let result = transport
            .repository_read_file(
                repository_read_arguments("src/large.rs", None),
                &test_credential(),
            )
            .await
            .expect("oversized text returns a typed truncated prefix")
            .into_json_value();
        let requests = repository_server_result(server).await;

        assert_eq!(result["outcome"], "content");
        assert_eq!(result["source_bytes"], source.len());
        assert_eq!(result["returned_bytes"], MAX_REPOSITORY_FILE_CONTENT_BYTES);
        assert_eq!(result["returned_lines"], 1);
        assert_eq!(result["start_line"], 1);
        assert_eq!(result["end_line"], 1);
        assert_eq!(result["last_line_complete"], false);
        assert_eq!(result["truncated"], true);
        assert_eq!(
            result["content"]
                .as_str()
                .expect("typed content remains text")
                .len(),
            MAX_REPOSITORY_FILE_CONTENT_BYTES
        );
        assert_eq!(requests, file_read_requests("src/large.rs"));
    }

    /// Directory entry count alone cannot admit JSON whose escaped paths exceed
    /// the aggregate result budget; the retained prefix exposes that truncation.
    #[test]
    fn repository_directory_listing_respects_encoded_result_budget() {
        const OBSERVED_ENTRIES: usize = MAX_RESULT_ITEMS;
        let arguments = repository_list_arguments("src");
        let result = bounded_repository_directory_result(
            &arguments,
            repository_escaping_directory_entries(OBSERVED_ENTRIES),
            CodeHostResultCompleteness::Complete,
        )
        .expect("a directory prefix fits after encoded-budget truncation")
        .into_value();
        let encoded = serde_json::to_vec(&result).expect("typed result encodes");

        assert_eq!(result["outcome"], "entries");
        assert_eq!(result["observed_entries"], OBSERVED_ENTRIES);
        assert!(
            result["returned_entries"]
                .as_u64()
                .expect("returned entry count is numeric")
                < OBSERVED_ENTRIES as u64,
            "encoded-budget truncation must remove at least one observed entry"
        );
        assert_eq!(result["truncated"], true);
        assert!(
            encoded.len() <= MAX_ENCODED_RESULT_BYTES,
            "retained directory output must fit the shared encoded-result budget"
        );
    }

    /// The contents endpoint cannot prove completeness when its fixed
    /// directory-entry ceiling is reached, even without a pagination header.
    #[test]
    fn repository_directory_host_boundary_is_never_complete() {
        let directory = repository_directory_value(
            "src",
            repository_directory_entries(MAX_OBSERVED_DIRECTORY_ENTRIES),
        );
        let completeness = repository_directory_lookup_completeness(&directory)
            .expect("a host-boundary directory response remains typed");

        assert_eq!(completeness, CodeHostResultCompleteness::Truncated);
    }

    /// A present repository-root response path must retain its documented string type.
    #[test]
    fn repository_root_rejects_a_non_string_response_path() {
        let value = serde_json::json!({
            "entries": [],
            "path": 17,
            "type": "dir",
        });

        assert_eq!(
            parse_repository_path_lookup(&value, ".", CodeHostResultCompleteness::Complete),
            Err(CodeHostTransportFailure::InvalidResponse)
        );
    }

    /// GitHub's legacy submodule marker must be either absent, null, or a string.
    #[test]
    fn repository_entry_rejects_a_non_string_submodule_marker() {
        let value = serde_json::json!({
            "path": "vendor/dependency",
            "submodule_git_url": 17,
            "type": "file",
        });

        assert_eq!(
            parse_repository_path_lookup(
                &value,
                "vendor/dependency",
                CodeHostResultCompleteness::Complete,
            ),
            Err(CodeHostTransportFailure::InvalidResponse)
        );
    }

    /// A submodule URL marker is admitted under its own byte bound so every
    /// valid directory entry remains covered by the aggregate ingress cap.
    #[test]
    fn repository_entry_bounds_the_submodule_url_marker() {
        let admitted = serde_json::json!({
            "path": "vendor/dependency",
            "submodule_git_url": "x".repeat(MAX_REPOSITORY_SUBMODULE_URL_BYTES),
            "type": "file",
        });
        let rejected = serde_json::json!({
            "path": "vendor/dependency",
            "submodule_git_url": "x".repeat(MAX_REPOSITORY_SUBMODULE_URL_BYTES + 1),
            "type": "file",
        });

        assert_eq!(
            parse_repository_path_lookup(
                &admitted,
                "vendor/dependency",
                CodeHostResultCompleteness::Complete,
            ),
            Ok(RepositoryPathLookup::Other {
                kind: RepositoryObjectKind::Submodule,
            })
        );
        assert_eq!(
            parse_repository_path_lookup(
                &rejected,
                "vendor/dependency",
                CodeHostResultCompleteness::Complete,
            ),
            Err(CodeHostTransportFailure::InvalidResponse)
        );
    }

    /// A legacy submodule marker cannot override a contradictory or absent
    /// base object discriminator.
    #[test]
    fn repository_submodule_marker_requires_a_compatible_base_type() {
        let directory = serde_json::json!({
            "path": "vendor/dependency",
            "submodule_git_url": "https://example.test/dependency.git",
            "type": "dir",
        });
        let missing_type = serde_json::json!({
            "path": "vendor/dependency",
            "submodule_git_url": "https://example.test/dependency.git",
        });
        let unknown_type = serde_json::json!({
            "path": "vendor/dependency",
            "submodule_git_url": "https://example.test/dependency.git",
            "type": "future-kind",
        });

        assert_eq!(
            parse_repository_path_lookup(
                &directory,
                "vendor/dependency",
                CodeHostResultCompleteness::Complete,
            ),
            Err(CodeHostTransportFailure::InvalidResponse)
        );
        assert_eq!(
            parse_repository_path_lookup(
                &missing_type,
                "vendor/dependency",
                CodeHostResultCompleteness::Complete,
            ),
            Err(CodeHostTransportFailure::InvalidResponse)
        );
        assert_eq!(
            parse_repository_path_lookup(
                &unknown_type,
                "vendor/dependency",
                CodeHostResultCompleteness::Complete,
            ),
            Err(CodeHostTransportFailure::InvalidResponse)
        );
    }

    /// A duplicate outside the retained prefix invalidates the complete host
    /// observation instead of becoming an honest truncation claim.
    #[test]
    fn repository_directory_rejects_duplicate_in_discarded_suffix() {
        let value =
            repository_directory_value("src", repository_directory_entries_with_suffix_duplicate());

        assert_eq!(
            parse_repository_path_lookup(&value, "src", CodeHostResultCompleteness::Complete,),
            Err(CodeHostTransportFailure::InvalidResponse)
        );
    }

    /// A symlink target is admitted under its own byte bound rather than being
    /// silently charged as a repository path.
    #[test]
    fn repository_directory_rejects_symlink_target_over_ingress_bound() {
        let value = repository_directory_value(
            "src",
            vec![serde_json::json!({
                "path": "src/link",
                "target": "x".repeat(MAX_REPOSITORY_SYMLINK_TARGET_BYTES + 1),
                "type": "symlink",
            })],
        );

        assert_eq!(
            parse_repository_path_lookup(&value, "src", CodeHostResultCompleteness::Complete,),
            Err(CodeHostTransportFailure::InvalidResponse)
        );
    }

    /// A complete standard contents-entry shape can repeat every admitted
    /// escaped path and still reach bounded projection.
    #[tokio::test]
    async fn repository_directory_ingress_admits_escaped_path_bound() {
        const OBSERVED_ENTRIES: usize = MAX_RESULT_ITEMS + 10;
        const PRIOR_RESPONSE_CAP: usize = (MAX_OBSERVED_DIRECTORY_ENTRIES + 1)
            * (MAX_FILE_PATH_BYTES * MAX_JSON_ESCAPE_BYTES_PER_SOURCE_BYTE + 256);
        let (transport, listener) = repository_test_transport().await;
        let directory = repository_directory_value(
            "src",
            repository_escaping_directory_values(OBSERVED_ENTRIES),
        )
        .to_string();
        let encoded_bytes = directory.len();
        let server = tokio::spawn(async move {
            serve_exact_revision_then(
                &listener,
                TestHttpResponse::Json {
                    status: "200 OK",
                    body: directory.as_bytes(),
                },
            )
            .await
        });
        let result = transport
            .repository_list_directory(repository_list_arguments("src"), &test_credential())
            .await
            .expect("complete escaped contents entries reach bounded projection")
            .into_json_value();
        let request = repository_server_result(server).await;

        assert!(encoded_bytes > PRIOR_RESPONSE_CAP);
        assert_eq!(result["outcome"], "entries");
        assert_eq!(result["observed_entries"], OBSERVED_ENTRIES);
        assert_eq!(result["truncated"], true);
        assert_eq!(request, path_lookup_requests("src"));
    }

    /// A directory larger than the result item bound reports both the observed
    /// and returned counts instead of discarding the truncation signal.
    #[tokio::test]
    async fn repository_directory_listing_reports_honest_truncation() {
        const OBSERVED_ENTRIES: usize = MAX_RESULT_ITEMS + 1;
        let (transport, listener) = repository_test_transport().await;
        let directory =
            repository_directory_value("src", repository_directory_entries(OBSERVED_ENTRIES))
                .to_string();
        let server = tokio::spawn(async move {
            serve_exact_revision_then(
                &listener,
                TestHttpResponse::Json {
                    status: "200 OK",
                    body: directory.as_bytes(),
                },
            )
            .await
        });
        let result = transport
            .repository_list_directory(repository_list_arguments("src"), &test_credential())
            .await
            .expect("an oversized directory returns a typed truncated prefix")
            .into_json_value();
        let request = repository_server_result(server).await;

        assert_eq!(result["outcome"], "entries");
        assert_eq!(result["observed_entries"], OBSERVED_ENTRIES);
        assert_eq!(result["returned_entries"], MAX_RESULT_ITEMS);
        assert_eq!(result["truncated"], true);
        assert_eq!(
            result["entries"]
                .as_array()
                .expect("typed entries remain an array")
                .len(),
            MAX_RESULT_ITEMS
        );
        assert_eq!(request, path_lookup_requests("src"));
    }

    async fn repository_test_transport() -> (GitHubCodeHostTransport, tokio::net::TcpListener) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener binds");
        let address = listener
            .local_addr()
            .expect("listener address is available");
        let mut transport = GitHubCodeHostTransport::try_new().expect("fixed transport constructs");
        transport.rest_base =
            Url::parse(&format!("http://{address}/")).expect("loopback REST base is valid");
        (transport, listener)
    }

    fn test_credential() -> CredentialValue {
        CredentialValue::new(b"synthetic-repository-test-token".to_vec())
    }

    fn repository_read_arguments(
        path: &str,
        line_range: Option<serde_json::Value>,
    ) -> crate::RepositoryReadFileArguments {
        serde_json::from_value(serde_json::json!({
            "line_range": line_range,
            "path": path,
            "repository": FILE_PATCH_REPOSITORY,
            "revision": REPOSITORY_REVISION,
        }))
        .expect("fixture repository-read arguments decode")
    }

    fn repository_list_arguments(path: &str) -> crate::RepositoryListDirectoryArguments {
        serde_json::from_value(serde_json::json!({
            "path": path,
            "repository": FILE_PATCH_REPOSITORY,
            "revision": REPOSITORY_REVISION,
        }))
        .expect("fixture directory-list arguments decode")
    }

    fn repository_file_value(path: &str, source_bytes: usize) -> serde_json::Value {
        serde_json::json!({
            "content": "",
            "encoding": "none",
            "path": path,
            "sha": REPOSITORY_BLOB,
            "size": source_bytes,
            "type": "file",
        })
    }

    fn repository_directory_value(
        path: &str,
        entries: Vec<serde_json::Value>,
    ) -> serde_json::Value {
        serde_json::json!({
            "entries": entries,
            "path": path,
            "type": "dir",
        })
    }

    fn repository_escaping_directory_entries(count: usize) -> Vec<RepositoryDirectoryEntry> {
        let entry_name = "\u{1}".repeat(super::super::arguments::MAX_FILE_PATH_BYTES - 7);
        (0..count)
            .map(|index| {
                RepositoryDirectoryEntry::try_new(
                    format!("src/{entry_name}{index:03}"),
                    RepositoryObjectKind::File,
                    None,
                )
                .expect("fixture directory entry is admitted")
            })
            .collect()
    }

    fn repository_escaping_directory_values(count: usize) -> Vec<serde_json::Value> {
        let entry_name = "\u{1}".repeat(super::super::arguments::MAX_FILE_PATH_BYTES - 7);
        (0..count)
            .map(|index| {
                let path = format!("src/{entry_name}{index:03}");
                serde_json::json!({
                    "_links": {
                        "git": path,
                        "html": path,
                        "self": path,
                    },
                    "download_url": path,
                    "git_url": path,
                    "html_url": path,
                    "name": path,
                    "path": path,
                    "size": index,
                    "target": path,
                    "type": "symlink",
                    "url": path,
                })
            })
            .collect()
    }

    #[test]
    fn repository_escaping_directory_values_have_complete_entry_shape() {
        let values = repository_escaping_directory_values(1);

        assert_eq!(values.len(), 1);
        let path = values[0]["path"].clone();
        assert_eq!(
            values[0],
            serde_json::json!({
                "_links": {
                    "git": path,
                    "html": path,
                    "self": path,
                },
                "download_url": path,
                "git_url": path,
                "html_url": path,
                "name": path,
                "path": path,
                "size": 0,
                "target": path,
                "type": "symlink",
                "url": path,
            })
        );
    }

    fn repository_directory_entries(count: usize) -> Vec<serde_json::Value> {
        (0..count)
            .map(|index| {
                serde_json::json!({
                    "path": format!("src/file-{index}.rs"),
                    "size": index,
                    "type": "file",
                })
            })
            .collect()
    }

    fn repository_directory_entries_with_suffix_duplicate() -> Vec<serde_json::Value> {
        let mut entries = repository_directory_entries(MAX_RESULT_ITEMS);
        entries.push(entries[0].clone());
        entries
    }

    fn repository_directory_lookup_completeness(
        value: &serde_json::Value,
    ) -> Result<CodeHostResultCompleteness, CodeHostTransportFailure> {
        match parse_repository_path_lookup(value, "src", CodeHostResultCompleteness::Complete)? {
            RepositoryPathLookup::Directory { completeness, .. } => Ok(completeness),
            RepositoryPathLookup::File { .. }
            | RepositoryPathLookup::Other { .. }
            | RepositoryPathLookup::PathNotFound
            | RepositoryPathLookup::RevisionNotFound => {
                Err(CodeHostTransportFailure::InvalidResponse)
            }
        }
    }

    fn revision_request() -> String {
        format!("GET /repos/{FILE_PATCH_REPOSITORY}/commits/{REPOSITORY_REVISION} HTTP/1.1")
    }

    fn contents_request(path: &str) -> String {
        format!(
            "GET /repos/{FILE_PATCH_REPOSITORY}/contents/{path}?ref={REPOSITORY_REVISION} HTTP/1.1"
        )
    }

    fn path_lookup_requests(path: &str) -> [String; 2] {
        [revision_request(), contents_request(path)]
    }

    fn file_read_requests(path: &str) -> [String; 3] {
        [
            revision_request(),
            contents_request(path),
            format!("GET /repos/{FILE_PATCH_REPOSITORY}/git/blobs/{REPOSITORY_BLOB} HTTP/1.1"),
        ]
    }

    enum TestHttpResponse<'a> {
        Json { status: &'a str, body: &'a [u8] },
        Raw { body: &'a [u8] },
    }

    struct TestHttpResponseParts<'a> {
        status: &'a str,
        content_type: &'a str,
        body: &'a [u8],
    }

    impl<'a> TestHttpResponse<'a> {
        fn into_parts(self) -> TestHttpResponseParts<'a> {
            match self {
                Self::Json { status, body } => TestHttpResponseParts {
                    status,
                    content_type: "application/json",
                    body,
                },
                Self::Raw { body } => TestHttpResponseParts {
                    status: "200 OK",
                    content_type: "application/octet-stream",
                    body,
                },
            }
        }
    }

    async fn serve_exact_revision(listener: &tokio::net::TcpListener) -> String {
        serve_test_response(
            listener,
            TestHttpResponse::Raw {
                body: REPOSITORY_REVISION.as_bytes(),
            },
        )
        .await
    }

    async fn serve_exact_revision_then(
        listener: &tokio::net::TcpListener,
        response: TestHttpResponse<'_>,
    ) -> [String; 2] {
        let revision_request = serve_exact_revision(listener).await;
        let path_request = serve_test_response(listener, response).await;
        [revision_request, path_request]
    }

    async fn serve_test_response(
        listener: &tokio::net::TcpListener,
        response: TestHttpResponse<'_>,
    ) -> String {
        let TestHttpResponseParts {
            status,
            content_type,
            body,
        } = response.into_parts();
        let (mut stream, _) = listener.accept().await.expect("one request connects");
        let mut request_line = String::new();
        let mut reader = BufReader::new(&mut stream);
        reader
            .read_line(&mut request_line)
            .await
            .expect("request line is readable");
        drop(reader);
        let header = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(header.as_bytes())
            .await
            .expect("response header is writable");
        stream
            .write_all(body)
            .await
            .expect("response body is writable");
        request_line.trim_end().to_owned()
    }

    async fn repository_server_result<Output>(server: tokio::task::JoinHandle<Output>) -> Output {
        tokio::time::timeout(FILE_PATCH_SERVER_TIMEOUT, server)
            .await
            .expect("loopback server observes every expected request")
            .expect("loopback server task completes")
    }

    fn changed_file_value(path: String) -> serde_json::Value {
        serde_json::json!({
            "additions": 1,
            "deletions": 0,
            "filename": path,
            "patch": "@@ -0,0 +1 @@\n+fixture",
            "status": "added",
        })
    }

    fn first_changed_file_page() -> serde_json::Value {
        serde_json::Value::Array(
            (0..100)
                .map(|index| changed_file_value(format!("generated/file-{index}.rs")))
                .collect(),
        )
    }

    /// A paginated patch assembled while the pull-request head moves is not
    /// returned as evidence from any single revision.
    #[tokio::test]
    async fn file_patch_fails_closed_when_head_moves_during_pagination() {
        let expected_transition = FilePatchRevisionTransitionInput {
            before: ChangeRequestDiffRevision {
                base: String::from(FILE_PATCH_BASE_REVISION),
                head: String::from(FILE_PATCH_HEAD_REVISION),
            },
            after: ChangeRequestDiffRevision {
                base: String::from(FILE_PATCH_BASE_REVISION),
                head: String::from(FILE_PATCH_MOVED_REVISION),
            },
        };
        let (failure, observation) =
            file_patch_revision_change_failure(expected_transition.clone()).await;

        assert_file_patch_revision_change(failure, observation, expected_transition);
    }

    /// A stable head does not make pages revision-consistent when the base
    /// revision moves during the search.
    #[tokio::test]
    async fn file_patch_fails_closed_when_base_moves_without_head() {
        let expected_transition = FilePatchRevisionTransitionInput {
            before: ChangeRequestDiffRevision {
                base: String::from(FILE_PATCH_BASE_REVISION),
                head: String::from(FILE_PATCH_HEAD_REVISION),
            },
            after: ChangeRequestDiffRevision {
                base: String::from(FILE_PATCH_MOVED_REVISION),
                head: String::from(FILE_PATCH_HEAD_REVISION),
            },
        };
        let (failure, observation) =
            file_patch_revision_change_failure(expected_transition.clone()).await;

        assert_file_patch_revision_change(failure, observation, expected_transition);
    }

    async fn file_patch_revision_change_failure(
        revision_transition: FilePatchRevisionTransitionInput,
    ) -> (
        CodeHostTransportFailure,
        FilePatchRevisionChangeServerObservation,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener binds");
        let address = listener
            .local_addr()
            .expect("listener address is available");
        let server = tokio::spawn(serve_changed_file_patch_revision(
            listener,
            revision_transition,
        ));
        let mut transport = GitHubCodeHostTransport::try_new().expect("fixed transport constructs");
        transport.rest_base =
            Url::parse(&format!("http://{address}/")).expect("loopback REST base is valid");
        let arguments = moved_head_file_patch_arguments();
        let credential = CredentialValue::new(b"synthetic-test-token".to_vec());

        let failure = transport
            .file_patch_transaction(arguments, &credential)
            .await
            .expect_err("a moved diff revision cannot yield consistent patch evidence");
        let requests = tokio::time::timeout(FILE_PATCH_SERVER_TIMEOUT, server)
            .await
            .expect("loopback server observes every expected request")
            .expect("loopback server task completes");

        (failure, requests)
    }

    #[track_caller]
    fn assert_file_patch_revision_change(
        failure: CodeHostTransportFailure,
        observation: FilePatchRevisionChangeServerObservation,
        expected_transition: FilePatchRevisionTransitionInput,
    ) {
        assert_eq!(
            failure,
            CodeHostTransportFailure::ChangeRequestRevisionChanged
        );
        assert_eq!(
            crate::code_host::transport_failure_class(
                crate::code_host::CodeHostToolKind::FilePatch,
                failure,
            ),
            Some(crate::code_host::OperatorFailureClass::Infrastructure {
                commit_ambiguous: false
            })
        );
        assert_eq!(observation.transition, expected_transition);
        assert_eq!(observation.requests, moved_head_file_patch_requests());
    }

    fn moved_head_file_patch_arguments() -> crate::FilePatchArguments {
        serde_json::from_value(serde_json::json!({
            "repository": FILE_PATCH_REPOSITORY,
            "number": FILE_PATCH_NUMBER,
            "path": FILE_PATCH_TARGET_PATH,
        }))
        .expect("fixture file-patch arguments decode")
    }

    fn moved_head_file_patch_requests() -> [String; 4] {
        let prefix = format!("/repos/{FILE_PATCH_REPOSITORY}/pulls/{FILE_PATCH_NUMBER}");
        [
            format!("GET {prefix} HTTP/1.1"),
            format!("GET {prefix}/files?per_page=100&page=1 HTTP/1.1"),
            format!("GET {prefix}/files?per_page=100&page=2 HTTP/1.1"),
            format!("GET {prefix} HTTP/1.1"),
        ]
    }

    async fn serve_changed_file_patch_revision(
        listener: tokio::net::TcpListener,
        revision_transition: FilePatchRevisionTransitionInput,
    ) -> FilePatchRevisionChangeServerObservation {
        let FilePatchRevisionTransitionInput { before, after } = revision_transition;
        let initial_revision = change_request_revision_value(&before);
        let target_page = serde_json::Value::Array(vec![changed_file_value(String::from(
            FILE_PATCH_TARGET_PATH,
        ))]);
        let initial_request =
            serve_json_response(&listener, &initial_revision.to_string(), None).await;
        let first_page_request = serve_json_response(
            &listener,
            &first_changed_file_page().to_string(),
            Some(r#"<http://fixture.invalid/page/2>; rel="next""#),
        )
        .await;
        let target_page_request =
            serve_json_response(&listener, &target_page.to_string(), None).await;
        let final_revision = change_request_revision_value(&after);
        let final_request = serve_json_response(&listener, &final_revision.to_string(), None).await;
        FilePatchRevisionChangeServerObservation {
            transition: FilePatchRevisionTransitionInput { before, after },
            requests: [
                initial_request,
                first_page_request,
                target_page_request,
                final_request,
            ],
        }
    }

    fn change_request_revision_value(revision: &ChangeRequestDiffRevision) -> serde_json::Value {
        serde_json::json!({
            "number": FILE_PATCH_NUMBER,
            "base": {"sha": revision.base},
            "head": {"sha": revision.head},
        })
    }

    async fn serve_json_response(
        listener: &tokio::net::TcpListener,
        body: &str,
        link: Option<&str>,
    ) -> String {
        let (mut stream, _) = listener.accept().await.expect("one request connects");
        let mut request_line = String::new();
        let mut reader = BufReader::new(&mut stream);
        reader
            .read_line(&mut request_line)
            .await
            .expect("request line is readable");
        drop(reader);
        let link_header = link
            .map(|value| format!("Link: {value}\r\n"))
            .unwrap_or_default();
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{link_header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .expect("JSON response is writable");
        request_line.trim_end().to_owned()
    }

    /// A requested patch beyond GitHub's first hundred changed files remains
    /// reachable through the same bounded tool operation.
    #[test]
    fn file_patch_search_reaches_the_second_changed_file_page() {
        let target_path = "src/target.rs";
        let target_value = changed_file_value(String::from(target_path));
        let expected_file =
            ChangedFile::try_new(String::from(target_path), String::from("added"), 1, 0)
                .expect("fixture changed file is bounded");
        let expected =
            FilePatchResult::try_new(expected_file, Some(String::from("@@ -0,0 +1 @@\n+fixture")))
                .expect("fixture patch is bounded");

        assert_eq!(
            inspect_file_patch_page(
                &first_changed_file_page(),
                CodeHostResultCompleteness::Truncated,
                target_path,
            ),
            Ok(FilePatchPageOutcome::Continue)
        );
        assert_eq!(
            inspect_file_patch_page(
                &serde_json::Value::Array(vec![target_value]),
                CodeHostResultCompleteness::Complete,
                target_path,
            ),
            Ok(FilePatchPageOutcome::Found(expected))
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

    /// Paths returned by GitHub retain canonical repository-relative
    /// components across both result shapes that expose them.
    #[test]
    fn returned_paths_reject_noncanonical_values() {
        let absolute_path = String::from("/src/lib.rs");
        let noncanonical_path = String::from("src/../lib.rs");
        let absolute_changed_file =
            ChangedFile::try_new(absolute_path.clone(), String::from("modified"), 1, 0);
        let absolute_review_thread = ReviewThread::try_new(ReviewThreadFields {
            id: String::from("PRRT_absolute_fixture"),
            resolved: false,
            outdated: false,
            path: absolute_path,
            line: Some(1),
            comments: Vec::new(),
            comments_truncated: false,
        });
        let noncanonical_changed_file =
            ChangedFile::try_new(noncanonical_path.clone(), String::from("modified"), 1, 0);
        let noncanonical_review_thread = ReviewThread::try_new(ReviewThreadFields {
            id: String::from("PRRT_noncanonical_fixture"),
            resolved: false,
            outdated: false,
            path: noncanonical_path,
            line: Some(1),
            comments: Vec::new(),
            comments_truncated: false,
        });

        assert_eq!(absolute_changed_file, None);
        assert_eq!(absolute_review_thread, None);
        assert_eq!(noncanonical_changed_file, None);
        assert_eq!(noncanonical_review_thread, None);
    }

    /// File-shaped results cannot identify the repository root directory.
    #[test]
    fn returned_file_paths_reject_repository_root() {
        let root_changed_file =
            ChangedFile::try_new(String::from("."), String::from("modified"), 1, 0);
        let root_review_thread = ReviewThread::try_new(ReviewThreadFields {
            id: String::from("PRRT_root_fixture"),
            resolved: false,
            outdated: false,
            path: String::from("."),
            line: Some(1),
            comments: Vec::new(),
            comments_truncated: false,
        });

        assert_eq!(root_changed_file, None);
        assert_eq!(root_review_thread, None);
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

    /// Every page in one changed-file search shares one elapsed-time budget.
    #[tokio::test]
    async fn file_patch_transaction_has_one_timeout_budget() {
        let result = with_read_operation_timeout(
            Duration::ZERO,
            std::future::pending::<Result<(), CodeHostTransportFailure>>(),
        )
        .await;

        assert_eq!(result, Err(CodeHostTransportFailure::DispatchUnknown));
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

    /// A thread arriving during final stack revalidation invalidates the gate snapshot.
    #[test]
    fn review_gate_rejects_convergence_changed_during_final_stack_read() {
        let stack = gate_stack_state();
        let initial_convergence = gate_convergence_state(None);
        let thread = ReviewThreadIdentity::try_new(
            String::from("PRRT_gate"),
            String::from("src/lib.rs"),
            String::from("Finding title"),
        )
        .expect("fixture thread identity is admitted");
        let current_convergence = gate_convergence_state(Some(thread));

        assert_eq!(
            ensure_review_gate_snapshot_unchanged(
                &stack,
                &stack,
                &initial_convergence,
                &current_convergence,
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

    const OWNED_THREAD_ID: &str = "PRRT_thread";
    const THREAD_REPLY_BODY: &str = "fixed";
    const REPLY_COMMENT_ID: &str = "PRRC_reply";
    const REPLY_COMMENT_URL: &str = "https://github.example/comment/7002";
    // Arbitrary foreign coordinates; each only needs to differ from the
    // owning `FILE_PATCH_REPOSITORY` / `FILE_PATCH_NUMBER` pair.
    const FOREIGN_CHANGE_REQUEST_NUMBER: u32 = 18;
    const FOREIGN_REPOSITORY: &str = "another/repository";
    /// How long a loopback server watches for a mutation request that a
    /// refused ownership check must never dispatch.
    const NO_FOLLOWUP_REQUEST_WINDOW: Duration = Duration::from_millis(200);

    fn change_request_number() -> CodeHostChangeRequestNumber {
        CodeHostChangeRequestNumber::try_new(u64::from(FILE_PATCH_NUMBER))
            .expect("fixture change-request number is admitted")
    }

    fn thread_reply_test_arguments() -> crate::ThreadReplyArguments {
        serde_json::from_value(serde_json::json!({
            "body": THREAD_REPLY_BODY,
            "number": FILE_PATCH_NUMBER,
            "repository": FILE_PATCH_REPOSITORY,
            "thread_id": OWNED_THREAD_ID,
        }))
        .expect("fixture thread-reply arguments decode")
    }

    fn thread_resolve_test_arguments() -> crate::ThreadResolveArguments {
        serde_json::from_value(serde_json::json!({
            "number": FILE_PATCH_NUMBER,
            "repository": FILE_PATCH_REPOSITORY,
            "thread_id": OWNED_THREAD_ID,
        }))
        .expect("fixture thread-resolve arguments decode")
    }

    fn thread_ownership_response(number: u32, name_with_owner: &str) -> Vec<u8> {
        serde_json::json!({
            "data": {"node": {
                "__typename": "PullRequestReviewThread",
                "pullRequest": {
                    "number": number,
                    "repository": {"nameWithOwner": name_with_owner},
                },
            }},
        })
        .to_string()
        .into_bytes()
    }

    fn owned_thread_ownership_response() -> Vec<u8> {
        thread_ownership_response(FILE_PATCH_NUMBER, FILE_PATCH_REPOSITORY)
    }

    fn foreign_number_thread_ownership_response() -> Vec<u8> {
        thread_ownership_response(FOREIGN_CHANGE_REQUEST_NUMBER, FILE_PATCH_REPOSITORY)
    }

    fn foreign_repository_thread_ownership_response() -> Vec<u8> {
        thread_ownership_response(FILE_PATCH_NUMBER, FOREIGN_REPOSITORY)
    }

    fn thread_reply_acknowledgement() -> Vec<u8> {
        serde_json::json!({
            "data": {"addPullRequestReviewThreadReply": {"comment": {
                "id": REPLY_COMMENT_ID,
                "url": REPLY_COMMENT_URL,
            }}},
        })
        .to_string()
        .into_bytes()
    }

    fn thread_resolve_acknowledgement() -> Vec<u8> {
        serde_json::json!({
            "data": {"resolveReviewThread": {"thread": {
                "id": OWNED_THREAD_ID,
                "isResolved": true,
            }}},
        })
        .to_string()
        .into_bytes()
    }

    async fn graphql_test_transport() -> (GitHubCodeHostTransport, tokio::net::TcpListener) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener binds");
        let address = listener
            .local_addr()
            .expect("listener address is available");
        let mut transport = GitHubCodeHostTransport::try_new().expect("fixed transport constructs");
        transport.graphql_url = Url::parse(&format!("http://{address}/graphql"))
            .expect("loopback GraphQL URL is valid");
        (transport, listener)
    }

    /// Serves one bounded JSON success response and returns the observed
    /// request body, so a test can assert which GraphQL document arrived.
    async fn serve_graphql_response(
        listener: &tokio::net::TcpListener,
        response_body: &[u8],
    ) -> String {
        use tokio::io::AsyncReadExt;

        let (mut stream, _) = listener.accept().await.expect("one request connects");
        let mut reader = BufReader::new(&mut stream);
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .expect("request header line is readable");
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value
                    .trim()
                    .parse()
                    .expect("request content length is numeric");
            }
        }
        let mut request_body = vec![0u8; content_length];
        reader
            .read_exact(&mut request_body)
            .await
            .expect("request body is readable");
        drop(reader);
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response_body.len()
        );
        stream
            .write_all(header.as_bytes())
            .await
            .expect("response header is writable");
        stream
            .write_all(response_body)
            .await
            .expect("response body is writable");
        String::from_utf8(request_body).expect("request body is UTF-8")
    }

    /// A thread reply is dispatched only after the code host places the named
    /// thread inside the change request the arguments name, so the ownership
    /// query precedes the reply mutation on the wire.
    #[tokio::test]
    async fn thread_reply_confirms_ownership_before_dispatching_the_mutation() {
        let (transport, listener) = graphql_test_transport().await;
        let ownership_response = owned_thread_ownership_response();
        let mutation_response = thread_reply_acknowledgement();
        let server = tokio::spawn(async move {
            let ownership_request = serve_graphql_response(&listener, &ownership_response).await;
            let mutation_request = serve_graphql_response(&listener, &mutation_response).await;
            [ownership_request, mutation_request]
        });
        let result = transport
            .thread_reply(thread_reply_test_arguments(), &test_credential())
            .await
            .expect("an owned thread admits the reply mutation");
        let [ownership_request, mutation_request] = repository_server_result(server).await;

        assert_eq!(
            result.into_json_value(),
            serde_json::json!({"id": REPLY_COMMENT_ID, "url": REPLY_COMMENT_URL})
        );
        assert!(ownership_request.contains("ThreadOwnership"));
        assert!(ownership_request.contains(OWNED_THREAD_ID));
        assert!(mutation_request.contains("addPullRequestReviewThreadReply"));
    }

    /// A reply naming a thread the code host places in another change request
    /// is refused with the typed ownership failure, and the refusal window
    /// observes no mutation request.
    #[tokio::test]
    async fn thread_reply_to_a_foreign_thread_dispatches_no_mutation() {
        let (transport, listener) = graphql_test_transport().await;
        let ownership_response = foreign_number_thread_ownership_response();
        let server = tokio::spawn(async move {
            let ownership_request = serve_graphql_response(&listener, &ownership_response).await;
            let followup =
                tokio::time::timeout(NO_FOLLOWUP_REQUEST_WINDOW, listener.accept()).await;
            (ownership_request, followup.is_err())
        });
        let failure = transport
            .thread_reply(thread_reply_test_arguments(), &test_credential())
            .await
            .expect_err("a foreign thread cannot admit a reply mutation");
        let (ownership_request, no_followup_request) = repository_server_result(server).await;

        assert_eq!(failure, CodeHostTransportFailure::ThreadNotInChangeRequest);
        assert!(ownership_request.contains("ThreadOwnership"));
        assert!(
            no_followup_request,
            "the refused reply must not dispatch a mutation request"
        );
    }

    /// A thread resolution is dispatched only after the same ownership
    /// confirmation as a reply.
    #[tokio::test]
    async fn thread_resolve_confirms_ownership_before_dispatching_the_mutation() {
        let (transport, listener) = graphql_test_transport().await;
        let ownership_response = owned_thread_ownership_response();
        let mutation_response = thread_resolve_acknowledgement();
        let server = tokio::spawn(async move {
            let ownership_request = serve_graphql_response(&listener, &ownership_response).await;
            let mutation_request = serve_graphql_response(&listener, &mutation_response).await;
            [ownership_request, mutation_request]
        });
        let result = transport
            .thread_resolve(thread_resolve_test_arguments(), &test_credential())
            .await
            .expect("an owned thread admits the resolve mutation");
        let [ownership_request, mutation_request] = repository_server_result(server).await;

        assert_eq!(
            result.into_json_value(),
            serde_json::json!({"resolved": true, "thread_id": OWNED_THREAD_ID})
        );
        assert!(ownership_request.contains("ThreadOwnership"));
        assert!(ownership_request.contains(OWNED_THREAD_ID));
        assert!(mutation_request.contains("resolveReviewThread"));
    }

    /// A resolution naming a thread the code host places in another
    /// repository is refused with the typed ownership failure, and the
    /// refusal window observes no mutation request.
    #[tokio::test]
    async fn thread_resolve_in_a_foreign_repository_dispatches_no_mutation() {
        let (transport, listener) = graphql_test_transport().await;
        let ownership_response = foreign_repository_thread_ownership_response();
        let server = tokio::spawn(async move {
            let ownership_request = serve_graphql_response(&listener, &ownership_response).await;
            let followup =
                tokio::time::timeout(NO_FOLLOWUP_REQUEST_WINDOW, listener.accept()).await;
            (ownership_request, followup.is_err())
        });
        let failure = transport
            .thread_resolve(thread_resolve_test_arguments(), &test_credential())
            .await
            .expect_err("a foreign thread cannot admit a resolve mutation");
        let (ownership_request, no_followup_request) = repository_server_result(server).await;

        assert_eq!(failure, CodeHostTransportFailure::ThreadNotInChangeRequest);
        assert!(ownership_request.contains("ThreadOwnership"));
        assert!(
            no_followup_request,
            "the refused resolution must not dispatch a mutation request"
        );
    }

    /// Complete ownership evidence for the named change request decodes into
    /// the exact owning coordinates.
    #[test]
    fn thread_ownership_evidence_decodes_the_owning_change_request() {
        let value: serde_json::Value = serde_json::from_slice(&owned_thread_ownership_response())
            .expect("fixture ownership response is JSON");

        assert_eq!(
            parse_thread_ownership(&value),
            Ok(ThreadOwnershipEvidence::Thread {
                number: u64::from(FILE_PATCH_NUMBER),
                repository: String::from(FILE_PATCH_REPOSITORY),
            })
        );
    }

    /// Definitive node absence arrives as an evaluated `data.node: null`
    /// beside the code host's typed not-found error, and reads as ownership
    /// refusal rather than as transport loss.
    #[test]
    fn a_missing_thread_node_is_definitive_ownership_refusal() {
        let value = serde_json::json!({
            "data": {"node": null},
            "errors": [{"type": "NOT_FOUND", "message": "Could not resolve to a node"}],
        });

        assert_eq!(
            parse_thread_ownership(&value),
            Ok(ThreadOwnershipEvidence::NotAThread)
        );
    }

    /// An evaluated null node with no error entry at all is the query's own
    /// answer that nothing resolved.
    #[test]
    fn an_evaluated_null_node_without_errors_is_definitive_absence() {
        let value = serde_json::json!({"data": {"node": null}});

        assert_eq!(
            parse_thread_ownership(&value),
            Ok(ThreadOwnershipEvidence::NotAThread)
        );
    }

    /// A field error without the not-found classification nulls the node for
    /// a transient resolver failure as readily as for absence, so it proves
    /// nothing about the thread and reports only the undispatched mutation.
    #[test]
    fn a_field_error_beside_a_null_node_is_not_absence_evidence() {
        let value = serde_json::json!({
            "data": {"node": null},
            "errors": [{"type": "INTERNAL", "message": "Something went wrong"}],
        });

        assert_eq!(
            parse_thread_ownership(&value),
            Err(CodeHostTransportFailure::MutationNotDispatched)
        );
    }

    /// An error entry that carries no classification field cannot prove
    /// absence, so the null node beside it stays an undispatched mutation.
    #[test]
    fn an_unclassified_error_beside_a_null_node_is_not_absence_evidence() {
        let value = serde_json::json!({
            "data": {"node": null},
            "errors": [{"message": "Could not resolve to a node"}],
        });

        assert_eq!(
            parse_thread_ownership(&value),
            Err(CodeHostTransportFailure::MutationNotDispatched)
        );
    }

    /// A node of another type cannot belong to any change request as a
    /// review thread.
    #[test]
    fn another_node_type_is_not_thread_ownership_evidence() {
        let value = serde_json::json!({"data": {"node": {"__typename": "Issue"}}});

        assert_eq!(
            parse_thread_ownership(&value),
            Ok(ThreadOwnershipEvidence::NotAThread)
        );
    }

    /// A response without an evaluated `data` member never ran the query, so
    /// it proves only that the mutation was not dispatched.
    #[test]
    fn an_unevaluated_ownership_response_proves_only_no_dispatch() {
        let value = serde_json::json!({"errors": [{"message": "rate limited"}]});

        assert_eq!(
            parse_thread_ownership(&value),
            Err(CodeHostTransportFailure::MutationNotDispatched)
        );
    }

    /// A node claiming the thread type without its owning change request is a
    /// malformed bounded response, not ownership evidence.
    #[test]
    fn a_malformed_thread_node_is_an_invalid_bounded_response() {
        let value =
            serde_json::json!({"data": {"node": {"__typename": "PullRequestReviewThread"}}});

        assert_eq!(
            parse_thread_ownership(&value),
            Err(CodeHostTransportFailure::InvalidResponse)
        );
    }

    /// Matching coordinates place the thread inside the named change request.
    #[test]
    fn thread_ownership_predicate_admits_the_owning_change_request() {
        let evidence = ThreadOwnershipEvidence::Thread {
            number: u64::from(FILE_PATCH_NUMBER),
            repository: String::from(FILE_PATCH_REPOSITORY),
        };

        assert!(thread_in_change_request(
            &evidence,
            &repository(),
            change_request_number(),
        ));
    }

    /// GitHub addresses repositories case-insensitively, so a case-variant
    /// spelling of the same repository is not a foreign target.
    #[test]
    fn thread_ownership_predicate_admits_case_variant_repository_spelling() {
        let evidence = ThreadOwnershipEvidence::Thread {
            number: u64::from(FILE_PATCH_NUMBER),
            repository: FILE_PATCH_REPOSITORY.to_ascii_uppercase(),
        };

        assert!(thread_in_change_request(
            &evidence,
            &repository(),
            change_request_number(),
        ));
    }

    /// A thread owned by another change-request number is a foreign target.
    #[test]
    fn thread_ownership_predicate_rejects_a_foreign_number() {
        let evidence = ThreadOwnershipEvidence::Thread {
            number: u64::from(FOREIGN_CHANGE_REQUEST_NUMBER),
            repository: String::from(FILE_PATCH_REPOSITORY),
        };

        assert!(!thread_in_change_request(
            &evidence,
            &repository(),
            change_request_number(),
        ));
    }

    /// A thread owned by another repository is a foreign target even at the
    /// same change-request number.
    #[test]
    fn thread_ownership_predicate_rejects_a_foreign_repository() {
        let evidence = ThreadOwnershipEvidence::Thread {
            number: u64::from(FILE_PATCH_NUMBER),
            repository: String::from(FOREIGN_REPOSITORY),
        };

        assert!(!thread_in_change_request(
            &evidence,
            &repository(),
            change_request_number(),
        ));
    }

    /// Evidence that the identity names no review thread never places it in
    /// any change request.
    #[test]
    fn thread_ownership_predicate_rejects_a_non_thread_node() {
        assert!(!thread_in_change_request(
            &ThreadOwnershipEvidence::NotAThread,
            &repository(),
            change_request_number(),
        ));
    }

    /// The mutation phase receives only the exchange budget the ownership
    /// confirmation left unconsumed, so both requests together respect the
    /// transport's single 30-second exchange timeout.
    #[test]
    fn thread_mutation_uses_the_remaining_exchange_budget() {
        const ELAPSED: Duration = Duration::from_secs(7);
        const EXPECTED_REMAINING: Duration = Duration::from_secs(23);

        assert_eq!(remaining_mutation_budget(ELAPSED), Ok(EXPECTED_REMAINING));
    }

    /// A confirmation that exhausts the whole exchange budget proves the
    /// mutation was never dispatched rather than claiming ambiguity.
    #[test]
    fn an_exhausted_exchange_budget_proves_no_dispatch() {
        assert_eq!(
            remaining_mutation_budget(DEFAULT_TIMEOUT),
            Err(CodeHostTransportFailure::MutationNotDispatched)
        );
    }

    /// Ownership-check failures keep read classification: definitive answers
    /// keep their meaning, refused bounded responses end the attempt as the
    /// code host's answer, and transport loss proves only that the mutation
    /// was never dispatched.
    #[test]
    fn ownership_evidence_failures_keep_read_classification() {
        assert_eq!(
            ownership_evidence_failure(CodeHostTransportFailure::InvalidCredential),
            CodeHostTransportFailure::InvalidCredential
        );
        assert_eq!(
            ownership_evidence_failure(CodeHostTransportFailure::Rejected),
            CodeHostTransportFailure::Rejected
        );
        assert_eq!(
            ownership_evidence_failure(CodeHostTransportFailure::ThreadNotInChangeRequest),
            CodeHostTransportFailure::ThreadNotInChangeRequest
        );
        assert_eq!(
            ownership_evidence_failure(CodeHostTransportFailure::InvalidResponse),
            CodeHostTransportFailure::Rejected
        );
        assert_eq!(
            ownership_evidence_failure(CodeHostTransportFailure::ResponseTooLarge),
            CodeHostTransportFailure::Rejected
        );
        assert_eq!(
            ownership_evidence_failure(CodeHostTransportFailure::DispatchUnknown),
            CodeHostTransportFailure::MutationNotDispatched
        );
        assert_eq!(
            ownership_evidence_failure(CodeHostTransportFailure::MutationNotDispatched),
            CodeHostTransportFailure::MutationNotDispatched
        );
    }
}
