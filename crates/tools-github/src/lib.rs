//! Credentialed GitHub pull-request tools.
//!
//! REST supplies pull-request metadata, changed-file patches, and review
//! publication. GraphQL supplies review threads because only that API exposes
//! thread resolution state.

use std::{borrow::Cow, error::Error, fmt, future::Future, time::Duration};

use futures_util::StreamExt;
use reqwest::{
    Method, Response, StatusCode, Url,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue, LINK, USER_AGENT},
};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, de::DeserializeOwned};
use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolArgumentValidator, ToolDefinition, ToolExecutionInvocation,
    ToolExecutor, ToolExecutorEvidence,
};
use signalbox_domain::{
    NormalizedToolArguments, ToolEffectClass, ToolExecutionErrorDetail, ToolPermissionDefault,
};
use signalbox_model_runtime::{CredentialAccess, CredentialReference, CredentialValue};
use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};
use signalbox_tools_basic::{
    PublicDestinationClientError, WebFetchTransportFailure, has_more_response_bytes,
    public_destination_client,
};

/// Pull-request metadata tool name.
pub const PULL_REQUEST_METADATA_NAME: &str = "github_pull_request_metadata";
/// Exact-revision changed-files tool name.
pub const PULL_REQUEST_DIFF_NAME: &str = "github_pull_request_diff";
/// Review-thread tool name.
pub const PULL_REQUEST_REVIEW_THREADS_NAME: &str = "github_pull_request_review_threads";
/// Review-publication tool name.
pub const PULL_REQUEST_PUBLISH_REVIEW_NAME: &str = "github_pull_request_publish_review";
/// Non-secret deployment credential reference.
pub const GITHUB_CREDENTIAL_REFERENCE: &str = "github-primary";
/// Fixed catalog order.
pub const GITHUB_TOOL_NAMES: [&str; 4] = [
    PULL_REQUEST_DIFF_NAME,
    PULL_REQUEST_METADATA_NAME,
    PULL_REQUEST_PUBLISH_REVIEW_NAME,
    PULL_REQUEST_REVIEW_THREADS_NAME,
];

const REST_BASE_URL: &str = "https://api.github.com/";
const GRAPHQL_URL: &str = "https://api.github.com/graphql";
const GITHUB_API_ORIGIN: &str = "https://api.github.com";
const API_VERSION: &str = "2026-03-10";
const USER_AGENT_VALUE: &str = "signalbox";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const PAGE_SIZE: &str = "100";
const MAX_FILE_PAGES: u16 = 30;
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_ERROR_SOURCE_BYTES: usize = 64 * 1024;
const MAX_ERROR_DETAIL_BYTES: usize = 2 * 1024;
const MAX_RESULT_BYTES: usize = 512 * 1024;
const MAX_REPOSITORY_BYTES: usize = 256;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_PATH_BYTES: usize = 4 * 1024;
const MAX_URL_BYTES: usize = 8 * 1024;
const MAX_INLINE_COMMENTS: usize = 50;
const ERROR_TRUNCATION_SUFFIX: &str = " … [truncated]";
const INVALID_ARGUMENTS_DETAIL: &str = "GitHub pull-request tool arguments are invalid";
const CREDENTIAL_UNAVAILABLE_DETAIL: &str = "GitHub credential is unavailable";
const REQUEST_REJECTED_DETAIL: &str = "GitHub rejected the pull-request operation";
const REVISION_CHANGED_DETAIL: &str = "pull-request base or head changed during the diff read";

const REVIEW_THREADS_QUERY: &str = r#"
query PullRequestReviewThreads($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100) {
        nodes {
          id isResolved isOutdated path line
          comments(first: 100) {
            nodes { id author { login } body createdAt url }
            pageInfo { hasNextPage }
          }
        }
        pageInfo { hasNextPage }
      }
    }
  }
}
"#;

/// Egress policy admitting exactly the public GitHub API origin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GitHubEgressPolicy {
    private: (),
}

impl GitHubEgressPolicy {
    /// Constructs the fixed GitHub-API-only policy.
    pub const fn github_api_only() -> Self {
        Self { private: () }
    }

    /// Returns the one admitted origin.
    pub const fn admitted_origin(&self) -> &'static str {
        GITHUB_API_ORIGIN
    }

    fn admits(&self, url: &Url) -> bool {
        url.scheme() == "https"
            && url.host_str() == Some("api.github.com")
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
    }
}

/// Checked `owner/repository` selector.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(try_from = "String")]
pub struct GitHubRepository {
    value: String,
    owner_end: usize,
}

impl GitHubRepository {
    /// Borrows the checked selector.
    pub fn as_str(&self) -> &str {
        &self.value
    }

    fn owner(&self) -> &str {
        &self.value[..self.owner_end]
    }

    fn name(&self) -> &str {
        &self.value[self.owner_end + 1..]
    }
}

impl TryFrom<String> for GitHubRepository {
    type Error = InvalidGitHubArguments;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty()
            || value.len() > MAX_REPOSITORY_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(InvalidGitHubArguments);
        }
        let mut separators = value.match_indices('/');
        let Some((owner_end, _)) = separators.next() else {
            return Err(InvalidGitHubArguments);
        };
        if separators.next().is_some() {
            return Err(InvalidGitHubArguments);
        }
        if !valid_repository_segment(&value[..owner_end])
            || !valid_repository_segment(&value[owner_end + 1..])
        {
            return Err(InvalidGitHubArguments);
        }
        Ok(Self { value, owner_end })
    }
}

impl JsonSchema for GitHubRepository {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("GitHubRepository")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "maxLength": MAX_REPOSITORY_BYTES,
            "pattern": r"^(?:[A-Za-z0-9_-]|\.[A-Za-z0-9_-]|\.\.[A-Za-z0-9._-])[A-Za-z0-9._-]*/(?:[A-Za-z0-9_-]|\.[A-Za-z0-9_-]|\.\.[A-Za-z0-9._-])[A-Za-z0-9._-]*$",
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

fn valid_repository_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Positive pull-request number supported by GitHub GraphQL.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(try_from = "u64")]
pub struct PullRequestNumber(u32);

impl PullRequestNumber {
    /// Returns the checked number.
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u64> for PullRequestNumber {
    type Error = InvalidGitHubArguments;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        let value = u32::try_from(value).map_err(|_| InvalidGitHubArguments)?;
        (value > 0 && value <= i32::MAX as u32)
            .then_some(Self(value))
            .ok_or(InvalidGitHubArguments)
    }
}

impl JsonSchema for PullRequestNumber {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("PullRequestNumber")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "integer", "minimum": 1, "maximum": i32::MAX})
    }

    fn inline_schema() -> bool {
        true
    }
}

/// Exact lowercase 40-hex Git revision.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(try_from = "String")]
pub struct GitHubRevision(String);

impl GitHubRevision {
    /// Borrows the exact revision.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for GitHubRevision {
    type Error = InvalidGitHubArguments;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        valid_revision(&value)
            .then_some(Self(value))
            .ok_or(InvalidGitHubArguments)
    }
}

