//! Credentialed GitHub pull-request tools.
//!
//! REST supplies pull-request metadata, changed-file patches, and review
//! publication. GraphQL supplies review threads because only that API exposes
//! thread resolution state.

use std::{
    borrow::Cow,
    collections::HashSet,
    error::Error,
    fmt,
    future::Future,
    time::{Duration, Instant},
};

use bstr::BStr;
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
    NormalizedToolArguments, ToolAttemptDispatchCorrelation, ToolEffectClass,
    ToolExecutionErrorDetail, ToolPermissionDefault,
};
use signalbox_egress_transport::{
    PublicDestinationClientError, WebFetchTransportFailure, has_more_response_bytes,
    public_destination_client,
};
use signalbox_model_runtime::{
    CredentialAccess, CredentialAccessError, CredentialReference, CredentialValue,
};
use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};

/// Pull-request metadata tool name.
pub const PULL_REQUEST_METADATA_NAME: &str = "github_pull_request_metadata";
/// Exact-revision changed-files tool name.
pub const PULL_REQUEST_DIFF_NAME: &str = "github_pull_request_diff";
/// Review-thread tool name.
pub const PULL_REQUEST_REVIEW_THREADS_NAME: &str = "github_pull_request_review_threads";
/// Review-publication tool name.
pub const PULL_REQUEST_PUBLISH_REVIEW_NAME: &str = "github_pull_request_publish_review";
/// Pull-request creation tool name.
pub const PULL_REQUEST_CREATE_NAME: &str = "github_pull_request_create";
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
const PAGE_SIZE: usize = 100;
const MAX_FILE_PAGES: u16 = 30;
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const JSON_NULL_BYTES: usize = 4;
const MAX_ERROR_SOURCE_BYTES: usize = 64 * 1024;
const MAX_ERROR_DETAIL_BYTES: usize = 2 * 1024;
const MAX_RESULT_BYTES: usize = 512 * 1024;
const MAX_REPOSITORY_BYTES: usize = 256;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_PATH_BYTES: usize = 4 * 1024;
const MAX_URL_BYTES: usize = 8 * 1024;
const MAX_OPAQUE_ID_BYTES: usize = 512;
const MIN_INLINE_COMMENT_LINE: u32 = 1;
const MAX_INLINE_COMMENTS: usize = 50;
const MAX_GIT_REF_BYTES: usize = 255;
const MAX_GITHUB_ACCOUNT_BYTES: usize = 39;
const ERROR_TRUNCATION_SUFFIX: &str = " … [truncated]";
const INVALID_ARGUMENTS_DETAIL: &str = "GitHub pull-request tool arguments are invalid";
const CREDENTIAL_UNAVAILABLE_DETAIL: &str = "GitHub credential is unavailable";
const REQUEST_REJECTED_DETAIL: &str = "GitHub rejected the pull-request operation";
const REVISION_CHANGED_DETAIL: &str = "pull-request diff snapshot changed during the diff read";

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

/// Pull-request creation arguments. The target repository is injected into the
/// creation suite and deliberately absent from this model-owned shape.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreatePullRequestArguments {
    /// Non-empty pull-request title.
    #[schemars(length(min = 1, max = MAX_TEXT_BYTES))]
    title: String,
    /// Pull-request body, including an intentionally empty body.
    #[schemars(length(max = MAX_TEXT_BYTES))]
    body: String,
    /// Source branch or GitHub head selector.
    #[schemars(length(min = 1, max = MAX_GIT_REF_BYTES))]
    head: String,
    /// Target branch.
    #[schemars(length(min = 1, max = MAX_GIT_REF_BYTES))]
    base: String,
}

impl CreatePullRequestArguments {
    /// Borrows the model-supplied title.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Borrows the model-supplied body.
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Borrows the source selector.
    pub fn head(&self) -> &str {
        &self.head
    }

    /// Borrows the target branch.
    pub fn base(&self) -> &str {
        &self.base
    }

    fn validate(&self) -> Result<(), InvalidGitHubArguments> {
        if valid_text(&self.title, TextPresence::Required)
            && valid_text(&self.body, TextPresence::Optional)
            && valid_head_selector(&self.head)
            && valid_git_ref(&self.base)
        {
            Ok(())
        } else {
            Err(InvalidGitHubArguments)
        }
    }
}

fn valid_git_ref(value: &str) -> bool {
    let reference = format!("refs/heads/{value}");
    !value.is_empty()
        && value.len() <= MAX_GIT_REF_BYTES
        && value != "@"
        && gix_validate::reference::branch_name(BStr::new(reference.as_bytes())).is_ok()
}

/// Accepts one local ref or one `account:ref` cross-repository head selector.
fn valid_head_selector(value: &str) -> bool {
    if value.len() > MAX_GIT_REF_BYTES {
        return false;
    }
    if valid_git_ref(value) {
        return true;
    }
    match value.split_once(':') {
        Some((account, reference)) => valid_github_account(account) && valid_git_ref(reference),
        None => false,
    }
}

fn valid_github_account(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_GITHUB_ACCOUNT_BYTES
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
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

    const fn acknowledged_state(self) -> &'static str {
        match self {
            Self::Comment => "COMMENTED",
            Self::Approve => "APPROVED",
            Self::RequestChanges => "CHANGES_REQUESTED",
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
        (!value.is_empty() && valid_text(&value, TextPresence::Optional))
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(try_from = "u32")]
struct PositiveInlineLine(u32);

impl PositiveInlineLine {
    const fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for PositiveInlineLine {
    type Error = InvalidGitHubArguments;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        (value >= MIN_INLINE_COMMENT_LINE)
            .then_some(Self(value))
            .ok_or(InvalidGitHubArguments)
    }
}

impl JsonSchema for PositiveInlineLine {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("PositiveInlineLine")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "integer", "minimum": MIN_INLINE_COMMENT_LINE})
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
    line: PositiveInlineLine,
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
        self.line.get()
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
#[derive(Clone, Debug, Eq, PartialEq, JsonSchema)]
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
    #[schemars(length(max = MAX_INLINE_COMMENTS))]
    comments: Vec<InlineReviewComment>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishReviewArgumentsWire {
    repository: GitHubRepository,
    number: PullRequestNumber,
    commit_id: GitHubRevision,
    event: PublishReviewEvent,
    body: Option<BoundedText>,
    #[serde(default)]
    comments: Vec<InlineReviewComment>,
}

impl<'de> Deserialize<'de> for PublishReviewArguments {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let wire = PublishReviewArgumentsWire::deserialize(deserializer)?;
        let arguments = Self {
            repository: wire.repository,
            number: wire.number,
            commit_id: wire.commit_id,
            event: wire.event,
            body: wire.body,
            comments: wire.comments,
        };
        if arguments.valid_combination() {
            Ok(arguments)
        } else {
            Err(<Deserializer::Error as serde::de::Error>::custom(
                "invalid publish review combination",
            ))
        }
    }
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
                PublishReviewEvent::Comment => self.body.is_some(),
                PublishReviewEvent::RequestChanges => self.body.is_some(),
            }
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

struct CreatePullRequestContract;
impl ToolContract for CreatePullRequestContract {
    type Arguments = CreatePullRequestArguments;
    const NAME: &'static str = PULL_REQUEST_CREATE_NAME;
    const DESCRIPTION: &'static str = "Creates one pull request in the deployment-configured GitHub repository from exact title, body, head, and base values.";
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolKind {
    Diff,
    Metadata,
    PublishReview,
    ReviewThreads,
}

impl ToolKind {
    fn all() -> impl Iterator<Item = Self> {
        std::iter::successors(Some(Self::Diff), |kind| kind.successor())
    }

    const fn successor(self) -> Option<Self> {
        match self {
            Self::Diff => Some(Self::Metadata),
            Self::Metadata => Some(Self::PublishReview),
            Self::PublishReview => Some(Self::ReviewThreads),
            Self::ReviewThreads => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Diff => PULL_REQUEST_DIFF_NAME,
            Self::Metadata => PULL_REQUEST_METADATA_NAME,
            Self::PublishReview => PULL_REQUEST_PUBLISH_REVIEW_NAME,
            Self::ReviewThreads => PULL_REQUEST_REVIEW_THREADS_NAME,
        }
    }

    const fn mutates(self) -> bool {
        match self {
            Self::Diff | Self::Metadata | Self::ReviewThreads => false,
            Self::PublishReview => true,
        }
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
        match self {
            Self::Diff => result.kind == GitHubResultKind::Diff,
            Self::Metadata => result.kind == GitHubResultKind::Metadata,
            Self::PublishReview => result.kind == GitHubResultKind::PublishedReview,
            Self::ReviewThreads => result.kind == GitHubResultKind::ReviewThreads,
        }
    }
}

fn kind_for_name(name: &str) -> Option<ToolKind> {
    ToolKind::all().find(|kind| kind.name() == name)
}

/// Typed operation crossing the injected transport boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitHubOperation {
    /// Pull-request creation in a deployment-configured repository.
    CreatePullRequest {
        /// Checked deployment repository, never model supplied.
        repository: GitHubRepository,
        /// Model-owned pull-request content and refs.
        arguments: CreatePullRequestArguments,
    },
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
            Self::CreatePullRequest { .. } => PULL_REQUEST_CREATE_NAME,
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

impl Error for InvalidGitHubArguments {}

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
    /// GitHub returned definitive GraphQL rejection evidence.
    GraphQlRejected,
    /// A success response violated the bounded contract.
    InvalidResponse {
        /// Sanitized response excerpt.
        detail: Option<SanitizedGitHubError>,
    },
    /// Response bytes exceeded the cap.
    ResponseTooLarge,
    /// Base or head changed during a diff read.
    RevisionChanged,
    /// Client setup failed before the request could be dispatched.
    PreDispatchInfrastructure,
    /// Physical dispatch outcome is unknown.
    DispatchUnknown,
    /// The destination was outside the explicit policy.
    EgressRejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GitHubTransportFailureClass {
    InvalidCredential,
    Rejected,
    GraphQlRejected,
    InvalidResponse,
    ResponseTooLarge,
    RevisionChanged,
    PreDispatchInfrastructure,
    DispatchUnknown,
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

    const fn class(&self) -> GitHubTransportFailureClass {
        match self {
            Self::InvalidCredential => GitHubTransportFailureClass::InvalidCredential,
            Self::Rejected { .. } => GitHubTransportFailureClass::Rejected,
            Self::GraphQlRejected => GitHubTransportFailureClass::GraphQlRejected,
            Self::InvalidResponse { .. } => GitHubTransportFailureClass::InvalidResponse,
            Self::ResponseTooLarge => GitHubTransportFailureClass::ResponseTooLarge,
            Self::RevisionChanged => GitHubTransportFailureClass::RevisionChanged,
            Self::PreDispatchInfrastructure => {
                GitHubTransportFailureClass::PreDispatchInfrastructure
            }
            Self::DispatchUnknown => GitHubTransportFailureClass::DispatchUnknown,
            Self::EgressRejected => GitHubTransportFailureClass::EgressRejected,
        }
    }
}

impl fmt::Display for GitHubTransportFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCredential => formatter.write_str("invalid GitHub credential"),
            Self::Rejected { status, detail: _ } => {
                write!(
                    formatter,
                    "GitHub rejected the request with HTTP status {status}"
                )
            }
            Self::GraphQlRejected => {
                formatter.write_str("GitHub returned definitive GraphQL rejection evidence")
            }
            Self::InvalidResponse { detail: _ } => {
                formatter.write_str("GitHub returned an invalid response")
            }
            Self::ResponseTooLarge => formatter.write_str("GitHub response exceeded the byte cap"),
            Self::RevisionChanged => {
                formatter.write_str("GitHub pull-request revision changed during the read")
            }
            Self::PreDispatchInfrastructure => {
                formatter.write_str("GitHub request could not be dispatched")
            }
            Self::DispatchUnknown => formatter.write_str("GitHub request outcome is unknown"),
            Self::EgressRejected => formatter.write_str("GitHub request destination was rejected"),
        }
    }
}

impl Error for GitHubTransportFailure {}

/// Result category crossing the injected transport boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitHubResultKind {
    /// Created pull-request acknowledgement.
    CreatedPullRequest,
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
    /// Constructs a pull-request creation result for an injected transport.
    pub fn created_pull_request(value: serde_json::Value) -> Self {
        Self {
            kind: GitHubResultKind::CreatedPullRequest,
            value,
        }
    }

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
            GitHubResultKind::CreatedPullRequest => "GitHubResult::CreatedPullRequest([REDACTED])",
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
        let compiled = ToolKind::all()
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

#[derive(Clone, Debug)]
struct CreatePullRequestArgumentValidator {
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for CreatePullRequestArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_create_pull_request(arguments)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

fn decode_create_pull_request(
    arguments: &NormalizedToolArguments,
) -> Result<CreatePullRequestArguments, InvalidGitHubArguments> {
    let arguments: CreatePullRequestArguments = decode(arguments)?;
    arguments.validate()?;
    Ok(arguments)
}

/// One approval-gated pull-request creation declaration and executor.
#[derive(Clone, Debug)]
pub struct GitHubPullRequestCreateTools<Credentials, Transport> {
    catalog: CompiledToolCatalog,
    executor: GitHubPullRequestCreateExecutor<Credentials, Transport>,
}

impl<Credentials, Transport> GitHubPullRequestCreateTools<Credentials, Transport> {
    /// Compiles creation around a configured repository, request-scoped
    /// credentials, fixed egress policy, and injected transport.
    pub fn try_new(
        credentials: Credentials,
        transport: Transport,
        egress_policy: GitHubEgressPolicy,
        repository: GitHubRepository,
    ) -> Result<Self, GitHubToolsConstructionError> {
        let invalid_detail = make_detail(INVALID_ARGUMENTS_DETAIL)?;
        let credential_detail = make_detail(CREDENTIAL_UNAVAILABLE_DETAIL)?;
        let rejected_detail = make_detail(REQUEST_REJECTED_DETAIL)?;
        let definition = compile_contract_definition::<CreatePullRequestContract>(
            ToolPermissionDefault::Confirm,
            ToolEffectClass::ExternalEffect,
        )
        .map_err(|error| match error {
            ToolContractCompileError::Name => GitHubToolsConstructionError::Name,
            ToolContractCompileError::Schema => GitHubToolsConstructionError::Schema,
        })?;
        let catalog = CompiledToolCatalog::try_new(vec![CompiledTool::new(
            definition,
            CreatePullRequestArgumentValidator {
                detail: invalid_detail,
            },
        )])
        .map_err(|_| GitHubToolsConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: GitHubPullRequestCreateExecutor {
                credentials,
                credential_reference: CredentialReference::new(GITHUB_CREDENTIAL_REFERENCE),
                transport,
                egress_policy,
                repository,
                credential_detail,
                rejected_detail,
            },
        })
    }