impl JsonSchema for GitHubRevision {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("GitHubRevision")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string", "minLength": 40, "maxLength": 40,
            "pattern": "^[0-9a-f]{40}$",
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

fn valid_revision(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Shared pull-request read arguments.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PullRequestArguments {
    /// Exact owner/repository spelling.
    repository: GitHubRepository,
    /// Pull-request number.
    number: PullRequestNumber,
}

impl PullRequestArguments {
    /// Borrows the repository selector.
    pub fn repository(&self) -> &GitHubRepository {
        &self.repository
    }

    /// Returns the pull-request number.
    pub const fn number(&self) -> PullRequestNumber {
        self.number
    }
}

/// Review action accepted by GitHub.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublishReviewEvent {
    /// Publish comments without a verdict.
    Comment,
    /// Approve the pull request.
    Approve,
    /// Request changes.
    RequestChanges,
}

impl PublishReviewEvent {
    const fn api_value(self) -> &'static str {
        match self {
            Self::Comment => "COMMENT",
            Self::Approve => "APPROVE",
            Self::RequestChanges => "REQUEST_CHANGES",
        }
    }
}

/// Side of a diff line.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DiffSide {
    /// Base/left side.
    Left,
    /// Head/right side.
    Right,
}

impl DiffSide {
    const fn api_value(self) -> &'static str {
        match self {
            Self::Left => "LEFT",
            Self::Right => "RIGHT",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(try_from = "String")]
struct BoundedText(String);

impl BoundedText {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for BoundedText {
    type Error = InvalidGitHubArguments;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        (!value.is_empty() && valid_text(&value, false))
            .then_some(Self(value))
            .ok_or(InvalidGitHubArguments)
    }
}

impl JsonSchema for BoundedText {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("BoundedText")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string", "minLength": 1, "maxLength": MAX_TEXT_BYTES,
            "pattern": r"^[^\u0000]+$",
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(try_from = "String")]
struct FilePath(String);

impl FilePath {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for FilePath {
    type Error = InvalidGitHubArguments;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        checked_path(value)
            .map(Self)
            .map_err(|_| InvalidGitHubArguments)
    }
}

impl JsonSchema for FilePath {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("FilePath")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string", "minLength": 1, "maxLength": MAX_PATH_BYTES,
            "pattern": r"^[^/\u0000][^\u0000]*$",
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

/// Optional inline comment in a review.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InlineReviewComment {
    /// Repository-relative path.
    path: FilePath,
    /// Positive diff line.
    line: u32,
    /// Diff side.
    side: DiffSide,
    /// Comment text.
    body: BoundedText,
}

impl InlineReviewComment {
    /// Borrows the path.
    pub fn path(&self) -> &str {
        self.path.as_str()
    }

    /// Returns the line.
    pub const fn line(&self) -> u32 {
        self.line
    }

    /// Returns the side.
    pub const fn side(&self) -> DiffSide {
        self.side
    }

    /// Borrows the body.
    pub fn body(&self) -> &str {
        self.body.as_str()
    }
}

/// Arguments for publishing a review against an exact head.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PublishReviewArguments {
    /// Exact owner/repository spelling.
    repository: GitHubRepository,
    /// Pull-request number.
    number: PullRequestNumber,
    /// Exact reviewed head commit.
    commit_id: GitHubRevision,
    /// Review action.
    event: PublishReviewEvent,
    /// Optional overall body.
    body: Option<BoundedText>,
    /// Optional inline comments.
    #[serde(default)]
    comments: Vec<InlineReviewComment>,
}

impl PublishReviewArguments {
    /// Borrows the repository selector.
    pub fn repository(&self) -> &GitHubRepository {
        &self.repository
    }

    /// Returns the pull-request number.
    pub const fn number(&self) -> PullRequestNumber {
        self.number
    }

    /// Borrows the exact head commit.
    pub fn commit_id(&self) -> &GitHubRevision {
        &self.commit_id
    }

    /// Returns the review action.
    pub const fn event(&self) -> PublishReviewEvent {
        self.event
    }

    /// Borrows the overall body.
    pub fn body(&self) -> Option<&str> {
        self.body.as_ref().map(BoundedText::as_str)
    }

    /// Borrows inline comments.
    pub fn comments(&self) -> &[InlineReviewComment] {
        &self.comments
    }

    fn valid_combination(&self) -> bool {
        self.comments.len() <= MAX_INLINE_COMMENTS
            && match self.event {
                PublishReviewEvent::Approve => true,
                PublishReviewEvent::Comment => self.body.is_some() || !self.comments.is_empty(),
                PublishReviewEvent::RequestChanges => self.body.is_some(),
            }
            && self.comments.iter().all(|comment| comment.line > 0)
    }
}

struct MetadataContract;
impl ToolContract for MetadataContract {
    type Arguments = PullRequestArguments;
    const NAME: &'static str = PULL_REQUEST_METADATA_NAME;
    const DESCRIPTION: &'static str =
        "Returns bounded metadata and exact base/head revisions for one GitHub pull request.";
}

struct DiffContract;
impl ToolContract for DiffContract {
    type Arguments = PullRequestArguments;
    const NAME: &'static str = PULL_REQUEST_DIFF_NAME;
    const DESCRIPTION: &'static str =
        "Returns bounded files and patches pinned to one exact GitHub pull-request base/head pair.";
}

struct ThreadsContract;
impl ToolContract for ThreadsContract {
    type Arguments = PullRequestArguments;
    const NAME: &'static str = PULL_REQUEST_REVIEW_THREADS_NAME;
    const DESCRIPTION: &'static str =
        "Returns bounded GitHub review threads and comments with resolved and outdated state.";
}

struct PublishReviewContract;
impl ToolContract for PublishReviewContract {
    type Arguments = PublishReviewArguments;
    const NAME: &'static str = PULL_REQUEST_PUBLISH_REVIEW_NAME;
    const DESCRIPTION: &'static str = "Publishes a comment, approval, or change request against an exact GitHub pull-request head, optionally with inline comments.";
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolKind {
    Diff,
    Metadata,
    PublishReview,
    ReviewThreads,
}

impl ToolKind {
    const ALL: [Self; 4] = [
        Self::Diff,
        Self::Metadata,
        Self::PublishReview,
        Self::ReviewThreads,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Diff => PULL_REQUEST_DIFF_NAME,
            Self::Metadata => PULL_REQUEST_METADATA_NAME,
            Self::PublishReview => PULL_REQUEST_PUBLISH_REVIEW_NAME,
            Self::ReviewThreads => PULL_REQUEST_REVIEW_THREADS_NAME,
        }
    }

    const fn mutates(self) -> bool {
        matches!(self, Self::PublishReview)
    }

    const fn permission(self) -> ToolPermissionDefault {
        if self.mutates() {
            ToolPermissionDefault::Confirm
        } else {
            ToolPermissionDefault::Auto
        }
    }

    fn definition(self) -> Result<ToolDefinition, ToolContractCompileError> {
        match self {
            Self::Diff => compile_contract_definition::<DiffContract>(
                self.permission(),
                ToolEffectClass::ExternalEffect,
            ),
            Self::Metadata => compile_contract_definition::<MetadataContract>(
                self.permission(),
                ToolEffectClass::ExternalEffect,
            ),
            Self::PublishReview => compile_contract_definition::<PublishReviewContract>(
                self.permission(),
                ToolEffectClass::ExternalEffect,
            ),
            Self::ReviewThreads => compile_contract_definition::<ThreadsContract>(
                self.permission(),
                ToolEffectClass::ExternalEffect,
            ),
        }
    }

    fn accepts(self, result: &GitHubResult) -> bool {
        matches!(
            (self, result.kind),
            (Self::Diff, GitHubResultKind::Diff)
                | (Self::Metadata, GitHubResultKind::Metadata)
                | (Self::PublishReview, GitHubResultKind::PublishedReview)
                | (Self::ReviewThreads, GitHubResultKind::ReviewThreads)
        )
    }
}

fn kind_for_name(name: &str) -> Option<ToolKind> {
    ToolKind::ALL.into_iter().find(|kind| kind.name() == name)
}

/// Typed operation crossing the injected transport boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitHubOperation {
    /// Metadata read.
    Metadata(PullRequestArguments),
    /// Exact-revision diff read.
    Diff(PullRequestArguments),
    /// Review-thread read.
    ReviewThreads(PullRequestArguments),
    /// Review publication.
    PublishReview(PublishReviewArguments),
}

impl GitHubOperation {
    /// Returns the originating tool name.
    pub const fn tool_name(&self) -> &'static str {
        match self {
            Self::Metadata(_) => PULL_REQUEST_METADATA_NAME,
            Self::Diff(_) => PULL_REQUEST_DIFF_NAME,
            Self::ReviewThreads(_) => PULL_REQUEST_REVIEW_THREADS_NAME,
            Self::PublishReview(_) => PULL_REQUEST_PUBLISH_REVIEW_NAME,
        }
    }
}

fn decode<Arguments: DeserializeOwned>(
    arguments: &NormalizedToolArguments,
) -> Result<Arguments, InvalidGitHubArguments> {
    serde_json::from_str(arguments.as_str()).map_err(|_| InvalidGitHubArguments)
}

fn decode_operation(
    kind: ToolKind,
    arguments: &NormalizedToolArguments,
) -> Result<GitHubOperation, InvalidGitHubArguments> {
    match kind {
        ToolKind::Diff => decode(arguments).map(GitHubOperation::Diff),
        ToolKind::Metadata => decode(arguments).map(GitHubOperation::Metadata),
        ToolKind::ReviewThreads => decode(arguments).map(GitHubOperation::ReviewThreads),
        ToolKind::PublishReview => {
            let decoded: PublishReviewArguments = decode(arguments)?;
            decoded
                .valid_combination()
                .then_some(GitHubOperation::PublishReview(decoded))
                .ok_or(InvalidGitHubArguments)
        }
    }
}

/// A model-provided GitHub argument failed its checked representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidGitHubArguments;

impl fmt::Display for InvalidGitHubArguments {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid GitHub tool arguments")
    }
}

#[derive(Clone, Debug)]
struct GitHubArgumentValidator {
    kind: ToolKind,
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for GitHubArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_operation(self.kind, arguments)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

/// Sanitized and bounded GitHub error-body excerpt.
#[derive(Clone, Eq, PartialEq)]
pub struct SanitizedGitHubError(String);

impl SanitizedGitHubError {
    /// Borrows the sanitized excerpt.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SanitizedGitHubError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SanitizedGitHubError([REDACTED])")
    }
}

/// Sanitized physical transport failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitHubTransportFailure {
    /// Credential bytes were unusable.
    InvalidCredential,
    /// GitHub definitively rejected the request.
    Rejected {
        /// HTTP status.
        status: u16,
        /// Sanitized response excerpt.
        detail: Option<SanitizedGitHubError>,
    },
    /// A success response violated the bounded contract.
    InvalidResponse {
        /// Sanitized response excerpt.
        detail: Option<SanitizedGitHubError>,
    },
    /// Response bytes exceeded the cap.
    ResponseTooLarge,
    /// Base or head changed during a diff read.
    RevisionChanged,
    /// Physical dispatch outcome is unknown.
    DispatchUnknown,
    /// The destination was outside the explicit policy.
    EgressRejected,
}

impl GitHubTransportFailure {
    /// Constructs a detail-free rejection for synthetic transports.
    pub const fn rejected(status: u16) -> Self {
        Self::Rejected {
            status,
            detail: None,
        }
    }
}

/// Result category crossing the injected transport boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitHubResultKind {
    /// Metadata.
    Metadata,
    /// Changed files.
    Diff,
    /// Review threads.
    ReviewThreads,
    /// Published review acknowledgement.
    PublishedReview,
}

/// JSON result with a redacted diagnostic representation.
#[derive(Clone, Eq, PartialEq)]
pub struct GitHubResult {
    kind: GitHubResultKind,
    value: serde_json::Value,
}

impl GitHubResult {
    /// Constructs a metadata result for an injected transport.
    pub fn metadata(value: serde_json::Value) -> Self {
        Self {
            kind: GitHubResultKind::Metadata,
            value,
        }
    }

    /// Constructs a diff result for an injected transport.
    pub fn diff(value: serde_json::Value) -> Self {
        Self {
            kind: GitHubResultKind::Diff,
            value,
        }
    }

    /// Constructs a review-thread result for an injected transport.
    pub fn review_threads(value: serde_json::Value) -> Self {
        Self {
            kind: GitHubResultKind::ReviewThreads,
            value,
        }
    }

    /// Constructs a review-publication result for an injected transport.
    pub fn published_review(value: serde_json::Value) -> Self {
        Self {
            kind: GitHubResultKind::PublishedReview,
            value,
        }
    }

    /// Returns the result category.
    pub const fn kind(&self) -> GitHubResultKind {
        self.kind
    }
}

impl fmt::Debug for GitHubResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            GitHubResultKind::Metadata => "GitHubResult::Metadata([REDACTED])",
            GitHubResultKind::Diff => "GitHubResult::Diff([REDACTED])",
            GitHubResultKind::ReviewThreads => "GitHubResult::ReviewThreads([REDACTED])",
            GitHubResultKind::PublishedReview => "GitHubResult::PublishedReview([REDACTED])",
        })
    }
}

/// Mockable request transport.
pub trait GitHubTransport: Send {
    /// Executes one operation with request-scoped credentials and exact egress.
    fn execute(
        &mut self,
        operation: GitHubOperation,
        credential: &CredentialValue,
        egress_policy: &GitHubEgressPolicy,
    ) -> impl Future<Output = Result<GitHubResult, GitHubTransportFailure>> + Send;
}

/// Four compiled declarations and their matching executor.
#[derive(Clone, Debug)]
pub struct GitHubTools<Credentials, Transport> {
    catalog: CompiledToolCatalog,
    executor: GitHubExecutor<Credentials, Transport>,
}

impl<Credentials, Transport> GitHubTools<Credentials, Transport> {
    /// Compiles the suite around injected credential and transport boundaries.
    pub fn try_new(
        credentials: Credentials,
        transport: Transport,
        egress_policy: GitHubEgressPolicy,
    ) -> Result<Self, GitHubToolsConstructionError> {
        let invalid_detail = make_detail(INVALID_ARGUMENTS_DETAIL)?;
        let credential_detail = make_detail(CREDENTIAL_UNAVAILABLE_DETAIL)?;
        let rejected_detail = make_detail(REQUEST_REJECTED_DETAIL)?;
        let revision_detail = make_detail(REVISION_CHANGED_DETAIL)?;
        let compiled = ToolKind::ALL
            .into_iter()
            .map(|kind| {
                let definition = kind.definition().map_err(|error| match error {
                    ToolContractCompileError::Name => GitHubToolsConstructionError::Name,
                    ToolContractCompileError::Schema => GitHubToolsConstructionError::Schema,
                })?;
                Ok(CompiledTool::new(
                    definition,
                    GitHubArgumentValidator {
                        kind,
                        detail: invalid_detail.clone(),
                    },
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let catalog = CompiledToolCatalog::try_new(compiled)
            .map_err(|_| GitHubToolsConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: GitHubExecutor {
                credentials,
                credential_reference: CredentialReference::new(GITHUB_CREDENTIAL_REFERENCE),
                transport,
                egress_policy,
                credential_detail,
                rejected_detail,
                revision_detail,
            },
        })
    }

    /// Separates catalog and executor composition roles.
    pub fn into_parts(self) -> (CompiledToolCatalog, GitHubExecutor<Credentials, Transport>) {
        (self.catalog, self.executor)
    }
}

impl<Credentials> GitHubTools<Credentials, GitHubApiTransport> {
    /// Builds the suite with the production GitHub transport.
    pub fn try_new_production(
        credentials: Credentials,
        egress_policy: GitHubEgressPolicy,
    ) -> Result<Self, GitHubToolsConstructionError> {
        let transport =
            GitHubApiTransport::try_new().map_err(|_| GitHubToolsConstructionError::Transport)?;
        Self::try_new(credentials, transport, egress_policy)
    }
}

/// Static suite construction failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitHubToolsConstructionError {
    /// Static name rejected.
    Name,
    /// Static schema rejected.
    Schema,
    /// Static sanitized detail rejected.
    ErrorDetail,
    /// Duplicate static name.
    Duplicate,
    /// Production transport construction failed.
    Transport,
}

impl fmt::Display for GitHubToolsConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GitHub pull-request tool construction failed")
    }
}