    /// Separates catalog and executor composition roles.
    pub fn into_parts(
        self,
    ) -> (
        CompiledToolCatalog,
        GitHubPullRequestCreateExecutor<Credentials, Transport>,
    ) {
        (self.catalog, self.executor)
    }
}

impl<Credentials> GitHubPullRequestCreateTools<Credentials, GitHubApiTransport> {
    /// Builds creation with the production fixed-origin GitHub transport.
    pub fn try_new_production(
        credentials: Credentials,
        egress_policy: GitHubEgressPolicy,
        repository: GitHubRepository,
    ) -> Result<Self, GitHubToolsConstructionError> {
        let transport =
            GitHubApiTransport::try_new().map_err(|_| GitHubToolsConstructionError::Transport)?;
        Self::try_new(credentials, transport, egress_policy, repository)
    }
}

/// Credential-resolving pull-request creation executor.
#[derive(Clone, Debug)]
pub struct GitHubPullRequestCreateExecutor<Credentials, Transport> {
    credentials: Credentials,
    credential_reference: CredentialReference,
    transport: Transport,
    egress_policy: GitHubEgressPolicy,
    repository: GitHubRepository,
    credential_detail: ToolExecutionErrorDetail,
    rejected_detail: ToolExecutionErrorDetail,
}

impl<Credentials, Transport> ToolExecutor
    for GitHubPullRequestCreateExecutor<Credentials, Transport>
where
    Credentials: CredentialAccess,
    Transport: GitHubTransport,
{
    type Error = GitHubExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        if invocation.request().name().as_str() != PULL_REQUEST_CREATE_NAME {
            return Err(caller_bug());
        }
        let arguments = decode_create_pull_request(invocation.request().arguments())
            .map_err(|_| caller_bug())?;
        let credential = match self.credentials.resolve(&self.credential_reference).await {
            Ok(value) => value,
            Err(error) => {
                let correlation = invocation.correlation();
                report_credential_access_failure(&error, &correlation);
                return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                    detail: Some(self.credential_detail.clone()),
                }));
            }
        };
        let Some(scrubber) = CredentialScrubber::try_new(&credential) else {
            let correlation = invocation.correlation();
            report_credential_value_failure(&correlation);
            return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                detail: Some(self.credential_detail.clone()),
            }));
        };
        let operation = GitHubOperation::CreatePullRequest {
            repository: self.repository.clone(),
            arguments,
        };
        let mut result = match self
            .transport
            .execute(operation, &credential, &self.egress_policy)
            .await
        {
            Ok(result) if result.kind() == GitHubResultKind::CreatedPullRequest => result,
            Ok(_) => return Err(infrastructure(CommitOutcome::Ambiguous)),
            Err(failure) => return self.failure_evidence(invocation, failure),
        };
        scrubber.redact_value(&mut result.value);
        let content = serde_json::to_string(&result.value).map_err(|_| caller_bug())?;
        if content.len() > MAX_RESULT_BYTES {
            return Err(infrastructure(CommitOutcome::Ambiguous));
        }
        Ok(invocation.bind(ToolExecutorEvidence::CompletedText(content)))
    }
}

impl<Credentials, Transport> GitHubPullRequestCreateExecutor<Credentials, Transport> {
    fn failure_evidence(
        &self,
        invocation: ToolExecutionInvocation,
        failure: GitHubTransportFailure,
    ) -> Result<CorrelatedToolExecutorEvidence, GitHubExecutorError> {
        let correlation = invocation.correlation();
        report_transport_failure(&failure, &correlation);
        let detail = self.failure_detail(&failure)?;
        Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
            detail: Some(detail),
        }))
    }

    fn failure_detail(
        &self,
        failure: &GitHubTransportFailure,
    ) -> Result<ToolExecutionErrorDetail, GitHubExecutorError> {
        let detail = match failure {
            GitHubTransportFailure::InvalidCredential => self.credential_detail.clone(),
            GitHubTransportFailure::Rejected { status, .. } if status_is_definitive(*status) => {
                self.rejected_detail.clone()
            }
            GitHubTransportFailure::GraphQlRejected | GitHubTransportFailure::EgressRejected => {
                self.rejected_detail.clone()
            }
            GitHubTransportFailure::PreDispatchInfrastructure => {
                return Err(infrastructure(CommitOutcome::Definite));
            }
            GitHubTransportFailure::Rejected { .. }
            | GitHubTransportFailure::InvalidResponse { .. }
            | GitHubTransportFailure::ResponseTooLarge
            | GitHubTransportFailure::RevisionChanged
            | GitHubTransportFailure::DispatchUnknown => {
                return Err(infrastructure(CommitOutcome::Ambiguous));
            }
        };
        Ok(detail)
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
            Err(error) => {
                let correlation = invocation.correlation();
                report_credential_access_failure(&error, &correlation);
                return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                    detail: Some(self.credential_detail.clone()),
                }));
            }
        };
        let Some(scrubber) = CredentialScrubber::try_new(&credential) else {
            let correlation = invocation.correlation();
            report_credential_value_failure(&correlation);
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
            Ok(_) => return Err(result_kind_mismatch(kind)),
            Err(failure) => return self.failure_evidence(invocation, kind, failure),
        };
        scrubber.redact_value(&mut result.value);
        let mut content = serde_json::to_string(&result.value).map_err(|_| caller_bug())?;
        if content.len() > MAX_RESULT_BYTES && matches!(kind, ToolKind::Diff) {
            truncate_diff_result(&mut result.value).map_err(|_| caller_bug())?;
            content = serde_json::to_string(&result.value).map_err(|_| caller_bug())?;
        }
        if content.len() > MAX_RESULT_BYTES && matches!(kind, ToolKind::ReviewThreads) {
            truncate_review_threads_result(&mut result.value).map_err(|_| caller_bug())?;
            content = serde_json::to_string(&result.value).map_err(|_| caller_bug())?;
        }
        if content.len() > MAX_RESULT_BYTES {
            if kind.mutates() {
                return Err(infrastructure(CommitOutcome::Ambiguous));
            }
            return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                detail: Some(self.rejected_detail.clone()),
            }));
        }
        Ok(invocation.bind(ToolExecutorEvidence::CompletedText(content)))
    }
}

fn truncate_diff_result(value: &mut serde_json::Value) -> Result<(), InvalidGitHubArguments> {
    let object = value.as_object_mut().ok_or(InvalidGitHubArguments)?;
    let files = object.remove("files").ok_or(InvalidGitHubArguments)?;
    let serde_json::Value::Array(files) = files else {
        return Err(InvalidGitHubArguments);
    };
    object.insert("files".to_owned(), serde_json::Value::Array(Vec::new()));
    object.insert("truncated".to_owned(), serde_json::Value::Bool(true));
    let empty_size = serde_json::to_vec(&serde_json::Value::Object(object.clone()))
        .map_err(|_| InvalidGitHubArguments)?
        .len();
    let mut remaining = MAX_RESULT_BYTES
        .checked_sub(empty_size)
        .ok_or(InvalidGitHubArguments)?;
    let mut retained = Vec::new();
    let mut patches = Vec::new();
    for mut file in files {
        let patch = file
            .as_object_mut()
            .and_then(|object| object.get_mut("patch"))
            .ok_or(InvalidGitHubArguments)?;
        let patch = std::mem::replace(patch, serde_json::Value::Null);
        if !patch.is_null() && !patch.is_string() {
            return Err(InvalidGitHubArguments);
        }
        let separator = usize::from(!retained.is_empty());
        let encoded_size = serde_json::to_vec(&file)
            .map_err(|_| InvalidGitHubArguments)?
            .len();
        if separator + encoded_size > remaining {
            break;
        }
        remaining -= separator + encoded_size;
        retained.push(file);
        patches.push(patch);
    }
    for (file, patch) in retained.iter_mut().zip(patches) {
        if patch.is_null() {
            continue;
        }
        let encoded_size = serde_json::to_vec(&patch)
            .map_err(|_| InvalidGitHubArguments)?
            .len();
        let additional_size = encoded_size.saturating_sub(JSON_NULL_BYTES);
        if additional_size > remaining {
            continue;
        }
        let retained_patch = file
            .as_object_mut()
            .and_then(|object| object.get_mut("patch"))
            .ok_or(InvalidGitHubArguments)?;
        *retained_patch = patch;
        remaining -= additional_size;
    }
    object.insert("files".to_owned(), serde_json::Value::Array(retained));
    Ok(())
}

fn truncate_review_threads_result(
    value: &mut serde_json::Value,
) -> Result<(), InvalidGitHubArguments> {
    let object = value.as_object_mut().ok_or(InvalidGitHubArguments)?;
    let threads_were_truncated = object
        .get("truncated")
        .and_then(serde_json::Value::as_bool)
        .ok_or(InvalidGitHubArguments)?;
    let threads = object.remove("threads").ok_or(InvalidGitHubArguments)?;
    let serde_json::Value::Array(threads) = threads else {
        return Err(InvalidGitHubArguments);
    };
    object.insert("threads".to_owned(), serde_json::Value::Array(Vec::new()));
    object.insert(
        "truncated".to_owned(),
        serde_json::Value::Bool(threads_were_truncated),
    );
    let empty_size = serde_json::to_vec(&serde_json::Value::Object(object.clone()))
        .map_err(|_| InvalidGitHubArguments)?
        .len();
    let mut remaining = MAX_RESULT_BYTES
        .checked_sub(empty_size)
        .ok_or(InvalidGitHubArguments)?;
    let mut reserved_threads = Vec::new();
    for mut thread in threads {
        let (comments, comments_were_truncated) = {
            let thread_object = thread.as_object_mut().ok_or(InvalidGitHubArguments)?;
            let comments = thread_object
                .remove("comments")
                .ok_or(InvalidGitHubArguments)?;
            let serde_json::Value::Array(comments) = comments else {
                return Err(InvalidGitHubArguments);
            };
            let comments_were_truncated = thread_object
                .get("comments_truncated")
                .and_then(serde_json::Value::as_bool)
                .ok_or(InvalidGitHubArguments)?;
            thread_object.insert("comments".to_owned(), serde_json::Value::Array(Vec::new()));
            thread_object.insert(
                "comments_truncated".to_owned(),
                serde_json::Value::Bool(comments_were_truncated || !comments.is_empty()),
            );
            (comments, comments_were_truncated)
        };
        let thread_separator = usize::from(!reserved_threads.is_empty());
        let thread_size = serde_json::to_vec(&thread)
            .map_err(|_| InvalidGitHubArguments)?
            .len();
        if thread_separator + thread_size > remaining {
            break;
        }
        remaining -= thread_separator + thread_size;
        reserved_threads.push((thread, comments, comments_were_truncated));
    }
    let mut retained_threads = Vec::with_capacity(reserved_threads.len());
    for (mut thread, comments, comments_were_truncated) in reserved_threads {
        let total_comments = comments.len();
        let mut retained_comments = Vec::new();
        for comment in comments {
            let comment_separator = usize::from(!retained_comments.is_empty());
            let comment_size = serde_json::to_vec(&comment)
                .map_err(|_| InvalidGitHubArguments)?
                .len();
            let completes_original = retained_comments.len() + 1 == total_comments;
            let completion_cost = usize::from(completes_original && !comments_were_truncated);
            if comment_separator + comment_size + completion_cost > remaining {
                break;
            }
            remaining -= comment_separator + comment_size + completion_cost;
            retained_comments.push(comment);
        }
        let retained_every_comment = retained_comments.len() == total_comments;
        let thread_object = thread.as_object_mut().ok_or(InvalidGitHubArguments)?;
        thread_object.insert(
            "comments".to_owned(),
            serde_json::Value::Array(retained_comments),
        );
        thread_object.insert(
            "comments_truncated".to_owned(),
            serde_json::Value::Bool(comments_were_truncated || !retained_every_comment),
        );
        retained_threads.push(thread);
    }
    object.insert(
        "threads".to_owned(),
        serde_json::Value::Array(retained_threads),
    );
    object.insert("truncated".to_owned(), serde_json::Value::Bool(true));
    Ok(())
}

impl<Credentials, Transport> GitHubExecutor<Credentials, Transport> {
    fn failure_evidence(
        &self,
        invocation: ToolExecutionInvocation,
        kind: ToolKind,
        failure: GitHubTransportFailure,
    ) -> Result<CorrelatedToolExecutorEvidence, GitHubExecutorError> {
        let correlation = invocation.correlation();
        report_transport_failure(&failure, &correlation);
        let detail = self.failure_detail(kind, &failure)?;
        Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
            detail: Some(detail),
        }))
    }

    fn failure_detail(
        &self,
        kind: ToolKind,
        failure: &GitHubTransportFailure,
    ) -> Result<ToolExecutionErrorDetail, GitHubExecutorError> {
        let detail = match failure {
            GitHubTransportFailure::InvalidCredential => self.credential_detail.clone(),
            GitHubTransportFailure::Rejected { status, .. } if status_is_definitive(*status) => {
                self.rejected_detail.clone()
            }
            GitHubTransportFailure::GraphQlRejected => self.rejected_detail.clone(),
            GitHubTransportFailure::InvalidResponse { .. } if !kind.mutates() => {
                self.rejected_detail.clone()
            }
            GitHubTransportFailure::ResponseTooLarge if !kind.mutates() => {
                self.rejected_detail.clone()
            }
            GitHubTransportFailure::RevisionChanged => self.revision_detail.clone(),
            GitHubTransportFailure::EgressRejected => self.rejected_detail.clone(),
            GitHubTransportFailure::PreDispatchInfrastructure => {
                return Err(infrastructure(CommitOutcome::Definite));
            }
            GitHubTransportFailure::Rejected { .. }
            | GitHubTransportFailure::InvalidResponse { .. }
            | GitHubTransportFailure::ResponseTooLarge
            | GitHubTransportFailure::DispatchUnknown => {
                return Err(infrastructure(kind.commit_outcome()));
            }
        };
        Ok(detail)
    }
}