impl Error for GitHubToolsConstructionError {}

fn make_detail(value: &str) -> Result<ToolExecutionErrorDetail, GitHubToolsConstructionError> {
    ToolExecutionErrorDetail::try_new(value.to_owned())
        .map_err(|_| GitHubToolsConstructionError::ErrorDetail)
}

/// Credential-resolving executor.
#[derive(Clone, Debug)]
pub struct GitHubExecutor<Credentials, Transport> {
    credentials: Credentials,
    credential_reference: CredentialReference,
    transport: Transport,
    egress_policy: GitHubEgressPolicy,
    credential_detail: ToolExecutionErrorDetail,
    rejected_detail: ToolExecutionErrorDetail,
    revision_detail: ToolExecutionErrorDetail,
}

/// Sanitized executor failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GitHubExecutorError {
    class: OperatorFailureClass,
}

impl fmt::Display for GitHubExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GitHub pull-request executor failed")
    }
}

impl Error for GitHubExecutorError {}

impl ClassifyOperatorFailure for GitHubExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        self.class
    }
}

impl<Credentials, Transport> ToolExecutor for GitHubExecutor<Credentials, Transport>
where
    Credentials: CredentialAccess,
    Transport: GitHubTransport,
{
    type Error = GitHubExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let kind = kind_for_name(invocation.request().name().as_str()).ok_or_else(caller_bug)?;
        let operation =
            decode_operation(kind, invocation.request().arguments()).map_err(|_| caller_bug())?;
        let credential = match self.credentials.resolve(&self.credential_reference).await {
            Ok(value) => value,
            Err(_) => {
                return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                    detail: Some(self.credential_detail.clone()),
                }));
            }
        };
        let Some(scrubber) = CredentialScrubber::try_new(&credential) else {
            return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                detail: Some(self.credential_detail.clone()),
            }));
        };
        let mut result = match self
            .transport
            .execute(operation, &credential, &self.egress_policy)
            .await
        {
            Ok(result) if kind.accepts(&result) => result,
            Ok(_) => return Err(caller_bug()),
            Err(failure) => return self.failure_evidence(invocation, kind, failure),
        };
        scrubber.redact_value(&mut result.value);
        let content = serde_json::to_string(&result.value).map_err(|_| caller_bug())?;
        if content.len() > MAX_RESULT_BYTES {
            if kind.mutates() {
                return Err(infrastructure(true));
            }
            return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                detail: Some(self.rejected_detail.clone()),
            }));
        }
        Ok(invocation.bind(ToolExecutorEvidence::CompletedText(content)))
    }
}

impl<Credentials, Transport> GitHubExecutor<Credentials, Transport> {
    fn failure_evidence(
        &self,
        invocation: ToolExecutionInvocation,
        kind: ToolKind,
        failure: GitHubTransportFailure,
    ) -> Result<CorrelatedToolExecutorEvidence, GitHubExecutorError> {
        let detail = match failure {
            GitHubTransportFailure::InvalidCredential => self.credential_detail.clone(),
            GitHubTransportFailure::Rejected { status, detail }
                if !kind.mutates() || status < 500 =>
            {
                self.response_detail(status, detail.as_ref())
            }
            GitHubTransportFailure::InvalidResponse { detail } if !kind.mutates() => {
                self.invalid_response_detail(detail.as_ref())
            }
            GitHubTransportFailure::ResponseTooLarge if !kind.mutates() => {
                self.rejected_detail.clone()
            }
            GitHubTransportFailure::RevisionChanged => self.revision_detail.clone(),
            GitHubTransportFailure::EgressRejected => self.rejected_detail.clone(),
            GitHubTransportFailure::Rejected { .. }
            | GitHubTransportFailure::InvalidResponse { .. }
            | GitHubTransportFailure::ResponseTooLarge
            | GitHubTransportFailure::DispatchUnknown => {
                return Err(infrastructure(kind.mutates()));
            }
        };
        Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
            detail: Some(detail),
        }))
    }

    fn response_detail(
        &self,
        status: u16,
        body: Option<&SanitizedGitHubError>,
    ) -> ToolExecutionErrorDetail {
        body.and_then(|body| {
            ToolExecutionErrorDetail::try_new(format!(
                "GitHub returned HTTP {status}: {}",
                body.as_str()
            ))
            .ok()
        })
        .unwrap_or_else(|| self.rejected_detail.clone())
    }

    fn invalid_response_detail(
        &self,
        body: Option<&SanitizedGitHubError>,
    ) -> ToolExecutionErrorDetail {
        body.and_then(|body| {
            ToolExecutionErrorDetail::try_new(format!(
                "GitHub returned an invalid response: {}",
                body.as_str()
            ))
            .ok()
        })
        .unwrap_or_else(|| self.rejected_detail.clone())
    }
}

fn caller_bug() -> GitHubExecutorError {
    GitHubExecutorError {
        class: OperatorFailureClass::CallerOrHubBug,
    }
}

const fn infrastructure(commit_ambiguous: bool) -> GitHubExecutorError {
    GitHubExecutorError {
        class: OperatorFailureClass::Infrastructure { commit_ambiguous },
    }
}

struct CredentialScrubber {
    exact: String,
    escaped: String,
}

impl CredentialScrubber {
    fn try_new(credential: &CredentialValue) -> Option<Self> {
        let exact = std::str::from_utf8(credential.expose_bytes())
            .ok()?
            .to_owned();
        if exact.is_empty() {
            return None;
        }
        let encoded = serde_json::to_string(&exact).ok()?;
        let escaped = encoded.get(1..encoded.len().checked_sub(1)?)?.to_owned();
        Some(Self { exact, escaped })
    }

    fn redact_text(&self, text: String) -> String {
        let text = text.replace(&self.exact, "[redacted]");
        if self.escaped == self.exact {
            text
        } else {
            text.replace(&self.escaped, "[redacted]")
        }
    }

    fn redact_trailing_prefix(&self, mut text: String) -> String {
        let prefix = longest_trailing_prefix(&text, &self.exact)
            .max(longest_trailing_prefix(&text, &self.escaped));
        if prefix > 0 {
            text.truncate(text.len() - prefix);
            text.push_str("[redacted]");
        }
        text
    }

    fn redact_value(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => {
                *text = self.redact_text(std::mem::take(text));
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    self.redact_value(value);
                }
            }
            serde_json::Value::Object(values) => {
                for value in values.values_mut() {
                    self.redact_value(value);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }
}

fn longest_trailing_prefix(text: &str, secret: &str) -> usize {
    let limit = secret.len().min(text.len() + 1);
    (1..limit)
        .rev()
        .find(|length| text.ends_with(&secret[..*length]))
        .unwrap_or(0)
}

fn sanitize_error_body(
    bytes: &[u8],
    source_truncated: bool,
    scrubber: &CredentialScrubber,
) -> Option<SanitizedGitHubError> {
    let redacted = scrubber.redact_text(String::from_utf8_lossy(bytes).into_owned());
    let redacted = if source_truncated {
        scrubber.redact_trailing_prefix(redacted)
    } else {
        redacted
    };
    let normalized = redacted
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.is_empty() {
        None
    } else {
        Some(SanitizedGitHubError(truncate_sanitized(normalized)))
    }
}

fn truncate_sanitized(mut text: String) -> String {
    if text.len() <= MAX_ERROR_DETAIL_BYTES {
        return text;
    }
    let mut boundary = MAX_ERROR_DETAIL_BYTES - ERROR_TRUNCATION_SUFFIX.len();
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
    text.push_str(ERROR_TRUNCATION_SUFFIX);
    text
}

/// Production REST/GraphQL transport with no ambient proxy, redirect, or retry.
#[derive(Clone, Debug)]
pub struct GitHubApiTransport {
    timeout: Duration,
    rest_base: Url,
    graphql_url: Url,
}

impl GitHubApiTransport {
    /// Constructs the fixed production transport.
    pub fn try_new() -> Result<Self, GitHubApiTransportConstructionError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        Ok(Self {
            timeout: DEFAULT_TIMEOUT,
            rest_base: Url::parse(REST_BASE_URL)
                .map_err(|_| GitHubApiTransportConstructionError)?,
            graphql_url: Url::parse(GRAPHQL_URL)
                .map_err(|_| GitHubApiTransportConstructionError)?,
        })
    }

    fn repository_url(
        &self,
        repository: &GitHubRepository,
        suffix: &[&str],
        query: Option<&[(&str, &str)]>,
    ) -> Result<Url, GitHubTransportFailure> {
        let mut url = self.rest_base.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| GitHubTransportFailure::EgressRejected)?;
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

    async fn metadata_value(
        &self,
        arguments: &PullRequestArguments,
        credential: &CredentialValue,
        policy: &GitHubEgressPolicy,
    ) -> Result<serde_json::Value, GitHubTransportFailure> {
        let number = arguments.number().get().to_string();
        let url = self.repository_url(arguments.repository(), &["pulls", &number], None)?;
        let response = self
            .send(Method::GET, url, None, credential, policy)
            .await?;
        let value = self
            .success_json(response, StatusCode::OK, credential)
            .await?;
        normalize_metadata(&value, arguments.number())
    }

    async fn metadata(
        &self,
        arguments: PullRequestArguments,
        credential: &CredentialValue,
        policy: &GitHubEgressPolicy,
    ) -> Result<GitHubResult, GitHubTransportFailure> {
        self.metadata_value(&arguments, credential, policy)
            .await
            .map(GitHubResult::metadata)
    }

    async fn diff(
        &self,
        arguments: PullRequestArguments,
        credential: &CredentialValue,
        policy: &GitHubEgressPolicy,
    ) -> Result<GitHubResult, GitHubTransportFailure> {
        let initial = self.metadata_value(&arguments, credential, policy).await?;
        let initial_base = required_string(required_object(&initial)?, "base_revision")?;
        let initial_head = required_string(required_object(&initial)?, "head_revision")?;
        let mut files = Vec::new();
        let mut truncated = false;
        for page in 1..=MAX_FILE_PAGES {
            let page_text = page.to_string();
            let number = arguments.number().get().to_string();
            let url = self.repository_url(
                arguments.repository(),
                &["pulls", &number, "files"],
                Some(&[("per_page", PAGE_SIZE), ("page", &page_text)]),
            )?;
            let response = self
                .send(Method::GET, url, None, credential, policy)
                .await?;
            let has_next = response_has_next_page(&response);
            let value = self
                .success_json(response, StatusCode::OK, credential)
                .await?;
            files.extend(normalize_files(&value)?);
            if !has_next {
                break;
            }
            if page == MAX_FILE_PAGES {
                truncated = true;
            }
        }
        let current = self.metadata_value(&arguments, credential, policy).await?;
        let current_base = required_string(required_object(&current)?, "base_revision")?;
        let current_head = required_string(required_object(&current)?, "head_revision")?;
        if initial_base != current_base || initial_head != current_head {
            return Err(GitHubTransportFailure::RevisionChanged);
        }
        Ok(GitHubResult::diff(serde_json::json!({
            "base_revision": initial_base,
            "head_revision": initial_head,
            "files": files,
            "truncated": truncated,
        })))
    }

    async fn review_threads(
        &self,
        arguments: PullRequestArguments,
        credential: &CredentialValue,
        policy: &GitHubEgressPolicy,
    ) -> Result<GitHubResult, GitHubTransportFailure> {
        let body = serde_json::to_vec(&serde_json::json!({
            "query": REVIEW_THREADS_QUERY,
            "variables": {
                "owner": arguments.repository().owner(),
                "name": arguments.repository().name(),
                "number": arguments.number().get(),
            }
        }))
        .map_err(|_| invalid_response(None))?;
        let response = self
            .send(
                Method::POST,
                self.graphql_url.clone(),
                Some(body),
                credential,
                policy,
            )
            .await?;
        let value = self
            .success_json(response, StatusCode::OK, credential)
            .await?;
        if value.get("errors").is_some() {
            return Err(GitHubTransportFailure::Rejected {
                status: 200,
                detail: sanitized_value_detail(&value, credential),
            });
        }
        normalize_threads(&value).map(GitHubResult::review_threads)
    }

    async fn publish_review(
        &self,
        arguments: PublishReviewArguments,
        credential: &CredentialValue,
        policy: &GitHubEgressPolicy,
    ) -> Result<GitHubResult, GitHubTransportFailure> {
        let number = arguments.number().get().to_string();
        let url =
            self.repository_url(arguments.repository(), &["pulls", &number, "reviews"], None)?;
        let comments = arguments
            .comments()
            .iter()
            .map(|comment| {
                serde_json::json!({
                    "path": comment.path(),
                    "line": comment.line(),
                    "side": comment.side().api_value(),
                    "body": comment.body(),
                })
            })
            .collect::<Vec<_>>();
        let body = serde_json::to_vec(&serde_json::json!({
            "commit_id": arguments.commit_id().as_str(),
            "event": arguments.event().api_value(),
            "body": arguments.body(),
            "comments": comments,
        }))
        .map_err(|_| invalid_response(None))?;
        let response = self
            .send(Method::POST, url, Some(body), credential, policy)
            .await?;
        let value = self
            .success_json(response, StatusCode::OK, credential)
            .await
            .map_err(mutation_failure)?;
        normalize_published_review(&value, arguments.commit_id().as_str())
            .map(GitHubResult::published_review)
            .map_err(mutation_failure)
    }