fn report_credential_access_failure(
    error: &CredentialAccessError,
    correlation: &ToolAttemptDispatchCorrelation,
) {
    tracing::warn!(
        target: "signalbox_tools_github",
        failure = ?error.failure,
        session_id = %correlation.session().as_uuid(),
        turn_id = %correlation.turn().as_uuid(),
        "GitHub credential resolution failed"
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CredentialValueFailure {
    Unusable,
}

fn report_credential_value_failure(correlation: &ToolAttemptDispatchCorrelation) {
    tracing::warn!(
        target: "signalbox_tools_github",
        failure = ?CredentialValueFailure::Unusable,
        session_id = %correlation.session().as_uuid(),
        turn_id = %correlation.turn().as_uuid(),
        "GitHub credential value was unusable"
    );
}

fn report_transport_failure(
    failure: &GitHubTransportFailure,
    correlation: &ToolAttemptDispatchCorrelation,
) {
    tracing::warn!(
        target: "signalbox_tools_github",
        failure = ?failure.class(),
        session_id = %correlation.session().as_uuid(),
        turn_id = %correlation.turn().as_uuid(),
        "GitHub transport failed"
    );
}

fn caller_bug() -> GitHubExecutorError {
    GitHubExecutorError {
        class: OperatorFailureClass::CallerOrHubBug,
    }
}

fn result_kind_mismatch(kind: ToolKind) -> GitHubExecutorError {
    if kind.mutates() {
        infrastructure(kind.commit_outcome())
    } else {
        caller_bug()
    }
}

const fn status_is_definitive(status: u16) -> bool {
    status < 500
}

fn classify_error_body_failure(
    status: StatusCode,
    failure: GitHubTransportFailure,
) -> GitHubTransportFailure {
    if status_is_definitive(status.as_u16()) {
        GitHubTransportFailure::Rejected {
            status: status.as_u16(),
            detail: None,
        }
    } else {
        failure
    }
}

#[derive(Clone, Copy)]
enum CommitOutcome {
    Definite,
    Ambiguous,
}

impl ToolKind {
    const fn commit_outcome(self) -> CommitOutcome {
        if self.mutates() {
            CommitOutcome::Ambiguous
        } else {
            CommitOutcome::Definite
        }
    }
}

const fn infrastructure(outcome: CommitOutcome) -> GitHubExecutorError {
    GitHubExecutorError {
        class: OperatorFailureClass::Infrastructure {
            commit_ambiguous: match outcome {
                CommitOutcome::Definite => false,
                CommitOutcome::Ambiguous => true,
            },
        },
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
        .filter(|length| secret.is_char_boundary(*length))
        .find(|length| text.ends_with(&secret[..*length]))
        .unwrap_or(0)
}

#[derive(Clone, Copy)]
enum ResponseExtent {
    Complete,
    Truncated,
}

fn sanitize_error_body(
    bytes: &[u8],
    source: ResponseExtent,
    scrubber: &CredentialScrubber,
) -> Option<SanitizedGitHubError> {
    let bytes = match source {
        ResponseExtent::Complete => bytes,
        ResponseExtent::Truncated => discard_incomplete_utf8_suffix(bytes),
    };
    let redacted = scrubber.redact_text(String::from_utf8_lossy(bytes).into_owned());
    let redacted = match source {
        ResponseExtent::Complete => redacted,
        ResponseExtent::Truncated => scrubber.redact_trailing_prefix(redacted),
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

fn discard_incomplete_utf8_suffix(bytes: &[u8]) -> &[u8] {
    let mut continuation_count = 0;
    for byte in bytes.iter().rev().take(3) {
        if byte & 0b1100_0000 == 0b1000_0000 {
            continuation_count += 1;
        } else {
            break;
        }
    }
    let lead_index = bytes.len().saturating_sub(continuation_count + 1);
    let Some(&lead) = bytes.get(lead_index) else {
        return bytes;
    };
    let expected_width = match lead {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return bytes,
    };
    let available_width = continuation_count + 1;
    if expected_width > available_width {
        &bytes[..lead_index]
    } else {
        bytes
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

    async fn create_pull_request(
        &self,
        repository: GitHubRepository,
        arguments: CreatePullRequestArguments,
        credential: &CredentialValue,
        policy: &GitHubEgressPolicy,
    ) -> Result<GitHubResult, GitHubTransportFailure> {
        let url = self.repository_url(&repository, &["pulls"], None)?;
        let body = create_pull_request_body(&arguments)?;
        let response = self
            .send(Method::POST, url, Some(body), credential, policy)
            .await?;
        let value = self
            .success_json(response, StatusCode::CREATED, credential)
            .await
            .map_err(mutation_failure)?;
        normalize_created_pull_request(&value, &arguments)
            .map(GitHubResult::created_pull_request)
            .map_err(mutation_failure)
    }

    async fn pull_request_value(
        &self,
        arguments: &PullRequestArguments,
        credential: &CredentialValue,
        policy: &GitHubEgressPolicy,
        timeout: Duration,
    ) -> Result<serde_json::Value, GitHubTransportFailure> {
        let number = arguments.number().get().to_string();
        let url = self.repository_url(arguments.repository(), &["pulls", &number], None)?;
        let response = self
            .send_with_timeout(Method::GET, url, None, credential, policy, timeout)
            .await?;
        let value = self
            .success_json(response, StatusCode::OK, credential)
            .await?;
        Ok(value)
    }

    async fn metadata_value(
        &self,
        arguments: &PullRequestArguments,
        credential: &CredentialValue,
        policy: &GitHubEgressPolicy,
    ) -> Result<serde_json::Value, GitHubTransportFailure> {
        let value = self
            .pull_request_value(arguments, credential, policy, self.timeout)
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
        let deadline = Instant::now()
            .checked_add(self.timeout)
            .ok_or(GitHubTransportFailure::DispatchUnknown)?;
        tokio::time::timeout(
            self.timeout,
            self.diff_before_deadline(arguments, credential, policy, deadline),
        )
        .await
        .map_err(|_| GitHubTransportFailure::DispatchUnknown)?
    }

    async fn diff_before_deadline(
        &self,
        arguments: PullRequestArguments,
        credential: &CredentialValue,
        policy: &GitHubEgressPolicy,
        deadline: Instant,
    ) -> Result<GitHubResult, GitHubTransportFailure> {
        let initial_value = self
            .pull_request_value(&arguments, credential, policy, remaining_timeout(deadline)?)
            .await?;
        let initial = normalize_diff_snapshot(&initial_value, arguments.number())?;
        let mut files = Vec::new();
        let mut pagination_extent = FilePaginationExtent::Complete;
        let mut patch_extent = FilePatchExtent::Complete;
        for page in 1..=MAX_FILE_PAGES {
            let page_text = page.to_string();
            let page_size_text = PAGE_SIZE.to_string();
            let number = arguments.number().get().to_string();
            let url = self.repository_url(
                arguments.repository(),
                &["pulls", &number, "files"],
                Some(&[("per_page", &page_size_text), ("page", &page_text)]),
            )?;
            let response = self
                .send_with_timeout(
                    Method::GET,
                    url,
                    None,
                    credential,
                    policy,
                    remaining_timeout(deadline)?,
                )
                .await?;
            let has_next = response_has_next_page(&response);
            let value = self
                .success_json(response, StatusCode::OK, credential)
                .await?;
            let normalized = normalize_files(&value)?;
            files.extend(normalized.files);
            patch_extent.absorb(normalized.patch_extent);
            if !has_next {
                break;
            }
            if page == MAX_FILE_PAGES {
                pagination_extent = FilePaginationExtent::Truncated;
            }
        }
        validate_unique_file_paths(&files)?;
        let truncated = files_incomplete(files.len(), initial.changed_files, pagination_extent)?
            || matches!(patch_extent, FilePatchExtent::Truncated);
        let current_value = self
            .pull_request_value(&arguments, credential, policy, remaining_timeout(deadline)?)
            .await?;
        let current = normalize_diff_snapshot(&current_value, arguments.number())?;
        if !diff_snapshot_unchanged(&initial, &current) {
            return Err(GitHubTransportFailure::RevisionChanged);
        }
        Ok(GitHubResult::diff(serde_json::json!({
            "base_revision": initial.base_revision,
            "head_revision": initial.head_revision,
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
        validate_graphql_errors(&value)?;
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
        let body = publish_review_body(&arguments)?;
        let response = self
            .send(Method::POST, url, Some(body), credential, policy)
            .await?;
        let value = self
            .success_json(response, StatusCode::OK, credential)
            .await
            .map_err(mutation_failure)?;
        normalize_published_review(&value, arguments.commit_id().as_str(), arguments.event())
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
        self.send_with_timeout(method, url, body, credential, policy, self.timeout)
            .await
    }

    async fn send_with_timeout(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        credential: &CredentialValue,
        policy: &GitHubEgressPolicy,
        timeout: Duration,
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
        let client = public_destination_client(&url, Some(timeout))
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
            .map_err(|error| classify_send_failure(error.is_connect()))?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let status_code = status.as_u16();
        let (body, extent) = match read_bounded(response, MAX_ERROR_SOURCE_BYTES).await {
            Ok(body) => body,
            Err(failure) => return Err(classify_error_body_failure(status, failure)),
        };
        let detail = sanitize_error_body(&body, extent, &scrubber);
        Err(GitHubTransportFailure::Rejected {
            status: status_code,
            detail,
        })
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
        let (body, extent) = read_bounded(response, MAX_RESPONSE_BYTES).await?;
        if matches!(extent, ResponseExtent::Truncated) {
            return Err(GitHubTransportFailure::ResponseTooLarge);
        }
        let scrubber = CredentialScrubber::try_new(credential)
            .ok_or(GitHubTransportFailure::InvalidCredential)?;
        let mut value = serde_json::from_slice::<serde_json::Value>(&body).map_err(|_| {
            invalid_response(sanitize_error_body(
                &body,
                ResponseExtent::Complete,
                &scrubber,
            ))
        })?;
        scrubber.redact_value(&mut value);
        Ok(value)
    }
}

fn publish_review_body(
    arguments: &PublishReviewArguments,
) -> Result<Vec<u8>, GitHubTransportFailure> {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "commit_id".to_owned(),
        serde_json::Value::String(arguments.commit_id().as_str().to_owned()),
    );
    payload.insert(
        "event".to_owned(),
        serde_json::Value::String(arguments.event().api_value().to_owned()),
    );
    if let Some(body) = arguments.body() {
        payload.insert(
            "body".to_owned(),
            serde_json::Value::String(body.to_owned()),
        );
    }
    if !arguments.comments().is_empty() {
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
            .collect();
        payload.insert("comments".to_owned(), serde_json::Value::Array(comments));
    }
    serde_json::to_vec(&serde_json::Value::Object(payload)).map_err(|_| invalid_response(None))
}

fn create_pull_request_body(
    arguments: &CreatePullRequestArguments,
) -> Result<Vec<u8>, GitHubTransportFailure> {
    serde_json::to_vec(&serde_json::json!({
        "title": arguments.title(),
        "body": arguments.body(),
        "head": arguments.head(),
        "base": arguments.base(),
    }))
    .map_err(|_| invalid_response(None))
}

impl GitHubTransport for GitHubApiTransport {
    async fn execute(
        &mut self,
        operation: GitHubOperation,
        credential: &CredentialValue,
        policy: &GitHubEgressPolicy,
    ) -> Result<GitHubResult, GitHubTransportFailure> {
        match operation {
            GitHubOperation::CreatePullRequest {
                repository,
                arguments,
            } => {
                self.create_pull_request(repository, arguments, credential, policy)
                    .await
            }
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
        | GitHubTransportFailure::GraphQlRejected
        | GitHubTransportFailure::EgressRejected => failure,
        GitHubTransportFailure::InvalidResponse { .. }
        | GitHubTransportFailure::ResponseTooLarge
        | GitHubTransportFailure::RevisionChanged
        | GitHubTransportFailure::DispatchUnknown => GitHubTransportFailure::DispatchUnknown,
        GitHubTransportFailure::PreDispatchInfrastructure => {
            GitHubTransportFailure::PreDispatchInfrastructure
        }
    }
}

const fn classify_send_failure(is_connect: bool) -> GitHubTransportFailure {
    if is_connect {
        GitHubTransportFailure::PreDispatchInfrastructure
    } else {
        GitHubTransportFailure::DispatchUnknown
    }
}

const fn classify_destination_failure(
    failure: PublicDestinationClientError,
) -> GitHubTransportFailure {
    match failure {
        PublicDestinationClientError::DestinationRejected => {
            GitHubTransportFailure::PreDispatchInfrastructure
        }
        PublicDestinationClientError::Infrastructure => {
            GitHubTransportFailure::PreDispatchInfrastructure
        }
    }
}

async fn read_bounded(
    response: Response,
    limit: usize,
) -> Result<(Vec<u8>, ResponseExtent), GitHubTransportFailure> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| GitHubTransportFailure::DispatchUnknown)?;
        let remaining = limit.saturating_sub(body.len());
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            return Ok((body, ResponseExtent::Truncated));
        }
        body.extend_from_slice(&chunk);
        if body.len() == limit {
            let has_more = has_more_response_bytes(&mut stream)
                .await
                .map_err(classify_more_bytes_failure)?;
            let extent = match has_more {
                true => ResponseExtent::Truncated,
                false => ResponseExtent::Complete,
            };
            return Ok((body, extent));
        }
    }
    Ok((body, ResponseExtent::Complete))
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

fn remaining_timeout(deadline: Instant) -> Result<Duration, GitHubTransportFailure> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(GitHubTransportFailure::DispatchUnknown)
}

#[derive(Clone, Copy)]
enum FilePaginationExtent {
    Complete,
    Truncated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FilePatchExtent {
    Complete,
    Truncated,
}

impl FilePatchExtent {
    fn absorb(&mut self, other: Self) {
        if matches!(other, Self::Truncated) {
            *self = Self::Truncated;
        }
    }
}

struct NormalizedFiles {
    files: Vec<serde_json::Value>,
    patch_extent: FilePatchExtent,
}

fn normalize_patch(
    patch: Option<String>,
) -> Result<(Option<String>, FilePatchExtent), GitHubTransportFailure> {
    match patch {
        None => Ok((None, FilePatchExtent::Truncated)),
        Some(patch) if patch.contains('\0') => Err(invalid_response(None)),
        Some(patch) if patch.len() > MAX_TEXT_BYTES => Ok((None, FilePatchExtent::Truncated)),
        Some(patch) => Ok((Some(patch), FilePatchExtent::Complete)),
    }
}

fn validate_unique_file_paths(files: &[serde_json::Value]) -> Result<(), GitHubTransportFailure> {
    let mut paths = HashSet::new();
    for file in files {
        let path = required_string(required_object(file)?, "path")?;
        if !paths.insert(path) {
            return Err(invalid_response(None));
        }
    }
    Ok(())
}

fn files_incomplete(
    received: usize,
    expected: usize,
    extent: FilePaginationExtent,
) -> Result<bool, GitHubTransportFailure> {
    let pagination_incomplete = match extent {
        FilePaginationExtent::Complete => false,
        FilePaginationExtent::Truncated => true,
    };
    (received <= expected)
        .then_some(pagination_incomplete || received < expected)
        .ok_or_else(|| invalid_response(None))
}

struct DiffSnapshot {
    base_revision: String,
    head_revision: String,
    changed_files: usize,
}

fn diff_snapshot_unchanged(initial: &DiffSnapshot, current: &DiffSnapshot) -> bool {
    initial.base_revision == current.base_revision
        && initial.head_revision == current.head_revision
        && initial.changed_files == current.changed_files
}

fn normalize_diff_snapshot(
    value: &serde_json::Value,
    expected_number: PullRequestNumber,
) -> Result<DiffSnapshot, GitHubTransportFailure> {
    let object = required_object(value)?;
    if required_u64(object, "number")? != u64::from(expected_number.get()) {
        return Err(invalid_response(None));
    }
    let base = required_object(required(object, "base")?)?;
    let head = required_object(required(object, "head")?)?;
    let changed_files = usize::try_from(required_u64(object, "changed_files")?)
        .map_err(|_| invalid_response(None))?;
    Ok(DiffSnapshot {
        base_revision: checked_revision(required_string(base, "sha")?)?,
        head_revision: checked_revision(required_string(head, "sha")?)?,
        changed_files,
    })
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
    let title = checked_text(required_string(object, "title")?, TextPresence::Required)?;
    let body = required_nullable_string(object, "body")?
        .map(|body| checked_text(body, TextPresence::Optional))
        .transpose()?;
    let state = checked_text(required_string(object, "state")?, TextPresence::Required)?;
    let author = required_nullable_object_string(object, "user", "login")?
        .map(|author| checked_text(author, TextPresence::Required))
        .transpose()?;
    Ok(serde_json::json!({
        "number": expected_number.get(),
        "title": title,
        "body": body,
        "state": state,
        "draft": required_bool(object, "draft")?,
        "author": author,
        "base_ref": checked_text(required_string(base, "ref")?, TextPresence::Required)?,
        "base_revision": checked_revision(required_string(base, "sha")?)?,
        "head_ref": checked_text(required_string(head, "ref")?, TextPresence::Required)?,
        "head_revision": checked_revision(required_string(head, "sha")?)?,
        "url": checked_url(required_string(object, "html_url")?)?,
    }))
}

fn normalize_created_pull_request(
    value: &serde_json::Value,
    arguments: &CreatePullRequestArguments,
) -> Result<serde_json::Value, GitHubTransportFailure> {
    let object = required_object(value)?;
    let number = PullRequestNumber::try_from(required_u64(object, "number")?)
        .map_err(|_| invalid_response(None))?;
    Ok(serde_json::json!({
        "number": number.get(),
        "url": checked_url(required_string(object, "html_url")?)?,
        "head": arguments.head(),
        "base": arguments.base(),
    }))
}

fn normalize_files(value: &serde_json::Value) -> Result<NormalizedFiles, GitHubTransportFailure> {
    let values = required_array(value)?;
    if values.len() > PAGE_SIZE {
        return Err(invalid_response(None));
    }
    let mut files = Vec::new();
    let mut patch_extent = FilePatchExtent::Complete;
    for value in values {
        let object = required_object(value)?;
        let previous = optional_string(object, "previous_filename")?
            .map(checked_path)
            .transpose()?;
        let (patch, current_extent) = normalize_patch(optional_string(object, "patch")?)?;
        patch_extent.absorb(current_extent);
        files.push(serde_json::json!({
            "path": checked_path(required_string(object, "filename")?)?,
            "previous_path": previous,
            "status": checked_text(required_string(object, "status")?, TextPresence::Required)?,
            "additions": required_u64(object, "additions")?,
            "deletions": required_u64(object, "deletions")?,
            "changes": required_u64(object, "changes")?,
            "patch": patch,
        }));
    }
    Ok(NormalizedFiles {
        files,
        patch_extent,
    })
}

fn validate_graphql_errors(value: &serde_json::Value) -> Result<(), GitHubTransportFailure> {
    let object = required_object(value)?;
    let Some(errors) = object.get("errors") else {
        return Ok(());
    };
    let errors = required_array(errors)?;
    if errors.is_empty() {
        return Ok(());
    }
    if errors.iter().all(graphql_error_is_definitive) {
        return Err(GitHubTransportFailure::GraphQlRejected);
    }
    Err(GitHubTransportFailure::DispatchUnknown)
}

fn graphql_error_is_definitive(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let error_type = object.get("type").and_then(serde_json::Value::as_str);
    let error_code = object
        .get("extensions")
        .and_then(serde_json::Value::as_object)
        .and_then(|extensions| extensions.get("code"))
        .and_then(serde_json::Value::as_str);
    error_type
        .into_iter()
        .chain(error_code)
        .all(graphql_error_code_is_definitive)
        && (error_type.is_some() || error_code.is_some())
}

fn graphql_error_code_is_definitive(code: &str) -> bool {
    matches!(code, "NOT_FOUND" | "FORBIDDEN" | "UNPROCESSABLE")
}

fn normalize_threads(
    value: &serde_json::Value,
) -> Result<serde_json::Value, GitHubTransportFailure> {
    let connection = nested(
        value,
        &["data", "repository", "pullRequest", "reviewThreads"],
    )?;
    let threads = bounded_connection_nodes(connection)?
        .iter()
        .map(normalize_thread)
        .collect::<Result<Vec<_>, _>>()?;
    validate_unique_review_identities(&threads)?;
    Ok(serde_json::json!({
        "threads": threads,
        "truncated": nested_bool(connection, &["pageInfo", "hasNextPage"])?,
    }))
}

fn validate_unique_review_identities(
    threads: &[serde_json::Value],
) -> Result<(), GitHubTransportFailure> {
    let mut thread_ids = HashSet::new();
    let mut comment_ids = HashSet::new();
    for thread in threads {
        let thread = required_object(thread)?;
        if !thread_ids.insert(required_string(thread, "id")?) {
            return Err(invalid_response(None));
        }
        for comment in required_array(required(thread, "comments")?)? {
            let comment = required_object(comment)?;
            if !comment_ids.insert(required_string(comment, "id")?) {
                return Err(invalid_response(None));
            }
        }
    }
    Ok(())
}

fn normalize_thread(
    value: &serde_json::Value,
) -> Result<serde_json::Value, GitHubTransportFailure> {
    let object = required_object(value)?;
    let comments_connection = required(object, "comments")?;
    let comments = bounded_connection_nodes(comments_connection)?
        .iter()
        .map(normalize_comment)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::json!({
        "id": checked_opaque_id(required_string(object, "id")?)?,
        "resolved": required_bool(object, "isResolved")?,
        "outdated": required_bool(object, "isOutdated")?,
        "path": checked_path(required_string(object, "path")?)?,
        "line": required_nullable_u64(object, "line")?,
        "comments": comments,
        "comments_truncated": nested_bool(comments_connection, &["pageInfo", "hasNextPage"])?,
    }))
}

fn bounded_connection_nodes(
    connection: &serde_json::Value,
) -> Result<&Vec<serde_json::Value>, GitHubTransportFailure> {
    let object = required_object(connection)?;
    let nodes = required_array(required(object, "nodes")?)?;
    (nodes.len() <= PAGE_SIZE)
        .then_some(nodes)
        .ok_or_else(|| invalid_response(None))
}

fn normalize_comment(
    value: &serde_json::Value,
) -> Result<serde_json::Value, GitHubTransportFailure> {
    let object = required_object(value)?;
    let author = required_nullable_object_string(object, "author", "login")?
        .map(|author| checked_text(author, TextPresence::Required))
        .transpose()?;
    Ok(serde_json::json!({
        "id": checked_opaque_id(required_string(object, "id")?)?,
        "author": author,
        "body": checked_text(required_string(object, "body")?, TextPresence::Optional)?,
        "created_at": checked_text(required_string(object, "createdAt")?, TextPresence::Required)?,
        "url": checked_url(required_string(object, "url")?)?,
    }))
}

fn normalize_published_review(
    value: &serde_json::Value,
    expected_commit: &str,
    expected_event: PublishReviewEvent,
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
    let state = checked_text(required_string(object, "state")?, TextPresence::Required)?;
    if state != expected_event.acknowledged_state() {
        return Err(invalid_response(None));
    }
    Ok(serde_json::json!({
        "id": id,
        "state": state,
        "url": checked_url(required_string(object, "html_url")?)?,
        "commit_id": commit_id,
    }))
}

fn invalid_response(detail: Option<SanitizedGitHubError>) -> GitHubTransportFailure {
    GitHubTransportFailure::InvalidResponse { detail }
}

#[derive(Clone, Copy)]
enum TextPresence {
    Required,
    Optional,
}

fn valid_text(value: &str, presence: TextPresence) -> bool {
    let content_is_valid = matches!(presence, TextPresence::Optional) || !value.is_empty();
    content_is_valid && value.len() <= MAX_TEXT_BYTES && !value.contains('\0')
}

fn checked_text(value: String, presence: TextPresence) -> Result<String, GitHubTransportFailure> {
    valid_text(&value, presence)
        .then_some(value)
        .ok_or_else(|| invalid_response(None))
}

fn checked_opaque_id(value: String) -> Result<String, GitHubTransportFailure> {
    (!value.is_empty()
        && value.len() <= MAX_OPAQUE_ID_BYTES
        && !value.chars().any(char::is_control))
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

fn required_nullable_string(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<Option<String>, GitHubTransportFailure> {
    match required(object, field)? {
        serde_json::Value::Null => Ok(None),
        value => value
            .as_str()
            .map(str::to_owned)
            .map(Some)
            .ok_or_else(|| invalid_response(None)),
    }
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

fn required_nullable_u64(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<Option<u64>, GitHubTransportFailure> {
    match required(object, field)? {
        serde_json::Value::Null => Ok(None),
        value => value
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

fn required_nullable_object_string(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
    nested_field: &str,
) -> Result<Option<String>, GitHubTransportFailure> {
    match required(object, field)? {
        serde_json::Value::Null => Ok(None),
        value => required_object(value)
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
mod test_support;

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        io::{self, Write},
        sync::{Arc, Mutex, OnceLock},
    };

    use signalbox_application::{
        ToolCatalog, ToolExecutionServiceError, ToolExecutionServiceOutcome,
    };
    use signalbox_domain::{
        SessionId, ToolAttemptDispatchCorrelationReconstitutionInput, ToolAttemptEnd,
        ToolAttemptId, ToolDispatchGeneration, ToolExecutionErrorKind, ToolName, ToolRequestId,
        TurnAttemptId, TurnId,
    };
    use signalbox_model_runtime::CredentialAccessFailure;

    use super::{test_support::*, *};

    const BASE_REVISION: &str = "1111111111111111111111111111111111111111";
    const HEAD_REVISION: &str = "2222222222222222222222222222222222222222";
    const PULL_REQUEST_NUMBER: u64 = 348;
    const CHANGED_FILES: usize = 1;
    const CHANGED_FILES_AFTER_SNAPSHOT: usize = CHANGED_FILES + 1;
    const FILE_PATH: &str = "crates/example/src/lib.rs";
    const FILE_PATCH: &str = "@@ -1 +1 @@\n-old\n+new";
    const THREADS_TRUNCATED: bool = false;
    const THREAD_RESOLVED: bool = false;
    const THREAD_OUTDATED: bool = false;
    const COMMENTS_TRUNCATED: bool = false;
    const REVIEW_COMMENT_BODY: &str = "Please cover this edge.";
    const GRAPHQL_ERROR_MESSAGE: &str = "synthetic GraphQL failure";
    const GRAPHQL_NOT_FOUND: &str = "NOT_FOUND";
    const GRAPHQL_FORBIDDEN: &str = "FORBIDDEN";
    const GRAPHQL_RATE_LIMITED: &str = "RATE_LIMITED";
    const ERROR_BODY_PREFIX: &str = "safe ";
    const PUBLISHED_REVIEW_STATE: &str = "APPROVED";
    const MISMATCHED_PUBLISHED_REVIEW_STATE: &str = "COMMENTED";
    const GITHUB_FILE_CEILING: usize = 3_000;
    const FILES_BEYOND_CEILING: usize = 3_001;
    const CLIENT_ERROR_STATUS: u16 = 422;
    const FORBIDDEN_STATUS: u16 = 403;
    const REDIRECT_STATUS: u16 = 302;
    const SERVER_ERROR_STATUS: u16 = 503;
    const BAD_GATEWAY_STATUS: u16 = 502;
    const SYNTHETIC_TOKEN: &str = "github_pat_synthetic_fixture_secret";
    const SYNTHETIC_UNICODE_TOKEN: &str = "github_pat_synthétic_fixture_secret";
    const SYNTHETIC_UNICODE_PREFIX: &str = "github_pat_synth";
    const DIFF_FILES_FOR_RESULT_OVERFLOW: usize = 64;
    const LAST_OVERFLOW_FILE_INDEX: usize = DIFF_FILES_FOR_RESULT_OVERFLOW - 1;
    const COMMENTS_FOR_RESULT_OVERFLOW: usize = 16;
    const THREADS_FOR_RESULT_OVERFLOW: usize = PAGE_SIZE;
    const COMMENT_BODY_FOR_RESERVATION_BYTES: usize = MAX_TEXT_BYTES / 8;
    const ZERO_INLINE_COMMENT_LINE: u32 = MIN_INLINE_COMMENT_LINE - 1;
    const AGGREGATE_PATCH_BYTES: usize = MAX_TEXT_BYTES / 4;
    const ITEMS_BEYOND_PAGE_BOUND: usize = PAGE_SIZE + 1;
    const INLINE_COMMENTS_BEYOND_MAX: usize = MAX_INLINE_COMMENTS + 1;
    const OPAQUE_ID_BYTES_BEYOND_BOUND: usize = MAX_OPAQUE_ID_BYTES + 1;
    const PROVIDER_ERROR_TEXT: &str = "private provider detail";
    const PATCH_FILLER: &str = "x";
    const SHORT_PATCH: &str = "";
    const OPAQUE_ID_FILLER: &str = "N";
    const CONTROLLED_OPAQUE_ID: &str = "PRRC_fixture\n";
    const OTHER_THREAD_ID: &str = "PRRT_fixture_other";
    const CREDENTIAL_FAILURE_CLASSIFICATION: &str = "failure=Unmapped";
    const CREDENTIAL_VALUE_FAILURE_CLASSIFICATION: &str = "failure=Unusable";
    const TRANSPORT_FAILURE_CLASSIFICATION: &str = "failure=InvalidResponse";
    const SESSION_IDENTITY: u128 = 1;
    const TURN_IDENTITY: u128 = 2;
    const ISSUING_ATTEMPT_IDENTITY: u128 = 3;
    const REQUEST_IDENTITY: u128 = 4;
    const ATTEMPT_IDENTITY: u128 = 5;
    const SESSION_ID_DIAGNOSTIC: &str = "session_id=00000000-0000-0000-0000-000000000001";
    const TURN_ID_DIAGNOSTIC: &str = "turn_id=00000000-0000-0000-0000-000000000002";
    const CONFIGURED_REPOSITORY: &str = "KeenWill/signalbox";
    const CREATED_PULL_REQUEST_NUMBER: u64 = 379;
    const CREATED_PULL_REQUEST_URL: &str = "https://github.com/KeenWill/signalbox/pull/379";
    const CREATE_TITLE: &str = "Repair the failing invariant";
    const CREATE_BODY: &str = "Synthetic repair body";
    const CREATE_HEAD: &str = "agent/fix-invariant";
    const CREATE_BASE: &str = "main";

    struct SyntheticCredentials;
    struct SyntheticTransport;

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct RecordedCreateRequest {
        repository: String,
        title: String,
        body: String,
        head: String,
        base: String,
        credential: Vec<u8>,
        origin: String,
    }

    #[derive(Clone, Debug, Default)]
    struct RecordingCreateTransport(Arc<Mutex<Option<RecordedCreateRequest>>>);

    impl RecordingCreateTransport {
        fn recorded(&self) -> RecordedCreateRequest {
            self.0
                .lock()
                .expect("recording transport lock is available")
                .clone()
                .expect("creation request was recorded")
        }
    }

    impl GitHubTransport for RecordingCreateTransport {
        async fn execute(
            &mut self,
            operation: GitHubOperation,
            credential: &CredentialValue,
            policy: &GitHubEgressPolicy,
        ) -> Result<GitHubResult, GitHubTransportFailure> {
            let GitHubOperation::CreatePullRequest {
                repository,
                arguments,
            } = operation
            else {
                return Err(GitHubTransportFailure::PreDispatchInfrastructure);
            };
            *self
                .0
                .lock()
                .expect("recording transport lock is available") = Some(RecordedCreateRequest {
                repository: repository.as_str().to_owned(),
                title: arguments.title().to_owned(),
                body: arguments.body().to_owned(),
                head: arguments.head().to_owned(),
                base: arguments.base().to_owned(),
                credential: credential.expose_bytes().to_vec(),
                origin: policy.admitted_origin().to_owned(),
            });
            Ok(GitHubResult::created_pull_request(serde_json::json!({
                "number": CREATED_PULL_REQUEST_NUMBER,
                "url": CREATED_PULL_REQUEST_URL,
                "head": CREATE_HEAD,
                "base": CREATE_BASE,
            })))
        }
    }

    thread_local! {
        /// Telemetry captured on this thread alone.
        static CAPTURED_TELEMETRY: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }

    /// Appends every formatted event to the emitting thread's own buffer.
    #[derive(Clone, Copy, Default)]
    struct CapturedTelemetry;

    impl Write for CapturedTelemetry {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            CAPTURED_TELEMETRY.with(|captured| captured.borrow_mut().extend_from_slice(buffer));
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for CapturedTelemetry {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self::Writer {
            *self
        }
    }

    /// Installs the capturing subscriber once for the whole test process, and
    /// clears whatever this thread captured earlier.
    ///
    /// It must be global rather than thread-scoped. `tracing` caches each
    /// callsite's interest process-wide, but `set_default` binds a subscriber
    /// to one thread, so a sibling test that reaches a callsite first on
    /// another thread registers it against no subscriber at all -- recording it
    /// as uninteresting for every thread, including the one that installed a
    /// capture. The event then is not merely written late; it is never emitted,
    /// and the assertion reads an empty buffer.
    ///
    /// Writes are routed per thread so concurrent tests never read each other's
    /// events, which keeps assertions on both presence and absence honest.
    fn capture_telemetry_for_this_thread() {
        static INSTALLED: OnceLock<()> = OnceLock::new();

        INSTALLED.get_or_init(|| {
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(CapturedTelemetry)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("no other global telemetry subscriber is installed");
        });
        CAPTURED_TELEMETRY.with(|captured| captured.borrow_mut().clear());
    }

    /// Returns the telemetry captured on this thread.
    fn captured_telemetry() -> String {
        CAPTURED_TELEMETRY
            .with(|captured| String::from_utf8(captured.borrow().clone()))
            .expect("captured telemetry is UTF-8")
    }

    fn dispatch_correlation() -> ToolAttemptDispatchCorrelation {
        ToolAttemptDispatchCorrelation::reconstitute(
            ToolAttemptDispatchCorrelationReconstitutionInput {
                session: SessionId::from_uuid(uuid::Uuid::from_u128(SESSION_IDENTITY)),
                turn: TurnId::from_uuid(uuid::Uuid::from_u128(TURN_IDENTITY)),
                issuing_attempt: TurnAttemptId::from_uuid(uuid::Uuid::from_u128(
                    ISSUING_ATTEMPT_IDENTITY,
                )),
                request: ToolRequestId::from_uuid(uuid::Uuid::from_u128(REQUEST_IDENTITY)),
                attempt: ToolAttemptId::from_uuid(uuid::Uuid::from_u128(ATTEMPT_IDENTITY)),
                generation: ToolDispatchGeneration::first(),
            },
        )
    }

    fn capture_credential_failure(
        error: &CredentialAccessError,
        correlation: &ToolAttemptDispatchCorrelation,
    ) -> String {
        capture_telemetry_for_this_thread();
        report_credential_access_failure(error, correlation);
        captured_telemetry()
    }

    fn capture_credential_value_failure(correlation: &ToolAttemptDispatchCorrelation) -> String {
        capture_telemetry_for_this_thread();
        report_credential_value_failure(correlation);
        captured_telemetry()
    }

    fn capture_transport_failure(
        failure: &GitHubTransportFailure,
        correlation: &ToolAttemptDispatchCorrelation,
    ) -> String {
        capture_telemetry_for_this_thread();
        report_transport_failure(failure, correlation);
        captured_telemetry()
    }

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

    fn publish_arguments(value: serde_json::Value) -> PublishReviewArguments {
        serde_json::from_value(value).expect("publish fixture is admitted")
    }

    fn definition(catalog: &CompiledToolCatalog, name: &str) -> ToolDefinition {
        catalog
            .definition(&ToolName::try_new(name.to_owned()).expect("fixture name is admitted"))
            .expect("fixture declaration exists")
    }

    fn metadata_response() -> serde_json::Value {
        serde_json::json!({
            "number": PULL_REQUEST_NUMBER,
            "changed_files": CHANGED_FILES,
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
            "filename": FILE_PATH,
            "status": "modified",
            "additions": 4,
            "deletions": 2,
            "changes": 6,
            "patch": FILE_PATCH
        }])
    }

    fn threads_response() -> serde_json::Value {
        serde_json::json!({
            "data": {"repository": {"pullRequest": {"reviewThreads": {
                "nodes": [{
                    "id": "PRRT_fixture",
                    "isResolved": THREAD_RESOLVED,
                    "isOutdated": THREAD_OUTDATED,
                    "path": FILE_PATH,
                    "line": 42,
                    "comments": {
                        "nodes": [{
                            "id": "PRRC_fixture",
                            "author": {"login": "reviewer"},
                            "body": REVIEW_COMMENT_BODY,
                            "createdAt": "2026-07-31T00:00:00Z",
                            "url": "https://github.com/KeenWill/signalbox/pull/1#discussion_r1"
                        }],
                        "pageInfo": {"hasNextPage": COMMENTS_TRUNCATED}
                    }
                }],
                "pageInfo": {"hasNextPage": THREADS_TRUNCATED}
            }}}}
        })
    }

    #[test]
    fn credential_failure_diagnostic_preserves_safe_classification() {
        let error = CredentialAccessError::new(
            CredentialReference::new(GITHUB_CREDENTIAL_REFERENCE),
            CredentialAccessFailure::Unmapped,
        );

        let correlation = dispatch_correlation();
        let diagnostic = capture_credential_failure(&error, &correlation);
        assert!(diagnostic.contains(SESSION_ID_DIAGNOSTIC));
        assert!(diagnostic.contains(TURN_ID_DIAGNOSTIC));

        assert!(diagnostic.contains(CREDENTIAL_FAILURE_CLASSIFICATION));
        assert!(!diagnostic.contains(SYNTHETIC_TOKEN));
    }

    #[test]
    fn unusable_credential_value_diagnostic_preserves_safe_classification() {
        let correlation = dispatch_correlation();

        let diagnostic = capture_credential_value_failure(&correlation);

        assert!(diagnostic.contains(SESSION_ID_DIAGNOSTIC));
        assert!(diagnostic.contains(TURN_ID_DIAGNOSTIC));
        assert!(diagnostic.contains(CREDENTIAL_VALUE_FAILURE_CLASSIFICATION));
        assert!(!diagnostic.contains(SYNTHETIC_TOKEN));
    }

    #[test]
    fn transport_failure_diagnostic_preserves_safe_classification() {
        let correlation = dispatch_correlation();
        let failure = GitHubTransportFailure::InvalidResponse {
            detail: Some(SanitizedGitHubError(PROVIDER_ERROR_TEXT.to_owned())),
        };

        let diagnostic = capture_transport_failure(&failure, &correlation);

        assert!(diagnostic.contains(SESSION_ID_DIAGNOSTIC));
        assert!(diagnostic.contains(TURN_ID_DIAGNOSTIC));
        assert!(diagnostic.contains(TRANSPORT_FAILURE_CLASSIFICATION));
        assert!(!diagnostic.contains(PROVIDER_ERROR_TEXT));
        assert!(!diagnostic.contains(SYNTHETIC_TOKEN));
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
    fn create_contract_requires_confirmation() {
        let repository = GitHubRepository::try_from(CONFIGURED_REPOSITORY.to_owned())
            .expect("configured repository is admitted");
        let catalog = GitHubPullRequestCreateTools::try_new(
            SyntheticCredentials,
            RecordingCreateTransport::default(),
            GitHubEgressPolicy::github_api_only(),
            repository,
        )
        .expect("creation suite constructs")
        .into_parts()
        .0;
        let create = definition(&catalog, PULL_REQUEST_CREATE_NAME);

        assert_eq!(create.permission_default(), ToolPermissionDefault::Confirm);
        assert_eq!(create.effect_class(), ToolEffectClass::ExternalEffect);
    }

    #[test]
    fn create_contract_has_no_repository_argument() {
        let repository = GitHubRepository::try_from(CONFIGURED_REPOSITORY.to_owned())
            .expect("configured repository is admitted");
        let catalog = GitHubPullRequestCreateTools::try_new(
            SyntheticCredentials,
            RecordingCreateTransport::default(),
            GitHubEgressPolicy::github_api_only(),
            repository,
        )
        .expect("creation suite constructs")
        .into_parts()
        .0;
        let create = definition(&catalog, PULL_REQUEST_CREATE_NAME);
        let schema: serde_json::Value =
            serde_json::from_str(create.input_schema().as_str()).expect("schema is valid JSON");
        let injected_repository = normalized(serde_json::json!({
            "title": CREATE_TITLE,
            "body": CREATE_BODY,
            "head": CREATE_HEAD,
            "base": CREATE_BASE,
            "repository": "attacker/exfiltration"
        }));

        assert_eq!(
            schema["additionalProperties"],
            serde_json::Value::Bool(false)
        );
        assert!(schema["properties"].get("repository").is_none());
        assert!(decode_create_pull_request(&injected_repository).is_err());
    }

    #[tokio::test]
    async fn create_transport_records_exact_configured_request() {
        let repository = GitHubRepository::try_from(CONFIGURED_REPOSITORY.to_owned())
            .expect("configured repository is admitted");
        let arguments = decode_create_pull_request(&normalized(serde_json::json!({
            "title": CREATE_TITLE,
            "body": CREATE_BODY,
            "head": CREATE_HEAD,
            "base": CREATE_BASE
        })))
        .expect("creation arguments are admitted");
        let operation = GitHubOperation::CreatePullRequest {
            repository,
            arguments,
        };
        let credential = CredentialValue::new(SYNTHETIC_TOKEN.as_bytes().to_vec());
        let policy = GitHubEgressPolicy::github_api_only();
        let mut transport = RecordingCreateTransport::default();
        let observer = transport.clone();

        let result = transport
            .execute(operation, &credential, &policy)
            .await
            .expect("synthetic creation succeeds");
        let recorded = observer.recorded();

        assert_eq!(result.kind(), GitHubResultKind::CreatedPullRequest);
        assert_eq!(recorded.repository, CONFIGURED_REPOSITORY);
        assert_eq!(recorded.title, CREATE_TITLE);
        assert_eq!(recorded.body, CREATE_BODY);
        assert_eq!(recorded.head, CREATE_HEAD);
        assert_eq!(recorded.base, CREATE_BASE);
        assert_eq!(recorded.credential, SYNTHETIC_TOKEN.as_bytes());
        assert_eq!(recorded.origin, GITHUB_API_ORIGIN);
    }

    #[test]
    fn create_body_is_exact() {
        let arguments = decode_create_pull_request(&normalized(serde_json::json!({
            "title": CREATE_TITLE,
            "body": CREATE_BODY,
            "head": CREATE_HEAD,
            "base": CREATE_BASE
        })))
        .expect("creation arguments are admitted");
        let body = create_pull_request_body(&arguments).expect("creation body serializes");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("creation body is JSON");

        assert_eq!(body["title"], arguments.title());
        assert_eq!(body["body"], arguments.body());
        assert_eq!(body["head"], arguments.head());
        assert_eq!(body["base"], arguments.base());
    }

    fn create_executor()
    -> GitHubPullRequestCreateExecutor<SyntheticCredentials, RecordingCreateTransport> {
        let repository = GitHubRepository::try_from(CONFIGURED_REPOSITORY.to_owned())
            .expect("configured repository is admitted");
        GitHubPullRequestCreateTools::try_new(
            SyntheticCredentials,
            RecordingCreateTransport::default(),
            GitHubEgressPolicy::github_api_only(),
            repository,
        )
        .expect("creation suite constructs")
        .into_parts()
        .1
    }

    /// a rejection GitHub answered definitively closes creation as a
    /// known failure, so the workflow reports the denial instead of stalling
    /// in reconciliation for an effect that never happened.
    #[test]
    fn definitive_create_rejection_is_a_known_failure() {
        let executor = create_executor();
        let expected = make_detail(REQUEST_REJECTED_DETAIL).expect("fixed detail is valid");

        assert_eq!(
            executor.failure_detail(&GitHubTransportFailure::rejected(FORBIDDEN_STATUS)),
            Ok(expected)
        );
    }

    /// a server-side rejection cannot establish whether the pull
    /// request was created, so it surfaces as an ambiguous commit rather than
    /// a definite failure the agent would retry into a duplicate pull request.
    #[test]
    fn server_side_create_rejection_is_an_ambiguous_commit() {
        let executor = create_executor();
        let error = executor
            .failure_detail(&GitHubTransportFailure::rejected(BAD_GATEWAY_STATUS))
            .expect_err("a server-side rejection cannot establish the commit outcome");

        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true
            }
        );
    }

    /// Answers a creation dispatch with one fixed transport rejection, and
    /// records that it was reached.
    ///
    /// Counting dispatches is what makes the tests below about the *server's*
    /// rejection: an executor that failed before dispatch — on credentials,
    /// egress policy, or argument decoding — can produce the same `KnownFailed`
    /// evidence and the same commit-ambiguous error, so without this the
    /// assertions would hold for a path that never left the process.
    #[derive(Clone, Debug)]
    struct RejectingCreateTransport {
        status: u16,
        dispatches: Arc<Mutex<usize>>,
    }

    impl RejectingCreateTransport {
        fn new(status: u16) -> (Self, Arc<Mutex<usize>>) {
            let dispatches = Arc::new(Mutex::new(0));
            (
                Self {
                    status,
                    dispatches: Arc::clone(&dispatches),
                },
                dispatches,
            )
        }
    }

    impl GitHubTransport for RejectingCreateTransport {
        async fn execute(
            &mut self,
            _operation: GitHubOperation,
            _credential: &CredentialValue,
            _policy: &GitHubEgressPolicy,
        ) -> Result<GitHubResult, GitHubTransportFailure> {
            *self
                .dispatches
                .lock()
                .expect("dispatch counter lock is available") += 1;
            Err(GitHubTransportFailure::rejected(self.status))
        }
    }

    /// a definitively rejected creation reaches the workflow as
    /// `KnownFailed` *evidence* carrying the sanitized rejection detail.
    ///
    /// The classifier tests above call `failure_detail` directly, so they stay
    /// green if `failure_evidence` stops wrapping the detail in `KnownFailed`
    /// and emits completion or another terminal shape instead — a regression
    /// that would report a denial as something else entirely. This drives the
    /// executor through a real correlated invocation so the bound evidence
    /// variant is what is asserted.
    #[tokio::test]
    async fn definitive_create_rejection_binds_known_failure_evidence() {
        let (transport, dispatches) = RejectingCreateTransport::new(FORBIDDEN_STATUS);
        let outcome = create_pull_request_evidence(transport).await;
        let expected = make_detail(REQUEST_REJECTED_DETAIL).expect("fixed detail is valid");

        assert_eq!(
            *dispatches
                .lock()
                .expect("dispatch counter lock is available"),
            1,
            "the evidence must come from exactly one real server rejection"
        );

        assert_eq!(
            outcome.evidence,
            Some(ToolExecutorEvidence::KnownFailed {
                detail: Some(expected.clone())
            })
        );
        // Committing is not the claim on its own: evidence mapped to the wrong
        // terminal observation still commits, leaving the durable attempt
        // marked completed or ambiguous while both the recorder and the
        // outcome shape look right. Assert the relation this path claims — the
        // same definitive detail, ending the attempt known-failed.
        let Ok(ToolExecutionServiceOutcome::ObservationCommitted(ended)) = outcome.result else {
            panic!(
                "definitive evidence commits an observation rather than stalling the batch, \
                 got {:?}",
                outcome.result
            )
        };
        let ToolAttemptEnd::KnownFailed { error } = ended.end() else {
            panic!(
                "a definitive rejection ends the attempt known-failed, got {:?}",
                ended.end()
            )
        };
        assert_eq!(error.kind(), ToolExecutionErrorKind::ExecutionFailed);
        assert_eq!(error.detail(), Some(&expected));
    }

    /// the same path refuses to bind *any* evidence when GitHub
    /// answered ambiguously, so an effect that may have happened is never
    /// reported as a definitive outcome the agent would retry.
    #[tokio::test]
    async fn server_side_create_rejection_binds_no_evidence() {
        let (transport, dispatches) = RejectingCreateTransport::new(BAD_GATEWAY_STATUS);
        let outcome = create_pull_request_evidence(transport).await;

        assert_eq!(
            *dispatches
                .lock()
                .expect("dispatch counter lock is available"),
            1,
            "the ambiguity must come from exactly one real server rejection"
        );
        assert_eq!(outcome.evidence, None);
        // Absence of evidence is not the claim. "No evidence, no commit" is
        // equally true when `failure_evidence` returns a *definite* executor
        // error, and when dispatch fails before that method is reached at all
        // — in either case the server rejection stopped being classified as
        // commit-ambiguous while these assertions stayed green. The
        // classification lives on the executor error the service nests inside
        // its own, so inspect that rather than the shape around it.
        let Err(ToolExecutionServiceError::ExecutorCrashClassification { executor_error, .. }) =
            outcome.result
        else {
            panic!(
                "an unresolvable ambiguous effect must reach crash classification, got {:?}",
                outcome.result
            )
        };
        assert_eq!(
            executor_error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true
            },
            "a server-side rejection of a mutating call stays commit-ambiguous"
        );
    }

    #[test]
    fn create_rejects_a_whitespace_bearing_base_selector() {
        let rejection = decode_create_pull_request(&normalized(serde_json::json!({
            "title": CREATE_TITLE,
            "body": CREATE_BODY,
            "head": CREATE_HEAD,
            "base": "release branch"
        })))
        .expect_err("a whitespace-bearing base selector is rejected before dispatch");

        assert_eq!(rejection, InvalidGitHubArguments);
    }

    #[test]
    fn create_rejects_a_lock_suffixed_interior_ref_component() {
        let rejection = decode_create_pull_request(&normalized(serde_json::json!({
            "title": CREATE_TITLE,
            "body": CREATE_BODY,
            "head": CREATE_HEAD,
            "base": "release.lock/fix"
        })))
        .expect_err("a lock-suffixed ref component is rejected before dispatch");

        assert_eq!(rejection, InvalidGitHubArguments);
    }

    #[test]
    fn create_rejects_an_account_qualified_base_selector() {
        let rejection = decode_create_pull_request(&normalized(serde_json::json!({
            "title": CREATE_TITLE,
            "body": CREATE_BODY,
            "head": CREATE_HEAD,
            "base": "contributor:main"
        })))
        .expect_err("an account-qualified base selector is rejected before dispatch");

        assert_eq!(rejection, InvalidGitHubArguments);
    }

    #[test]
    fn create_admits_an_account_qualified_head_selector() {
        let arguments = decode_create_pull_request(&normalized(serde_json::json!({
            "title": CREATE_TITLE,
            "body": CREATE_BODY,
            "head": "contributor:agent/fix-invariant",
            "base": CREATE_BASE
        })))
        .expect("a well-formed cross-repository head selector is admitted");

        assert_eq!(arguments.head(), "contributor:agent/fix-invariant");
    }

    #[test]
    fn create_rejects_an_account_qualified_head_over_the_total_byte_bound() {
        let account = "a".repeat(MAX_GITHUB_ACCOUNT_BYTES);
        let reference = "b".repeat(MAX_GIT_REF_BYTES);
        let head = format!("{account}:{reference}");
        let rejection = decode_create_pull_request(&normalized(serde_json::json!({
            "title": CREATE_TITLE,
            "body": CREATE_BODY,
            "head": head,
            "base": CREATE_BASE
        })))
        .expect_err("an account-qualified head over the total byte bound is rejected");

        assert_eq!(rejection, InvalidGitHubArguments);
    }

    #[test]
    fn create_rejects_an_account_qualified_head_with_adjacent_hyphens() {
        let rejection = decode_create_pull_request(&normalized(serde_json::json!({
            "title": CREATE_TITLE,
            "body": CREATE_BODY,
            "head": "contributor--fork:agent/fix",
            "base": CREATE_BASE
        })))
        .expect_err("an account name with adjacent hyphens is rejected before dispatch");

        assert_eq!(rejection, InvalidGitHubArguments);
    }

    #[test]
    fn create_rejects_a_doubly_qualified_head_selector() {
        let rejection = decode_create_pull_request(&normalized(serde_json::json!({
            "title": CREATE_TITLE,
            "body": CREATE_BODY,
            "head": "contributor:fork:agent/fix",
            "base": CREATE_BASE
        })))
        .expect_err("a doubly qualified head selector is rejected before dispatch");

        assert_eq!(rejection, InvalidGitHubArguments);
    }

    #[test]
    fn create_response_normalizes_exact_fields() {
        let arguments = decode_create_pull_request(&normalized(serde_json::json!({
            "title": CREATE_TITLE,
            "body": CREATE_BODY,
            "head": CREATE_HEAD,
            "base": CREATE_BASE
        })))
        .expect("creation arguments are admitted");
        let response = serde_json::json!({
            "number": CREATED_PULL_REQUEST_NUMBER,
            "html_url": CREATED_PULL_REQUEST_URL
        });

        let normalized =
            normalize_created_pull_request(&response, &arguments).expect("response normalizes");

        assert_eq!(normalized["number"], response["number"]);
        assert_eq!(normalized["url"], response["html_url"]);
        assert_eq!(normalized["head"], arguments.head());
        assert_eq!(normalized["base"], arguments.base());
    }

    #[test]
    fn publish_schema_states_inline_comment_runtime_bounds() {
        let catalog = catalog();
        let publish = definition(&catalog, PULL_REQUEST_PUBLISH_REVIEW_NAME);
        let schema: serde_json::Value =
            serde_json::from_str(publish.input_schema().as_str()).expect("schema is valid JSON");

        assert_eq!(
            schema["properties"]["comments"]["maxItems"],
            serde_json::json!(MAX_INLINE_COMMENTS)
        );
        assert_eq!(
            schema["$defs"]["InlineReviewComment"]["properties"]["line"]["minimum"],
            serde_json::json!(MIN_INLINE_COMMENT_LINE)
        );
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
    fn inline_comment_deserialization_rejects_zero_line() {
        let value = serde_json::json!({
            "path": FILE_PATH,
            "line": ZERO_INLINE_COMMENT_LINE,
            "side": "right",
            "body": REVIEW_COMMENT_BODY
        });

        assert!(serde_json::from_value::<InlineReviewComment>(value).is_err());
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
        let inline_only_comment = normalized(serde_json::json!({
            "repository": "KeenWill/signalbox", "number": 1,
            "commit_id": HEAD_REVISION, "event": "comment",
            "comments": [{
                "path": FILE_PATH, "line": 1, "side": "right", "body": REVIEW_COMMENT_BODY
            }]
        }));
        let approval = normalized(serde_json::json!({
            "repository": "KeenWill/signalbox", "number": 1,
            "commit_id": HEAD_REVISION, "event": "approve", "comments": []
        }));

        assert!(catalog.validate_arguments(&name, &empty_comment).is_err());
        assert!(
            catalog
                .validate_arguments(&name, &inline_only_comment)
                .is_err()
        );
        assert_eq!(catalog.validate_arguments(&name, &approval), Ok(()));
    }

    #[test]
    fn deserialization_rejects_inline_only_comment() {
        let value = serde_json::json!({
            "repository": "KeenWill/signalbox", "number": 1,
            "commit_id": HEAD_REVISION, "event": "comment",
            "comments": [{
                "path": FILE_PATH, "line": 1, "side": "right", "body": REVIEW_COMMENT_BODY
            }]
        });

        assert!(serde_json::from_value::<PublishReviewArguments>(value).is_err());
    }

    #[test]
    fn deserialization_rejects_bodyless_change_request() {
        let value = serde_json::json!({
            "repository": "KeenWill/signalbox", "number": 1,
            "commit_id": HEAD_REVISION, "event": "request_changes"
        });

        assert!(serde_json::from_value::<PublishReviewArguments>(value).is_err());
    }

    #[test]
    fn deserialization_rejects_too_many_comments() {
        let comment = serde_json::json!({
            "path": FILE_PATH, "line": 1, "side": "right", "body": REVIEW_COMMENT_BODY
        });
        let value = serde_json::json!({
            "repository": "KeenWill/signalbox", "number": 1,
            "commit_id": HEAD_REVISION, "event": "approve",
            "comments": vec![comment; INLINE_COMMENTS_BEYOND_MAX]
        });

        assert!(serde_json::from_value::<PublishReviewArguments>(value).is_err());
    }

    #[test]
    fn approval_without_body_omits_optional_payload_fields() {
        let arguments = publish_arguments(serde_json::json!({
            "repository": "KeenWill/signalbox", "number": 1,
            "commit_id": HEAD_REVISION, "event": "approve"
        }));
        let body = publish_review_body(&arguments).expect("publish fixture serializes");
        let payload: serde_json::Value =
            serde_json::from_slice(&body).expect("serialized fixture is JSON");

        assert_eq!(payload["commit_id"], HEAD_REVISION);
        assert_eq!(payload.get("body"), None);
        assert_eq!(payload.get("comments"), None);
    }

    #[test]
    fn recorded_metadata_preserves_exact_base_and_head() {
        let number =
            PullRequestNumber::try_from(PULL_REQUEST_NUMBER).expect("fixture number is admitted");

        let parsed =
            normalize_metadata(&metadata_response(), number).expect("recorded response is valid");

        assert_eq!(parsed["base_revision"], BASE_REVISION);
        assert_eq!(parsed["head_revision"], HEAD_REVISION);
        assert_eq!(parsed["number"], PULL_REQUEST_NUMBER);
    }

    #[test]
    fn metadata_requires_nullable_members_to_be_present() {
        let number =
            PullRequestNumber::try_from(PULL_REQUEST_NUMBER).expect("fixture number is admitted");
        let mut without_body = metadata_response();
        without_body
            .as_object_mut()
            .expect("metadata fixture is an object")
            .remove("body")
            .expect("metadata fixture contains body");
        let mut without_user = metadata_response();
        without_user
            .as_object_mut()
            .expect("metadata fixture is an object")
            .remove("user")
            .expect("metadata fixture contains user");

        assert_eq!(
            normalize_metadata(&without_body, number),
            Err(invalid_response(None))
        );
        assert_eq!(
            normalize_metadata(&without_user, number),
            Err(invalid_response(None))
        );
    }

    #[test]
    fn metadata_accepts_present_null_body_and_user() {
        let number =
            PullRequestNumber::try_from(PULL_REQUEST_NUMBER).expect("fixture number is admitted");
        let mut response = metadata_response();
        response["body"] = serde_json::Value::Null;
        response["user"] = serde_json::Value::Null;

        let parsed = normalize_metadata(&response, number).expect("nullable members are admitted");

        assert_eq!(parsed["body"], response["body"]);
        assert_eq!(parsed["author"], response["user"]);
    }

    #[test]
    fn diff_snapshot_ignores_unrelated_metadata_fields() {
        let mut response = metadata_response();
        response["body"] = serde_json::Value::String("é".repeat(MAX_TEXT_BYTES));
        let number =
            PullRequestNumber::try_from(PULL_REQUEST_NUMBER).expect("fixture number is admitted");
        let snapshot =
            normalize_diff_snapshot(&response, number).expect("revision-only snapshot is valid");

        assert!(normalize_metadata(&response, number).is_err());
        assert_eq!(snapshot.base_revision, BASE_REVISION);
        assert_eq!(snapshot.head_revision, HEAD_REVISION);
        assert_eq!(snapshot.changed_files, CHANGED_FILES);
    }

    #[test]
    fn diff_snapshot_change_detects_changed_file_count() {
        let number =
            PullRequestNumber::try_from(PULL_REQUEST_NUMBER).expect("fixture number is admitted");
        let initial = normalize_diff_snapshot(&metadata_response(), number)
            .expect("initial snapshot is valid");
        let mut current_response = metadata_response();
        current_response["changed_files"] = serde_json::json!(CHANGED_FILES_AFTER_SNAPSHOT);
        let current =
            normalize_diff_snapshot(&current_response, number).expect("current snapshot is valid");

        assert!(!diff_snapshot_unchanged(&initial, &current));
    }

    #[test]
    fn recorded_files_preserve_patch_text() {
        let parsed = normalize_files(&files_response()).expect("recorded response is valid");

        assert_eq!(parsed.files.len(), CHANGED_FILES);
        assert_eq!(parsed.files[0]["path"], FILE_PATH);
        assert_eq!(parsed.files[0]["patch"], FILE_PATCH);
        assert_eq!(parsed.patch_extent, FilePatchExtent::Complete);
    }

    #[test]
    fn oversized_file_patch_is_omitted_and_marks_content_truncated() {
        let mut response = files_response();
        response[0]["patch"] = serde_json::Value::String(PATCH_FILLER.repeat(MAX_TEXT_BYTES + 1));

        let parsed = normalize_files(&response).expect("bounded response is retained");

        assert_eq!(parsed.files.len(), CHANGED_FILES);
        assert_eq!(parsed.files[0]["path"], FILE_PATH);
        assert_eq!(parsed.files[0]["patch"], serde_json::Value::Null);
        assert_eq!(parsed.patch_extent, FilePatchExtent::Truncated);
    }

    #[test]
    fn malformed_file_patch_is_rejected() {
        let mut response = files_response();
        response[0]["patch"] = serde_json::Value::String(String::from('\0'));

        assert!(normalize_files(&response).is_err());
    }

    #[test]
    fn rest_rejects_more_than_requested_files() {
        let mut response = files_response();
        let file = response[0].clone();
        response = serde_json::Value::Array(vec![file; ITEMS_BEYOND_PAGE_BOUND]);

        assert!(normalize_files(&response).is_err());
    }

    #[test]
    fn duplicate_normalized_file_paths_are_rejected() {
        let mut files = normalize_files(&files_response())
            .expect("recorded response is valid")
            .files;
        files.push(files[0].clone());

        assert_eq!(
            validate_unique_file_paths(&files),
            Err(invalid_response(None))
        );
    }

    #[test]
    fn aggregate_diff_evidence_is_truncated_to_result_bound() {
        let mut response = files_response();
        response[0]["patch"] =
            serde_json::Value::String(PATCH_FILLER.repeat(AGGREGATE_PATCH_BYTES));
        let file = normalize_files(&response)
            .expect("bounded response is retained")
            .files
            .pop()
            .expect("recorded response contains one file");
        let mut result = serde_json::json!({
            "base_revision": BASE_REVISION,
            "head_revision": HEAD_REVISION,
            "files": vec![file; DIFF_FILES_FOR_RESULT_OVERFLOW],
            "truncated": false,
        });

        assert!(
            serde_json::to_vec(&result)
                .expect("fixture serializes")
                .len()
                > MAX_RESULT_BYTES
        );
        truncate_diff_result(&mut result).expect("diff result can be bounded");

        assert!(
            serde_json::to_vec(&result)
                .expect("result serializes")
                .len()
                <= MAX_RESULT_BYTES
        );
        assert_eq!(result["truncated"], serde_json::Value::Bool(true));
        assert_eq!(
            result["files"].as_array().map(Vec::len),
            Some(DIFF_FILES_FOR_RESULT_OVERFLOW)
        );
        assert_eq!(
            result["files"][LAST_OVERFLOW_FILE_INDEX]["patch"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn aggregate_diff_truncation_retains_short_patch() {
        let mut response = files_response();
        response[0]["patch"] =
            serde_json::Value::String(PATCH_FILLER.repeat(AGGREGATE_PATCH_BYTES));
        let file = normalize_files(&response)
            .expect("bounded response is retained")
            .files
            .pop()
            .expect("recorded response contains one file");
        let mut short_file = file.clone();
        short_file["patch"] = serde_json::Value::String(SHORT_PATCH.to_owned());
        let mut files = vec![file; DIFF_FILES_FOR_RESULT_OVERFLOW];
        files[0] = short_file;
        let mut result = serde_json::json!({
            "base_revision": BASE_REVISION,
            "head_revision": HEAD_REVISION,
            "files": files,
            "truncated": false,
        });

        assert!(
            serde_json::to_vec(&result)
                .expect("fixture serializes")
                .len()
                > MAX_RESULT_BYTES
        );
        truncate_diff_result(&mut result).expect("short patch can be retained");

        assert_eq!(result["files"][0]["patch"], SHORT_PATCH);
    }

    #[test]
    fn aggregate_review_thread_evidence_is_truncated_to_result_bound() {
        let mut result = normalize_threads(&threads_response()).expect("recording normalizes");
        let mut comment = result["threads"][0]["comments"][0].clone();
        comment["body"] = serde_json::Value::String(PATCH_FILLER.repeat(MAX_TEXT_BYTES));
        result["threads"][0]["comments"] =
            serde_json::Value::Array(vec![comment; COMMENTS_FOR_RESULT_OVERFLOW]);

        assert!(
            serde_json::to_vec(&result)
                .expect("fixture serializes")
                .len()
                > MAX_RESULT_BYTES
        );
        truncate_review_threads_result(&mut result).expect("thread result can be bounded");
        let retained_comments = result["threads"][0]["comments"]
            .as_array()
            .expect("retained comments stay an array");

        assert!(
            serde_json::to_vec(&result)
                .expect("result serializes")
                .len()
                <= MAX_RESULT_BYTES
        );
        assert_eq!(result["truncated"], serde_json::Value::Bool(true));
        assert_eq!(
            result["threads"][0]["comments_truncated"],
            serde_json::Value::Bool(true)
        );
        assert!(!retained_comments.is_empty());
        assert!(retained_comments.len() < COMMENTS_FOR_RESULT_OVERFLOW);
    }

    #[test]
    fn review_thread_truncation_reserves_later_thread_metadata() {
        let mut result = normalize_threads(&threads_response()).expect("recording normalizes");
        let mut first_thread = result["threads"][0].clone();
        let mut comment = first_thread["comments"][0].clone();
        comment["body"] =
            serde_json::Value::String(PATCH_FILLER.repeat(COMMENT_BODY_FOR_RESERVATION_BYTES));
        first_thread["comments"] =
            serde_json::Value::Array(vec![comment; THREADS_FOR_RESULT_OVERFLOW]);
        let mut metadata_only_thread = first_thread.clone();
        metadata_only_thread["comments"] = serde_json::Value::Array(Vec::new());
        let mut threads = vec![metadata_only_thread; THREADS_FOR_RESULT_OVERFLOW];
        threads[0] = first_thread;
        result["threads"] = serde_json::Value::Array(threads);

        assert!(
            serde_json::to_vec(&result)
                .expect("fixture serializes")
                .len()
                > MAX_RESULT_BYTES
        );
        truncate_review_threads_result(&mut result).expect("thread result can be bounded");

        assert_eq!(
            result["threads"].as_array().map(Vec::len),
            Some(THREADS_FOR_RESULT_OVERFLOW)
        );
        assert_eq!(
            result["threads"][0]["comments_truncated"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn result_kind_mismatch_preserves_mutation_ambiguity() {
        assert_eq!(
            result_kind_mismatch(ToolKind::PublishReview).operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            }
        );
        assert_eq!(
            result_kind_mismatch(ToolKind::Metadata).operator_failure_class(),
            OperatorFailureClass::CallerOrHubBug
        );
    }

    #[test]
    fn github_file_ceiling_is_reported_as_incomplete() {
        assert!(
            files_incomplete(
                GITHUB_FILE_CEILING,
                FILES_BEYOND_CEILING,
                FilePaginationExtent::Complete,
            )
            .expect("bounded inventory is classified")
        );
        assert!(
            !files_incomplete(CHANGED_FILES, CHANGED_FILES, FilePaginationExtent::Complete,)
                .expect("matching inventory is classified")
        );
    }

    #[test]
    fn github_file_inventory_larger_than_snapshot_is_rejected() {
        assert_eq!(
            files_incomplete(
                CHANGED_FILES_AFTER_SNAPSHOT,
                CHANGED_FILES,
                FilePaginationExtent::Complete,
            ),
            Err(invalid_response(None))
        );
    }

    #[test]
    fn public_failures_implement_standard_error() {
        fn require_error<Failure: Error>() {}

        require_error::<InvalidGitHubArguments>();
        require_error::<GitHubTransportFailure>();
    }

    #[test]
    fn send_error_classification_preserves_connect_certainty() {
        assert_eq!(
            classify_send_failure(true),
            GitHubTransportFailure::PreDispatchInfrastructure
        );
        assert_eq!(
            classify_send_failure(false),
            GitHubTransportFailure::DispatchUnknown
        );
    }

    #[test]
    fn client_setup_failure_preserves_definite_commit_outcome() {
        let failure = classify_destination_failure(PublicDestinationClientError::Infrastructure);
        let executor = GitHubTools::try_new(
            SyntheticCredentials,
            SyntheticTransport,
            GitHubEgressPolicy::github_api_only(),
        )
        .expect("static declarations compile")
        .into_parts()
        .1;
        let error = executor
            .failure_detail(ToolKind::PublishReview, &failure)
            .expect_err("pre-dispatch infrastructure is surfaced to the operator");

        assert_eq!(failure, GitHubTransportFailure::PreDispatchInfrastructure);
        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false
            }
        );
    }

    #[test]
    fn fixed_host_destination_rejection_is_pre_dispatch_infrastructure() {
        let failure =
            classify_destination_failure(PublicDestinationClientError::DestinationRejected);
        let executor = GitHubTools::try_new(
            SyntheticCredentials,
            SyntheticTransport,
            GitHubEgressPolicy::github_api_only(),
        )
        .expect("static declarations compile")
        .into_parts()
        .1;
        let error = executor
            .failure_detail(ToolKind::PublishReview, &failure)
            .expect_err("fixed-host admission failure is surfaced to the operator");

        assert_eq!(failure, GitHubTransportFailure::PreDispatchInfrastructure);
        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false
            }
        );
    }

    #[test]
    fn provider_status_distinguishes_rejection_from_infrastructure() {
        let client_status =
            StatusCode::from_u16(CLIENT_ERROR_STATUS).expect("fixture status is valid");
        let server_status =
            StatusCode::from_u16(SERVER_ERROR_STATUS).expect("fixture status is valid");
        let client_failure =
            classify_error_body_failure(client_status, GitHubTransportFailure::DispatchUnknown);
        let server_failure =
            classify_error_body_failure(server_status, GitHubTransportFailure::DispatchUnknown);

        assert!(status_is_definitive(CLIENT_ERROR_STATUS));
        assert!(!status_is_definitive(SERVER_ERROR_STATUS));
        assert_eq!(
            client_failure,
            GitHubTransportFailure::rejected(CLIENT_ERROR_STATUS)
        );
        assert_eq!(server_failure, GitHubTransportFailure::DispatchUnknown);
    }

    #[test]
    fn redirect_status_remains_definitive_after_error_body_failure() {
        let redirect_status =
            StatusCode::from_u16(REDIRECT_STATUS).expect("fixture status is valid");
        let failure =
            classify_error_body_failure(redirect_status, GitHubTransportFailure::DispatchUnknown);

        assert!(status_is_definitive(REDIRECT_STATUS));
        assert_eq!(failure, GitHubTransportFailure::rejected(REDIRECT_STATUS));
    }

    #[test]
    fn provider_response_text_never_enters_durable_error_detail() {
        let executor = GitHubTools::try_new(
            SyntheticCredentials,
            SyntheticTransport,
            GitHubEgressPolicy::github_api_only(),
        )
        .expect("static declarations compile")
        .into_parts()
        .1;
        let expected = make_detail(REQUEST_REJECTED_DETAIL).expect("fixed detail is valid");
        let rejection = GitHubTransportFailure::Rejected {
            status: CLIENT_ERROR_STATUS,
            detail: Some(SanitizedGitHubError(PROVIDER_ERROR_TEXT.to_owned())),
        };
        let malformed = GitHubTransportFailure::InvalidResponse {
            detail: Some(SanitizedGitHubError(PROVIDER_ERROR_TEXT.to_owned())),
        };

        assert_eq!(
            executor.failure_detail(ToolKind::PublishReview, &rejection),
            Ok(expected.clone())
        );
        assert_eq!(
            executor.failure_detail(ToolKind::Metadata, &malformed),
            Ok(expected)
        );
    }

    #[test]
    fn exhausted_diff_deadline_is_dispatch_unknown() {
        assert_eq!(
            remaining_timeout(Instant::now()),
            Err(GitHubTransportFailure::DispatchUnknown),
        );
    }

    #[test]
    fn graphql_unclassified_errors_are_dispatch_unknown() {
        let mut response = threads_response();
        response["errors"] = serde_json::json!([{"message": GRAPHQL_ERROR_MESSAGE}]);

        assert_eq!(
            validate_graphql_errors(&response),
            Err(GitHubTransportFailure::DispatchUnknown)
        );
    }

    #[test]
    fn graphql_definitive_error_types_are_rejections() {
        let mut response = threads_response();
        response["errors"] = serde_json::json!([
            {"message": GRAPHQL_ERROR_MESSAGE, "type": GRAPHQL_NOT_FOUND},
            {"message": GRAPHQL_ERROR_MESSAGE, "type": GRAPHQL_FORBIDDEN}
        ]);

        assert_eq!(
            validate_graphql_errors(&response),
            Err(GitHubTransportFailure::GraphQlRejected)
        );
    }

    #[test]
    fn graphql_definitive_extension_codes_are_rejections() {
        let mut response = threads_response();
        response["errors"] = serde_json::json!([{
            "message": GRAPHQL_ERROR_MESSAGE,
            "extensions": {"code": GRAPHQL_NOT_FOUND}
        }]);

        assert_eq!(
            validate_graphql_errors(&response),
            Err(GitHubTransportFailure::GraphQlRejected)
        );
    }

    #[test]
    fn graphql_transient_error_prevents_definitive_classification() {
        let mut response = threads_response();
        response["errors"] = serde_json::json!([
            {"message": GRAPHQL_ERROR_MESSAGE, "type": GRAPHQL_NOT_FOUND},
            {"message": GRAPHQL_ERROR_MESSAGE, "type": GRAPHQL_RATE_LIMITED}
        ]);

        assert_eq!(
            validate_graphql_errors(&response),
            Err(GitHubTransportFailure::DispatchUnknown)
        );
    }
    #[test]
    fn graphql_empty_errors_allow_normalization() {
        let mut response = threads_response();
        response["errors"] = serde_json::json!([]);

        validate_graphql_errors(&response).expect("empty GraphQL errors are non-errors");
        normalize_threads(&response).expect("data remains available");
    }

    #[test]
    fn graphql_recording_preserves_resolution_state_and_comments() {
        let parsed = normalize_threads(&threads_response()).expect("recorded response is valid");

        assert_eq!(parsed["truncated"], THREADS_TRUNCATED);
        assert_eq!(parsed["threads"][0]["resolved"], THREAD_RESOLVED);
        assert_eq!(parsed["threads"][0]["outdated"], THREAD_OUTDATED);
        assert_eq!(
            parsed["threads"][0]["comments_truncated"],
            COMMENTS_TRUNCATED
        );
        assert_eq!(
            parsed["threads"][0]["comments"][0]["body"],
            REVIEW_COMMENT_BODY
        );
    }

    #[test]
    fn graphql_requires_nullable_thread_line_to_be_present() {
        let mut response = threads_response();
        response["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]
            .as_object_mut()
            .expect("thread fixture is an object")
            .remove("line")
            .expect("thread fixture contains line");

        assert_eq!(normalize_threads(&response), Err(invalid_response(None)));
    }

    #[test]
    fn graphql_requires_nullable_comment_author_to_be_present() {
        let mut response = threads_response();
        response["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["comments"]
            ["nodes"][0]
            .as_object_mut()
            .expect("comment fixture is an object")
            .remove("author")
            .expect("comment fixture contains author");

        assert_eq!(normalize_threads(&response), Err(invalid_response(None)));
    }

    #[test]
    fn graphql_accepts_present_null_thread_line_and_comment_author() {
        let mut response = threads_response();
        response["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["line"] =
            serde_json::Value::Null;
        response["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["comments"]["nodes"]
            [0]["author"] = serde_json::Value::Null;

        let parsed = normalize_threads(&response).expect("nullable members are admitted");

        assert_eq!(
            parsed["threads"][0]["line"],
            response["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["line"]
        );
        assert_eq!(
            parsed["threads"][0]["comments"][0]["author"],
            response["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["comments"]
                ["nodes"][0]["author"]
        );
    }

    #[test]
    fn graphql_rejects_more_than_requested_review_threads() {
        let mut response = threads_response();
        let thread =
            response["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0].clone();
        response["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"] =
            serde_json::Value::Array(vec![thread; ITEMS_BEYOND_PAGE_BOUND]);

        assert!(normalize_threads(&response).is_err());
    }

    #[test]
    fn graphql_rejects_more_than_requested_review_comments() {
        let mut response = threads_response();
        let comment =
            response["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["comments"]
                ["nodes"][0]
                .clone();
        response["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["comments"]["nodes"] =
            serde_json::Value::Array(vec![comment; ITEMS_BEYOND_PAGE_BOUND]);

        assert!(normalize_threads(&response).is_err());
    }

    #[test]
    fn graphql_rejects_duplicate_review_thread_ids() {
        let mut response = threads_response();
        let thread =
            response["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0].clone();
        response["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"] =
            serde_json::json!([thread.clone(), thread]);

        assert!(normalize_threads(&response).is_err());
    }

    #[test]
    fn graphql_rejects_duplicate_review_comment_ids_within_thread() {
        let mut response = threads_response();
        let comment =
            response["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["comments"]
                ["nodes"][0]
                .clone();
        response["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["comments"]["nodes"] =
            serde_json::json!([comment.clone(), comment]);

        assert!(normalize_threads(&response).is_err());
    }

    #[test]
    fn graphql_rejects_duplicate_review_comment_ids_across_threads() {
        let mut response = threads_response();
        let mut other_thread =
            response["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0].clone();
        other_thread["id"] = serde_json::Value::String(OTHER_THREAD_ID.to_owned());
        let first_thread =
            response["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0].clone();
        response["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"] =
            serde_json::json!([first_thread, other_thread]);

        assert!(normalize_threads(&response).is_err());
    }

    #[test]
    fn graphql_rejects_oversized_review_thread_id() {
        let mut response = threads_response();
        response["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["id"] =
            serde_json::Value::String(OPAQUE_ID_FILLER.repeat(OPAQUE_ID_BYTES_BEYOND_BOUND));

        assert!(normalize_threads(&response).is_err());
    }

    #[test]
    fn graphql_rejects_controlled_review_comment_id() {
        let mut response = threads_response();
        response["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["comments"]["nodes"]
            [0]["id"] = serde_json::Value::String(CONTROLLED_OPAQUE_ID.to_owned());

        assert!(normalize_threads(&response).is_err());
    }

    #[test]
    fn error_body_redaction_precedes_truncation() {
        let credential = CredentialValue::new(SYNTHETIC_TOKEN.as_bytes().to_vec());
        let scrubber = CredentialScrubber::try_new(&credential).expect("fixture token is admitted");
        let prefix = "x".repeat(
            MAX_ERROR_DETAIL_BYTES - ERROR_TRUNCATION_SUFFIX.len() - "[redacted]".len() - 4,
        );
        let body = format!("{prefix}{SYNTHETIC_TOKEN}{}", "tail".repeat(100));

        let sanitized = sanitize_error_body(body.as_bytes(), ResponseExtent::Complete, &scrubber)
            .expect("nonempty error remains");

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
        let body = format!("{ERROR_BODY_PREFIX}{token_prefix}");

        let sanitized = sanitize_error_body(body.as_bytes(), ResponseExtent::Truncated, &scrubber)
            .expect("nonempty error remains");

        assert_eq!(sanitized.as_str(), format!("{ERROR_BODY_PREFIX}[redacted]"));
    }

    #[test]
    fn truncated_error_source_handles_unicode_token_prefix() {
        let credential = CredentialValue::new(SYNTHETIC_UNICODE_TOKEN.as_bytes().to_vec());
        let scrubber = CredentialScrubber::try_new(&credential).expect("fixture token is admitted");
        let body = format!("{ERROR_BODY_PREFIX}{SYNTHETIC_UNICODE_PREFIX}");

        let sanitized = sanitize_error_body(body.as_bytes(), ResponseExtent::Truncated, &scrubber)
            .expect("nonempty error remains");

        assert_eq!(sanitized.as_str(), format!("{ERROR_BODY_PREFIX}[redacted]"));
    }

    #[test]
    fn truncated_error_source_discards_partial_unicode_before_redaction() {
        let credential = CredentialValue::new(SYNTHETIC_UNICODE_TOKEN.as_bytes().to_vec());
        let scrubber = CredentialScrubber::try_new(&credential).expect("fixture token is admitted");
        let partial_prefix_end = SYNTHETIC_UNICODE_PREFIX.len() + 1;
        let mut body = ERROR_BODY_PREFIX.as_bytes().to_vec();
        body.extend_from_slice(&SYNTHETIC_UNICODE_TOKEN.as_bytes()[..partial_prefix_end]);

        let sanitized = sanitize_error_body(&body, ResponseExtent::Truncated, &scrubber)
            .expect("nonempty error remains");

        assert_eq!(sanitized.as_str(), format!("{ERROR_BODY_PREFIX}[redacted]"));
        assert!(!sanitized.as_str().contains(char::REPLACEMENT_CHARACTER));
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
            "state": PUBLISHED_REVIEW_STATE,
            "html_url": "https://github.com/KeenWill/signalbox/pull/1#pullrequestreview-7001",
            "commit_id": HEAD_REVISION
        });

        let parsed =
            normalize_published_review(&response, HEAD_REVISION, PublishReviewEvent::Approve)
                .expect("recorded response is valid");

        assert_eq!(parsed["commit_id"], HEAD_REVISION);
        assert_eq!(parsed["state"], PUBLISHED_REVIEW_STATE);
    }

    #[test]
    fn mutation_acknowledgement_rejects_inconsistent_state_as_ambiguous() {
        let response = serde_json::json!({
            "id": 7001,
            "state": MISMATCHED_PUBLISHED_REVIEW_STATE,
            "html_url": "https://github.com/KeenWill/signalbox/pull/1#pullrequestreview-7001",
            "commit_id": HEAD_REVISION
        });
        let failure =
            normalize_published_review(&response, HEAD_REVISION, PublishReviewEvent::Approve)
                .expect_err("mismatched acknowledgement is rejected");

        assert_eq!(
            mutation_failure(failure),
            GitHubTransportFailure::DispatchUnknown
        );
    }
}