    async fn send(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        credential: &CredentialValue,
        policy: &GitHubEgressPolicy,
    ) -> Result<Response, GitHubTransportFailure> {
        if !policy.admits(&url) {
            return Err(GitHubTransportFailure::EgressRejected);
        }
        let scrubber = CredentialScrubber::try_new(credential)
            .ok_or(GitHubTransportFailure::InvalidCredential)?;
        let mut authentication = Vec::with_capacity(7 + credential.expose_bytes().len());
        authentication.extend_from_slice(b"Bearer ");
        authentication.extend_from_slice(credential.expose_bytes());
        let mut authentication = HeaderValue::from_bytes(&authentication)
            .map_err(|_| GitHubTransportFailure::InvalidCredential)?;
        authentication.set_sensitive(true);
        let client = public_destination_client(&url, self.timeout)
            .await
            .map_err(classify_destination_failure)?;
        let mut request = client
            .request(method, url)
            .header(AUTHORIZATION, authentication)
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header(USER_AGENT, USER_AGENT_VALUE);
        if let Some(body) = body {
            request = request.header(CONTENT_TYPE, "application/json").body(body);
        }
        let response = request
            .send()
            .await
            .map_err(|_| GitHubTransportFailure::DispatchUnknown)?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let (body, truncated) = read_bounded(response, MAX_ERROR_SOURCE_BYTES).await?;
        let detail = sanitize_error_body(&body, truncated, &scrubber);
        Err(GitHubTransportFailure::Rejected { status, detail })
    }

    async fn success_json(
        &self,
        response: Response,
        expected: StatusCode,
        credential: &CredentialValue,
    ) -> Result<serde_json::Value, GitHubTransportFailure> {
        if response.status() != expected {
            return Err(invalid_response(None));
        }
        let (body, truncated) = read_bounded(response, MAX_RESPONSE_BYTES).await?;
        if truncated {
            return Err(GitHubTransportFailure::ResponseTooLarge);
        }
        let scrubber = CredentialScrubber::try_new(credential)
            .ok_or(GitHubTransportFailure::InvalidCredential)?;
        let mut value = serde_json::from_slice::<serde_json::Value>(&body)
            .map_err(|_| invalid_response(sanitize_error_body(&body, false, &scrubber)))?;
        scrubber.redact_value(&mut value);
        Ok(value)
    }
}

impl GitHubTransport for GitHubApiTransport {
    async fn execute(
        &mut self,
        operation: GitHubOperation,
        credential: &CredentialValue,
        policy: &GitHubEgressPolicy,
    ) -> Result<GitHubResult, GitHubTransportFailure> {
        match operation {
            GitHubOperation::Metadata(arguments) => {
                self.metadata(arguments, credential, policy).await
            }
            GitHubOperation::Diff(arguments) => self.diff(arguments, credential, policy).await,
            GitHubOperation::ReviewThreads(arguments) => {
                self.review_threads(arguments, credential, policy).await
            }
            GitHubOperation::PublishReview(arguments) => {
                self.publish_review(arguments, credential, policy).await
            }
        }
    }
}

/// Production transport construction failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GitHubApiTransportConstructionError;

impl fmt::Display for GitHubApiTransportConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GitHub API transport construction failed")
    }
}

impl Error for GitHubApiTransportConstructionError {}

fn mutation_failure(failure: GitHubTransportFailure) -> GitHubTransportFailure {
    match failure {
        GitHubTransportFailure::InvalidCredential
        | GitHubTransportFailure::Rejected { .. }
        | GitHubTransportFailure::EgressRejected => failure,
        GitHubTransportFailure::InvalidResponse { .. }
        | GitHubTransportFailure::ResponseTooLarge
        | GitHubTransportFailure::RevisionChanged
        | GitHubTransportFailure::DispatchUnknown => GitHubTransportFailure::DispatchUnknown,
    }
}

const fn classify_destination_failure(
    failure: PublicDestinationClientError,
) -> GitHubTransportFailure {
    match failure {
        PublicDestinationClientError::DestinationRejected => GitHubTransportFailure::EgressRejected,
        PublicDestinationClientError::Infrastructure => GitHubTransportFailure::DispatchUnknown,
    }
}

async fn read_bounded(
    response: Response,
    limit: usize,
) -> Result<(Vec<u8>, bool), GitHubTransportFailure> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| GitHubTransportFailure::DispatchUnknown)?;
        let remaining = limit.saturating_sub(body.len());
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
        if body.len() == limit {
            let truncated = has_more_response_bytes(&mut stream)
                .await
                .map_err(classify_more_bytes_failure)?;
            return Ok((body, truncated));
        }
    }
    Ok((body, false))
}

const fn classify_more_bytes_failure(_failure: WebFetchTransportFailure) -> GitHubTransportFailure {
    GitHubTransportFailure::DispatchUnknown
}

fn response_has_next_page(response: &Response) -> bool {
    response
        .headers()
        .get(LINK)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|link| link.contains("rel=\"next\"")))
}

fn normalize_metadata(
    value: &serde_json::Value,
    expected_number: PullRequestNumber,
) -> Result<serde_json::Value, GitHubTransportFailure> {
    let object = required_object(value)?;
    if required_u64(object, "number")? != u64::from(expected_number.get()) {
        return Err(invalid_response(None));
    }
    let base = required_object(required(object, "base")?)?;
    let head = required_object(required(object, "head")?)?;
    let title = checked_text(required_string(object, "title")?, true)?;
    let body = optional_string(object, "body")?
        .map(|body| checked_text(body, false))
        .transpose()?;
    let state = checked_text(required_string(object, "state")?, true)?;
    let author = optional_object_string(object, "user", "login")?
        .map(|author| checked_text(author, true))
        .transpose()?;
    Ok(serde_json::json!({
        "number": expected_number.get(),
        "title": title,
        "body": body,
        "state": state,
        "draft": required_bool(object, "draft")?,
        "author": author,
        "base_ref": checked_text(required_string(base, "ref")?, true)?,
        "base_revision": checked_revision(required_string(base, "sha")?)?,
        "head_ref": checked_text(required_string(head, "ref")?, true)?,
        "head_revision": checked_revision(required_string(head, "sha")?)?,
        "url": checked_url(required_string(object, "html_url")?)?,
    }))
}

fn normalize_files(
    value: &serde_json::Value,
) -> Result<Vec<serde_json::Value>, GitHubTransportFailure> {
    required_array(value)?
        .iter()
        .map(|value| {
            let object = required_object(value)?;
            let previous = optional_string(object, "previous_filename")?
                .map(checked_path)
                .transpose()?;
            let patch = optional_string(object, "patch")?
                .map(|patch| checked_text(patch, false))
                .transpose()?;
            Ok(serde_json::json!({
                "path": checked_path(required_string(object, "filename")?)?,
                "previous_path": previous,
                "status": checked_text(required_string(object, "status")?, true)?,
                "additions": required_u64(object, "additions")?,
                "deletions": required_u64(object, "deletions")?,
                "changes": required_u64(object, "changes")?,
                "patch": patch,
            }))
        })
        .collect()
}

fn normalize_threads(
    value: &serde_json::Value,
) -> Result<serde_json::Value, GitHubTransportFailure> {
    let connection = nested(
        value,
        &["data", "repository", "pullRequest", "reviewThreads"],
    )?;
    let object = required_object(connection)?;
    let threads = required_array(required(object, "nodes")?)?
        .iter()
        .map(normalize_thread)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::json!({
        "threads": threads,
        "truncated": nested_bool(connection, &["pageInfo", "hasNextPage"])?,
    }))
}

fn normalize_thread(
    value: &serde_json::Value,
) -> Result<serde_json::Value, GitHubTransportFailure> {
    let object = required_object(value)?;
    let comments_connection = required(object, "comments")?;
    let comments_object = required_object(comments_connection)?;
    let comments = required_array(required(comments_object, "nodes")?)?
        .iter()
        .map(normalize_comment)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::json!({
        "id": checked_text(required_string(object, "id")?, true)?,
        "resolved": required_bool(object, "isResolved")?,
        "outdated": required_bool(object, "isOutdated")?,
        "path": checked_path(required_string(object, "path")?)?,
        "line": optional_u64(object, "line")?,
        "comments": comments,
        "comments_truncated": nested_bool(comments_connection, &["pageInfo", "hasNextPage"])?,
    }))
}

fn normalize_comment(
    value: &serde_json::Value,
) -> Result<serde_json::Value, GitHubTransportFailure> {
    let object = required_object(value)?;
    let author = optional_object_string(object, "author", "login")?
        .map(|author| checked_text(author, true))
        .transpose()?;
    Ok(serde_json::json!({
        "id": checked_text(required_string(object, "id")?, true)?,
        "author": author,
        "body": checked_text(required_string(object, "body")?, false)?,
        "created_at": checked_text(required_string(object, "createdAt")?, true)?,
        "url": checked_url(required_string(object, "url")?)?,
    }))
}

fn normalize_published_review(
    value: &serde_json::Value,
    expected_commit: &str,
) -> Result<serde_json::Value, GitHubTransportFailure> {
    let object = required_object(value)?;
    let commit_id = checked_revision(required_string(object, "commit_id")?)?;
    if commit_id != expected_commit {
        return Err(invalid_response(None));
    }
    let id = required_u64(object, "id")?;
    if id == 0 {
        return Err(invalid_response(None));
    }
    Ok(serde_json::json!({
        "id": id,
        "state": checked_text(required_string(object, "state")?, true)?,
        "url": checked_url(required_string(object, "html_url")?)?,
        "commit_id": commit_id,
    }))
}

fn invalid_response(detail: Option<SanitizedGitHubError>) -> GitHubTransportFailure {
    GitHubTransportFailure::InvalidResponse { detail }
}

fn valid_text(value: &str, required: bool) -> bool {
    (!required || !value.is_empty()) && value.len() <= MAX_TEXT_BYTES && !value.contains('\0')
}

fn checked_text(value: String, required: bool) -> Result<String, GitHubTransportFailure> {
    valid_text(&value, required)
        .then_some(value)
        .ok_or_else(|| invalid_response(None))
}

fn checked_revision(value: String) -> Result<String, GitHubTransportFailure> {
    valid_revision(&value)
        .then_some(value)
        .ok_or_else(|| invalid_response(None))
}

fn checked_path(value: String) -> Result<String, GitHubTransportFailure> {
    (!value.is_empty()
        && !value.starts_with('/')
        && value.len() <= MAX_PATH_BYTES
        && !value.contains('\0'))
    .then_some(value)
    .ok_or_else(|| invalid_response(None))
}

fn checked_url(value: String) -> Result<String, GitHubTransportFailure> {
    let valid = value.len() <= MAX_URL_BYTES
        && !value.chars().any(char::is_control)
        && Url::parse(&value).is_ok_and(|url| {
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
        });
    valid.then_some(value).ok_or_else(|| invalid_response(None))
}

fn sanitized_value_detail(
    value: &serde_json::Value,
    credential: &CredentialValue,
) -> Option<SanitizedGitHubError> {
    let bytes = serde_json::to_vec(value).ok()?;
    let scrubber = CredentialScrubber::try_new(credential)?;
    sanitize_error_body(&bytes, false, &scrubber)
}

fn required_object(
    value: &serde_json::Value,
) -> Result<&serde_json::Map<String, serde_json::Value>, GitHubTransportFailure> {
    value.as_object().ok_or_else(|| invalid_response(None))
}

fn required_array(
    value: &serde_json::Value,
) -> Result<&Vec<serde_json::Value>, GitHubTransportFailure> {
    value.as_array().ok_or_else(|| invalid_response(None))
}

fn required<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<&'a serde_json::Value, GitHubTransportFailure> {
    object.get(field).ok_or_else(|| invalid_response(None))
}

fn required_string(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<String, GitHubTransportFailure> {
    required(object, field)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid_response(None))
}

fn optional_string(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<Option<String>, GitHubTransportFailure> {
    match object.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(str::to_owned)
            .map(Some)
            .ok_or_else(|| invalid_response(None)),
    }
}

fn required_u64(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<u64, GitHubTransportFailure> {
    required(object, field)?
        .as_u64()
        .ok_or_else(|| invalid_response(None))
}

fn optional_u64(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<Option<u64>, GitHubTransportFailure> {
    match object.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| invalid_response(None)),
    }
}

fn required_bool(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<bool, GitHubTransportFailure> {
    required(object, field)?
        .as_bool()
        .ok_or_else(|| invalid_response(None))
}

fn optional_object_string(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
    nested_field: &str,
) -> Result<Option<String>, GitHubTransportFailure> {
    match object.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => required_object(value)
            .and_then(|nested| required_string(nested, nested_field))
            .map(Some),
    }
}

fn nested<'a>(
    value: &'a serde_json::Value,
    path: &[&str],
) -> Result<&'a serde_json::Value, GitHubTransportFailure> {
    path.iter().try_fold(value, |current, segment| {
        required_object(current).and_then(|object| required(object, segment))
    })
}

fn nested_bool(value: &serde_json::Value, path: &[&str]) -> Result<bool, GitHubTransportFailure> {
    nested(value, path)?
        .as_bool()
        .ok_or_else(|| invalid_response(None))
}

#[cfg(test)]
mod tests {
    use signalbox_application::ToolCatalog;
    use signalbox_domain::ToolName;

    use super::*;

    const BASE_REVISION: &str = "1111111111111111111111111111111111111111";
    const HEAD_REVISION: &str = "2222222222222222222222222222222222222222";
    const SYNTHETIC_TOKEN: &str = "github_pat_synthetic_fixture_secret";

    struct SyntheticCredentials;
    struct SyntheticTransport;

    fn catalog() -> CompiledToolCatalog {
        GitHubTools::try_new(
            SyntheticCredentials,
            SyntheticTransport,
            GitHubEgressPolicy::github_api_only(),
        )
        .expect("static declarations compile")
        .into_parts()
        .0
    }

    fn normalized(value: serde_json::Value) -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(value.to_string())
            .expect("fixture arguments are admitted")
    }

    fn definition(catalog: &CompiledToolCatalog, name: &str) -> ToolDefinition {
        catalog
            .definition(&ToolName::try_new(name.to_owned()).expect("fixture name is admitted"))
            .expect("fixture declaration exists")
    }

    fn metadata_response() -> serde_json::Value {
        serde_json::json!({
            "number": 348,
            "title": "Exact revision repository reads",
            "body": "Synthetic body",
            "state": "open",
            "draft": false,
            "user": {"login": "fixture-author"},
            "base": {"ref": "main", "sha": BASE_REVISION},
            "head": {"ref": "agent/repository-read-tools", "sha": HEAD_REVISION},
            "html_url": "https://github.com/KeenWill/signalbox/pull/348"
        })
    }

    fn files_response() -> serde_json::Value {
        serde_json::json!([{
            "filename": "crates/example/src/lib.rs",
            "status": "modified",
            "additions": 4,
            "deletions": 2,
            "changes": 6,
            "patch": "@@ -1 +1 @@\n-old\n+new"
        }])
    }

    fn threads_response() -> serde_json::Value {
        serde_json::json!({
            "data": {"repository": {"pullRequest": {"reviewThreads": {
                "nodes": [{
                    "id": "PRRT_fixture",
                    "isResolved": false,
                    "isOutdated": false,
                    "path": "crates/example/src/lib.rs",
                    "line": 42,
                    "comments": {
                        "nodes": [{
                            "id": "PRRC_fixture",
                            "author": {"login": "reviewer"},
                            "body": "Please cover this edge.",
                            "createdAt": "2026-07-31T00:00:00Z",
                            "url": "https://github.com/KeenWill/signalbox/pull/1#discussion_r1"
                        }],
                        "pageInfo": {"hasNextPage": false}
                    }
                }],
                "pageInfo": {"hasNextPage": false}
            }}}}
        })
    }

    #[test]
    fn catalog_declares_three_auto_reads_and_one_confirmed_write() {
        let catalog = catalog();
        let diff = definition(&catalog, PULL_REQUEST_DIFF_NAME);
        let metadata = definition(&catalog, PULL_REQUEST_METADATA_NAME);
        let threads = definition(&catalog, PULL_REQUEST_REVIEW_THREADS_NAME);
        let publish = definition(&catalog, PULL_REQUEST_PUBLISH_REVIEW_NAME);

        assert_eq!(diff.permission_default(), ToolPermissionDefault::Auto);
        assert_eq!(metadata.permission_default(), ToolPermissionDefault::Auto);
        assert_eq!(threads.permission_default(), ToolPermissionDefault::Auto);
        assert_eq!(publish.permission_default(), ToolPermissionDefault::Confirm);
        assert_eq!(diff.effect_class(), ToolEffectClass::ExternalEffect);
        assert_eq!(metadata.effect_class(), ToolEffectClass::ExternalEffect);
        assert_eq!(threads.effect_class(), ToolEffectClass::ExternalEffect);
        assert_eq!(publish.effect_class(), ToolEffectClass::ExternalEffect);
    }

    #[test]
    fn egress_policy_admits_only_the_exact_github_api_origin() {
        let policy = GitHubEgressPolicy::github_api_only();
        let api = Url::parse("https://api.github.com/repos/KeenWill/signalbox")
            .expect("fixture URL is valid");
        let website =
            Url::parse("https://github.com/KeenWill/signalbox").expect("fixture URL is valid");
        let subdomain =
            Url::parse("https://evil.api.github.com/graphql").expect("fixture URL is valid");
        let plaintext = Url::parse("http://api.github.com/graphql").expect("fixture URL is valid");
        let other_port =
            Url::parse("https://api.github.com:444/graphql").expect("fixture URL is valid");

        assert!(policy.admits(&api));
        assert!(!policy.admits(&website));
        assert!(!policy.admits(&subdomain));
        assert!(!policy.admits(&plaintext));
        assert!(!policy.admits(&other_port));
    }

    #[test]
    fn publish_validator_requires_content_for_comment_and_change_request() {
        let catalog = catalog();
        let name = ToolName::try_new(PULL_REQUEST_PUBLISH_REVIEW_NAME.to_owned())
            .expect("fixture name is admitted");
        let empty_comment = normalized(serde_json::json!({
            "repository": "KeenWill/signalbox", "number": 1,
            "commit_id": HEAD_REVISION, "event": "comment", "comments": []
        }));
        let approval = normalized(serde_json::json!({
            "repository": "KeenWill/signalbox", "number": 1,
            "commit_id": HEAD_REVISION, "event": "approve", "comments": []
        }));

        assert!(catalog.validate_arguments(&name, &empty_comment).is_err());
        assert_eq!(catalog.validate_arguments(&name, &approval), Ok(()));
    }

    #[test]
    fn recorded_metadata_preserves_exact_base_and_head() {
        let number = PullRequestNumber::try_from(348).expect("fixture number is admitted");

        let parsed =
            normalize_metadata(&metadata_response(), number).expect("recorded response is valid");

        assert_eq!(parsed["base_revision"], BASE_REVISION);
        assert_eq!(parsed["head_revision"], HEAD_REVISION);
        assert_eq!(parsed["number"], 348);
    }

    #[test]
    fn recorded_files_preserve_patch_text() {
        let parsed = normalize_files(&files_response()).expect("recorded response is valid");

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["path"], "crates/example/src/lib.rs");
        assert_eq!(parsed[0]["patch"], "@@ -1 +1 @@\n-old\n+new");
    }

    #[test]
    fn graphql_recording_preserves_resolution_state_and_comments() {
        let parsed = normalize_threads(&threads_response()).expect("recorded response is valid");

        assert_eq!(parsed["truncated"], false);
        assert_eq!(parsed["threads"][0]["resolved"], false);
        assert_eq!(parsed["threads"][0]["outdated"], false);
        assert_eq!(parsed["threads"][0]["comments_truncated"], false);
        assert_eq!(
            parsed["threads"][0]["comments"][0]["body"],
            "Please cover this edge."
        );
    }

    #[test]
    fn error_body_redaction_precedes_truncation() {
        let credential = CredentialValue::new(SYNTHETIC_TOKEN.as_bytes().to_vec());
        let scrubber = CredentialScrubber::try_new(&credential).expect("fixture token is admitted");
        let prefix = "x".repeat(
            MAX_ERROR_DETAIL_BYTES - ERROR_TRUNCATION_SUFFIX.len() - "[redacted]".len() - 4,
        );
        let body = format!("{prefix}{SYNTHETIC_TOKEN}{}", "tail".repeat(100));

        let sanitized =
            sanitize_error_body(body.as_bytes(), false, &scrubber).expect("nonempty error remains");

        assert!(!sanitized.as_str().contains(SYNTHETIC_TOKEN));
        assert!(sanitized.as_str().contains("[redacted]"));
        assert!(sanitized.as_str().ends_with(ERROR_TRUNCATION_SUFFIX));
        assert!(sanitized.as_str().len() <= MAX_ERROR_DETAIL_BYTES);
        assert_eq!(format!("{sanitized:?}"), "SanitizedGitHubError([REDACTED])");
    }

    #[test]
    fn truncated_error_source_redacts_trailing_token_prefix() {
        let credential = CredentialValue::new(SYNTHETIC_TOKEN.as_bytes().to_vec());
        let scrubber = CredentialScrubber::try_new(&credential).expect("fixture token is admitted");
        let token_prefix = &SYNTHETIC_TOKEN[..SYNTHETIC_TOKEN.len() - 3];
        let body = format!("safe {token_prefix}");

        let sanitized =
            sanitize_error_body(body.as_bytes(), true, &scrubber).expect("nonempty error remains");

        assert_eq!(sanitized.as_str(), "safe [redacted]");
    }

    #[test]
    fn result_debug_never_formats_provider_content() {
        let result = GitHubResult::metadata(serde_json::json!({"body": SYNTHETIC_TOKEN}));

        let diagnostic = format!("{result:?}");

        assert_eq!(diagnostic, "GitHubResult::Metadata([REDACTED])");
        assert!(!diagnostic.contains(SYNTHETIC_TOKEN));
    }

    #[test]
    fn mutation_acknowledgement_is_pinned_to_requested_commit() {
        let response = serde_json::json!({
            "id": 7001,
            "state": "APPROVED",
            "html_url": "https://github.com/KeenWill/signalbox/pull/1#pullrequestreview-7001",
            "commit_id": HEAD_REVISION
        });

        let parsed = normalize_published_review(&response, HEAD_REVISION)
            .expect("recorded response is valid");

        assert_eq!(parsed["commit_id"], HEAD_REVISION);
        assert_eq!(parsed["state"], "APPROVED");
    }
}
