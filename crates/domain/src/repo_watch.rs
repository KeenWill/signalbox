//! Repository-watch events, matchers, and dispatch action values.
//!
//! The normative cross-component contract is `docs/spec/repo-watch.md`.

use std::{
    error::Error,
    fmt,
    hash::{Hash, Hasher},
    num::NonZeroU64,
    time::Duration,
};

use regex::Regex;

use crate::{RepoWatchEventId, SessionTemplateName};

const MAX_REPOSITORY_BYTES: usize = 201;
const MAX_BRANCH_BYTES: usize = 255;
const MAX_LOGIN_BASE_BYTES: usize = 39;
const BOT_LOGIN_SUFFIX: &str = "[bot]";
const MAX_LOGIN_BYTES: usize = MAX_LOGIN_BASE_BYTES + BOT_LOGIN_SUFFIX.len();
const MAX_LABEL_BYTES: usize = 200;
const MAX_LABEL_CHARACTERS: usize = 50;
const MAX_NAME_BYTES: usize = 256;
const MAX_REACTION_BYTES: usize = 64;
const MAX_RULE_ID_BYTES: usize = 128;
const MAX_PATTERN_BYTES: usize = 1_024;
const MAX_TITLE_BYTES: usize = 1_024;
const MAX_BODY_BYTES: usize = 262_144;

/// Why one repository-watch text value was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchTextError {
    Empty,
    ContainsNull,
    TooLong { bytes: usize, maximum: usize },
    TooManyCharacters { characters: usize, maximum: usize },
    Malformed,
    UnanchoredPattern,
    InvalidPattern { reason: String },
}

impl fmt::Display for RepoWatchTextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("repository-watch value is empty"),
            Self::ContainsNull => formatter.write_str("repository-watch value contains U+0000"),
            Self::TooLong { bytes, maximum } => write!(
                formatter,
                "repository-watch value has {bytes} bytes; maximum is {maximum}"
            ),
            Self::TooManyCharacters {
                characters,
                maximum,
            } => write!(
                formatter,
                "repository-watch value has {characters} characters; maximum is {maximum}"
            ),
            Self::Malformed => formatter.write_str("repository-watch value has an invalid shape"),
            Self::UnanchoredPattern => {
                formatter.write_str("repository-watch regex must be anchored with ^ and $")
            }
            Self::InvalidPattern { reason } => {
                write!(formatter, "repository-watch regex is invalid: {reason}")
            }
        }
    }
}

impl Error for RepoWatchTextError {}

fn validate_text(value: &str, maximum: usize) -> Result<(), RepoWatchTextError> {
    if value.is_empty() {
        Err(RepoWatchTextError::Empty)
    } else if value.contains('\0') {
        Err(RepoWatchTextError::ContainsNull)
    } else if value.len() > maximum {
        Err(RepoWatchTextError::TooLong {
            bytes: value.len(),
            maximum,
        })
    } else {
        Ok(())
    }
}

macro_rules! bounded_text {
    ($(#[$meta:meta])* $name:ident, $maximum:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn try_new(value: String) -> Result<Self, RepoWatchTextError> {
                validate_text(&value, $maximum)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }
    };
}

/// A GitHub repository in canonical `namespace/name` spelling.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepositorySlug(String);

impl RepositorySlug {
    pub fn try_new(mut value: String) -> Result<Self, RepoWatchTextError> {
        validate_text(&value, MAX_REPOSITORY_BYTES)?;
        let mut parts = value.split('/');
        let namespace = parts.next().unwrap_or_default();
        let repository = parts.next().unwrap_or_default();
        if !valid_repository_segment(namespace)
            || !valid_repository_segment(repository)
            || parts.next().is_some()
        {
            return Err(RepoWatchTextError::Malformed);
        }
        value.make_ascii_lowercase();
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
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

/// One exact repository branch name admitted by Git's ref-name grammar.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BranchName(String);

impl BranchName {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError> {
        validate_text(&value, MAX_BRANCH_BYTES)?;
        let invalid_component = value.split('/').any(|component| {
            component.is_empty() || component.starts_with('.') || component.ends_with(".lock")
        });
        if value == "@"
            || value.starts_with('-')
            || value.ends_with('.')
            || value.contains("..")
            || value.contains("@{")
            || value.bytes().any(|byte| {
                byte <= 0x20
                    || byte == 0x7f
                    || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
            })
            || invalid_component
        {
            return Err(RepoWatchTextError::Malformed);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}
/// One exact repository label name.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LabelName(String);

impl LabelName {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError> {
        validate_text(&value, MAX_LABEL_BYTES)?;
        let characters = value.chars().count();
        if characters > MAX_LABEL_CHARACTERS {
            return Err(RepoWatchTextError::TooManyCharacters {
                characters,
                maximum: MAX_LABEL_CHARACTERS,
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}
/// One checked GitHub human, managed-user, or App-bot actor login.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepoWatchAuthorLogin(String);

impl RepoWatchAuthorLogin {
    pub fn try_new(mut value: String) -> Result<Self, RepoWatchTextError> {
        validate_text(&value, MAX_LOGIN_BYTES)?;
        value.make_ascii_lowercase();
        let base = value.strip_suffix(BOT_LOGIN_SUFFIX).unwrap_or(&value);
        let valid = !base.is_empty()
            && base.len() <= MAX_LOGIN_BASE_BYTES
            && !base.starts_with('-')
            && !base.ends_with('-')
            && !base.contains("--")
            && base
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
        if !valid {
            return Err(RepoWatchTextError::Malformed);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}
bounded_text!(/// One check-run name.
    CheckRunName, MAX_NAME_BYTES);
bounded_text!(/// One workflow name.
    WorkflowName, MAX_NAME_BYTES);
bounded_text!(/// One reaction content spelling retained as event evidence.
    ReactionContent, MAX_REACTION_BYTES);
/// One stable operator-defined rule name.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepoWatchRuleId(String);

impl RepoWatchRuleId {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError> {
        validate_text(&value, MAX_RULE_ID_BYTES)?;
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(RepoWatchTextError::Malformed);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}
bounded_text!(/// One stable provider review-thread identifier.
    ReviewThreadId, MAX_NAME_BYTES);
bounded_text!(/// One exact pull-request title.
    PullRequestTitle, MAX_TITLE_BYTES);

/// One possibly empty, bounded pull-request body.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PullRequestBody(String);

impl PullRequestBody {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError> {
        if value.contains('\0') {
            Err(RepoWatchTextError::ContainsNull)
        } else if value.len() > MAX_BODY_BYTES {
            Err(RepoWatchTextError::TooLong {
                bytes: value.len(),
                maximum: MAX_BODY_BYTES,
            })
        } else {
            Ok(Self(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

/// One exact Git commit object identifier.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CommitSha(String);

impl CommitSha {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError> {
        if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(RepoWatchTextError::Malformed);
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

/// One positive pull-request number.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PullRequestNumber(NonZeroU64);

impl PullRequestNumber {
    pub const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// One positive provider object identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GitHubObjectId(NonZeroU64);

impl GitHubObjectId {
    pub const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// One bounded, anchored, linear-time regular expression.
#[derive(Clone)]
pub struct RepoWatchPattern {
    source: String,
    compiled: Regex,
}

impl fmt::Debug for RepoWatchPattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("RepoWatchPattern")
            .field(&self.source)
            .finish()
    }
}

impl PartialEq for RepoWatchPattern {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}

impl Eq for RepoWatchPattern {}

impl Hash for RepoWatchPattern {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.source.hash(state);
    }
}

impl RepoWatchPattern {
    pub const MAX_UTF8_BYTES: usize = MAX_PATTERN_BYTES;

    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError> {
        validate_text(&value, MAX_PATTERN_BYTES)?;
        if !value.starts_with('^') || !value.ends_with('$') {
            return Err(RepoWatchTextError::UnanchoredPattern);
        }
        let compiled = Regex::new(&format!(r"\A(?:{value})\z")).map_err(|error| {
            RepoWatchTextError::InvalidPattern {
                reason: error.to_string(),
            }
        })?;
        Ok(Self {
            source: value,
            compiled,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.source
    }

    pub fn is_match(&self, candidate: &str) -> bool {
        self.compiled.is_match(candidate)
    }
}

/// Version of one durable rule shape.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepoWatchRuleVersion(NonZeroU64);

impl RepoWatchRuleVersion {
    pub const V1: Self = Self(NonZeroU64::MIN);

    pub const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Closed version-one event-kind discriminator vocabulary.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RepoWatchEventKindNameV1 {
    PullRequestOpened,
    PullRequestClosed,
    PullRequestMerged,
    HeadChanged,
    MergeableStateChanged,
    ChecksCompleted,
    CheckRunCompleted,
    BranchWorkflowRunCompleted,
    ReviewSubmitted,
    ThreadOpened,
    ThreadResolved,
    Labeled,
    Unlabeled,
    BaseAdvanced,
    ReactionChanged,
}

const PULL_REQUEST_EVENT_KINDS_V1: [RepoWatchEventKindNameV1; 14] = [
    RepoWatchEventKindNameV1::PullRequestOpened,
    RepoWatchEventKindNameV1::PullRequestClosed,
    RepoWatchEventKindNameV1::PullRequestMerged,
    RepoWatchEventKindNameV1::HeadChanged,
    RepoWatchEventKindNameV1::MergeableStateChanged,
    RepoWatchEventKindNameV1::ChecksCompleted,
    RepoWatchEventKindNameV1::CheckRunCompleted,
    RepoWatchEventKindNameV1::ReviewSubmitted,
    RepoWatchEventKindNameV1::ThreadOpened,
    RepoWatchEventKindNameV1::ThreadResolved,
    RepoWatchEventKindNameV1::Labeled,
    RepoWatchEventKindNameV1::Unlabeled,
    RepoWatchEventKindNameV1::BaseAdvanced,
    RepoWatchEventKindNameV1::ReactionChanged,
];

/// Aggregate check-suite result used by the closed version-one vocabulary.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ChecksOutcome {
    Success,
    Failure,
}

/// Closed provider conclusion vocabulary admitted by version one.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CheckConclusion {
    Success,
    Failure,
    Neutral,
    Cancelled,
    Skipped,
    TimedOut,
    ActionRequired,
    Stale,
    StartupFailure,
}

impl From<ChecksOutcome> for CheckConclusion {
    fn from(value: ChecksOutcome) -> Self {
        match value {
            ChecksOutcome::Success => Self::Success,
            ChecksOutcome::Failure => Self::Failure,
        }
    }
}

/// GitHub's mergeability classification retained by the differ.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MergeableState {
    Mergeable,
    Conflicting,
    Unknown,
}

/// Review state relevant to repository-watch rules and dispatch context.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
}

/// Whether a configured reviewer's reaction was added or removed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReactionChange {
    Added,
    Removed,
}

/// The object on which a configured reviewer's reaction changed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReactionSubject {
    PullRequestBody,
    IssueComment { id: GitHubObjectId },
    ReviewComment { id: GitHubObjectId },
}

/// Closed version-one repository-watch fact payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchEventKindV1 {
    PullRequestOpened,
    PullRequestClosed,
    PullRequestMerged,
    HeadChanged {
        previous: CommitSha,
        current: CommitSha,
    },
    MergeableStateChanged {
        current: MergeableState,
    },
    ChecksCompleted {
        outcome: ChecksOutcome,
    },
    CheckRunCompleted {
        name: CheckRunName,
        conclusion: CheckConclusion,
    },
    BranchWorkflowRunCompleted {
        branch: BranchName,
        workflow: WorkflowName,
        conclusion: CheckConclusion,
    },
    ReviewSubmitted {
        reviewer: RepoWatchAuthorLogin,
        state: ReviewState,
        commit: CommitSha,
    },
    ThreadOpened {
        thread: ReviewThreadId,
    },
    ThreadResolved {
        thread: ReviewThreadId,
    },
    Labeled {
        label: LabelName,
    },
    Unlabeled {
        label: LabelName,
    },
    BaseAdvanced {
        branch: BranchName,
    },
    ReactionChanged {
        subject: ReactionSubject,
        reactor: RepoWatchAuthorLogin,
        content: ReactionContent,
        change: ReactionChange,
    },
}

impl RepoWatchEventKindV1 {
    pub const fn name(&self) -> RepoWatchEventKindNameV1 {
        match self {
            Self::PullRequestOpened => RepoWatchEventKindNameV1::PullRequestOpened,
            Self::PullRequestClosed => RepoWatchEventKindNameV1::PullRequestClosed,
            Self::PullRequestMerged => RepoWatchEventKindNameV1::PullRequestMerged,
            Self::HeadChanged { .. } => RepoWatchEventKindNameV1::HeadChanged,
            Self::MergeableStateChanged { .. } => RepoWatchEventKindNameV1::MergeableStateChanged,
            Self::ChecksCompleted { .. } => RepoWatchEventKindNameV1::ChecksCompleted,
            Self::CheckRunCompleted { .. } => RepoWatchEventKindNameV1::CheckRunCompleted,
            Self::BranchWorkflowRunCompleted { .. } => {
                RepoWatchEventKindNameV1::BranchWorkflowRunCompleted
            }
            Self::ReviewSubmitted { .. } => RepoWatchEventKindNameV1::ReviewSubmitted,
            Self::ThreadOpened { .. } => RepoWatchEventKindNameV1::ThreadOpened,
            Self::ThreadResolved { .. } => RepoWatchEventKindNameV1::ThreadResolved,
            Self::Labeled { .. } => RepoWatchEventKindNameV1::Labeled,
            Self::Unlabeled { .. } => RepoWatchEventKindNameV1::Unlabeled,
            Self::BaseAdvanced { .. } => RepoWatchEventKindNameV1::BaseAdvanced,
            Self::ReactionChanged { .. } => RepoWatchEventKindNameV1::ReactionChanged,
        }
    }
}

/// Complete normalized pull-request facts available to version-one matchers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequestEventContext {
    number: PullRequestNumber,
    head_sha: CommitSha,
    head_repository: RepositorySlug,
    base_branch: BranchName,
    head_branch: BranchName,
    title: PullRequestTitle,
    body: PullRequestBody,
    labels: Box<[LabelName]>,
    draft: bool,
    author: Option<RepoWatchAuthorLogin>,
}

/// Field-labeled construction input for normalized pull-request event context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequestEventContextInput {
    pub number: PullRequestNumber,
    pub head_sha: CommitSha,
    pub head_repository: RepositorySlug,
    pub base_branch: BranchName,
    pub head_branch: BranchName,
    pub title: PullRequestTitle,
    pub body: PullRequestBody,
    pub labels: Vec<LabelName>,
    pub draft: bool,
    pub author: Option<RepoWatchAuthorLogin>,
}

impl PullRequestEventContext {
    pub fn new(input: PullRequestEventContextInput) -> Self {
        let mut labels = input.labels;
        labels.sort();
        labels.dedup();
        Self {
            number: input.number,
            head_sha: input.head_sha,
            head_repository: input.head_repository,
            base_branch: input.base_branch,
            head_branch: input.head_branch,
            title: input.title,
            body: input.body,
            labels: labels.into_boxed_slice(),
            draft: input.draft,
            author: input.author,
        }
    }

    pub const fn number(&self) -> PullRequestNumber {
        self.number
    }
    pub const fn head_sha(&self) -> &CommitSha {
        &self.head_sha
    }
    pub const fn head_repository(&self) -> &RepositorySlug {
        &self.head_repository
    }
    pub const fn base_branch(&self) -> &BranchName {
        &self.base_branch
    }
    pub const fn head_branch(&self) -> &BranchName {
        &self.head_branch
    }
    pub const fn title(&self) -> &PullRequestTitle {
        &self.title
    }
    pub const fn body(&self) -> &PullRequestBody {
        &self.body
    }
    pub fn labels(&self) -> &[LabelName] {
        &self.labels
    }
    pub const fn draft(&self) -> bool {
        self.draft
    }
    pub const fn author(&self) -> Option<&RepoWatchAuthorLogin> {
        self.author.as_ref()
    }
}

/// The subject shape carried by one version-one event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchEventTarget {
    PullRequest(PullRequestEventContext),
    Branch,
}

/// One version-one durable repository-watch fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchEvent {
    id: RepoWatchEventId,
    repository: RepositorySlug,
    target: RepoWatchEventTarget,
    kind: RepoWatchEventKindV1,
}

impl RepoWatchEvent {
    pub fn try_pull_request(
        id: RepoWatchEventId,
        repository: RepositorySlug,
        context: PullRequestEventContext,
        kind: RepoWatchEventKindV1,
    ) -> Result<Self, RepoWatchEventConstructionError> {
        match &kind {
            RepoWatchEventKindV1::BranchWorkflowRunCompleted { .. } => {
                return Err(RepoWatchEventConstructionError::BranchKindOnPullRequest);
            }
            RepoWatchEventKindV1::HeadChanged { previous, current } if previous == current => {
                return Err(RepoWatchEventConstructionError::HeadChangedWithoutChange);
            }
            RepoWatchEventKindV1::HeadChanged { current, .. } if current != context.head_sha() => {
                return Err(RepoWatchEventConstructionError::HeadChangedCurrentMismatch);
            }
            RepoWatchEventKindV1::BaseAdvanced { branch } if branch != context.base_branch() => {
                return Err(RepoWatchEventConstructionError::BaseAdvancedBranchMismatch);
            }
            RepoWatchEventKindV1::Labeled { label } if !context.labels().contains(label) => {
                return Err(RepoWatchEventConstructionError::LabeledContextMissingLabel);
            }
            RepoWatchEventKindV1::Unlabeled { label } if context.labels().contains(label) => {
                return Err(RepoWatchEventConstructionError::UnlabeledContextContainsLabel);
            }
            RepoWatchEventKindV1::PullRequestOpened
            | RepoWatchEventKindV1::PullRequestClosed
            | RepoWatchEventKindV1::PullRequestMerged
            | RepoWatchEventKindV1::HeadChanged { .. }
            | RepoWatchEventKindV1::MergeableStateChanged { .. }
            | RepoWatchEventKindV1::ChecksCompleted { .. }
            | RepoWatchEventKindV1::CheckRunCompleted { .. }
            | RepoWatchEventKindV1::ReviewSubmitted { .. }
            | RepoWatchEventKindV1::ThreadOpened { .. }
            | RepoWatchEventKindV1::ThreadResolved { .. }
            | RepoWatchEventKindV1::Labeled { .. }
            | RepoWatchEventKindV1::Unlabeled { .. }
            | RepoWatchEventKindV1::BaseAdvanced { .. }
            | RepoWatchEventKindV1::ReactionChanged { .. } => {}
        }
        Ok(Self {
            id,
            repository,
            target: RepoWatchEventTarget::PullRequest(context),
            kind,
        })
    }

    pub const fn branch_workflow(
        id: RepoWatchEventId,
        repository: RepositorySlug,
        branch: BranchName,
        workflow: WorkflowName,
        conclusion: CheckConclusion,
    ) -> Self {
        Self {
            id,
            repository,
            target: RepoWatchEventTarget::Branch,
            kind: RepoWatchEventKindV1::BranchWorkflowRunCompleted {
                branch,
                workflow,
                conclusion,
            },
        }
    }

    pub const fn id(&self) -> RepoWatchEventId {
        self.id
    }
    pub const fn repository(&self) -> &RepositorySlug {
        &self.repository
    }
    pub const fn target(&self) -> &RepoWatchEventTarget {
        &self.target
    }
    pub const fn kind(&self) -> &RepoWatchEventKindV1 {
        &self.kind
    }
}

/// Why an event target and event kind could not be combined.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchEventConstructionError {
    BranchKindOnPullRequest,
    HeadChangedCurrentMismatch,
    HeadChangedWithoutChange,
    BaseAdvancedBranchMismatch,
    LabeledContextMissingLabel,
    UnlabeledContextContainsLabel,
}

impl fmt::Display for RepoWatchEventConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BranchKindOnPullRequest => {
                "branch-workflow event cannot carry a pull-request target"
            }
            Self::HeadChangedCurrentMismatch => {
                "head-change current SHA differs from pull-request context"
            }
            Self::HeadChangedWithoutChange => "head-change previous and current SHAs are identical",
            Self::BaseAdvancedBranchMismatch => {
                "base-advance branch differs from pull-request context"
            }
            Self::LabeledContextMissingLabel => {
                "labeled event label is absent from pull-request context"
            }
            Self::UnlabeledContextContainsLabel => {
                "unlabeled event label remains in pull-request context"
            }
        })
    }
}

impl Error for RepoWatchEventConstructionError {}

/// Label predicates for one version-one rule. Empty lists impose no condition.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RepoWatchLabelMatcher {
    any_of: Box<[LabelName]>,
    all_of: Box<[LabelName]>,
    none_of: Box<[LabelName]>,
}

/// Field-labeled construction input for version-one label predicates.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RepoWatchLabelMatcherInput {
    pub any_of: Vec<LabelName>,
    pub all_of: Vec<LabelName>,
    pub none_of: Vec<LabelName>,
}

impl RepoWatchLabelMatcher {
    pub fn new(input: RepoWatchLabelMatcherInput) -> Self {
        Self {
            any_of: input.any_of.into_boxed_slice(),
            all_of: input.all_of.into_boxed_slice(),
            none_of: input.none_of.into_boxed_slice(),
        }
    }

    pub fn any_of(&self) -> &[LabelName] {
        &self.any_of
    }
    pub fn all_of(&self) -> &[LabelName] {
        &self.all_of
    }
    pub fn none_of(&self) -> &[LabelName] {
        &self.none_of
    }
}

/// Structured, conjunctive version-one rule matcher.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RepoWatchMatcherV1 {
    event_kinds: Box<[RepoWatchEventKindNameV1]>,
    repository: Option<RepositorySlug>,
    base_branch: Option<BranchName>,
    head_branch: Option<RepoWatchPattern>,
    title: Option<RepoWatchPattern>,
    body: Option<RepoWatchPattern>,
    labels: RepoWatchLabelMatcher,
    draft: Option<bool>,
    author: Option<RepoWatchAuthorLogin>,
    mergeable_state: Box<[MergeableState]>,
    conclusion: Box<[CheckConclusion]>,
}

/// Field-labeled construction input for one version-one rule matcher.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RepoWatchMatcherV1Input {
    pub event_kinds: Vec<RepoWatchEventKindNameV1>,
    pub repository: Option<RepositorySlug>,
    pub base_branch: Option<BranchName>,
    pub head_branch: Option<RepoWatchPattern>,
    pub title: Option<RepoWatchPattern>,
    pub body: Option<RepoWatchPattern>,
    pub labels: RepoWatchLabelMatcher,
    pub draft: Option<bool>,
    pub author: Option<RepoWatchAuthorLogin>,
    pub mergeable_state: Vec<MergeableState>,
    pub conclusion: Vec<CheckConclusion>,
}

impl RepoWatchMatcherV1 {
    pub fn new(input: RepoWatchMatcherV1Input) -> Self {
        Self {
            event_kinds: input.event_kinds.into_boxed_slice(),
            repository: input.repository,
            base_branch: input.base_branch,
            head_branch: input.head_branch,
            title: input.title,
            body: input.body,
            labels: input.labels,
            draft: input.draft,
            author: input.author,
            mergeable_state: input.mergeable_state.into_boxed_slice(),
            conclusion: input.conclusion.into_boxed_slice(),
        }
    }

    fn produces_branch_context(&self) -> bool {
        self.base_branch.is_none()
            && self.head_branch.is_none()
            && self.title.is_none()
            && self.body.is_none()
            && self.labels.any_of.is_empty()
            && self.labels.all_of.is_empty()
            && self.labels.none_of.is_empty()
            && self.draft.is_none()
            && self.author.is_none()
            && self.kind_can_match(RepoWatchEventKindNameV1::BranchWorkflowRunCompleted)
    }

    fn produces_pull_request_context(&self) -> bool {
        PULL_REQUEST_EVENT_KINDS_V1
            .into_iter()
            .any(|kind| self.kind_can_match(kind))
    }

    fn kind_can_match(&self, kind: RepoWatchEventKindNameV1) -> bool {
        let selected = self.event_kinds.is_empty() || self.event_kinds.contains(&kind);
        let mergeable_applies = self.mergeable_state.is_empty()
            || kind == RepoWatchEventKindNameV1::MergeableStateChanged;
        let conclusion_applies = self.conclusion.is_empty()
            || match kind {
                RepoWatchEventKindNameV1::ChecksCompleted => self.conclusion.iter().any(|value| {
                    matches!(value, CheckConclusion::Success | CheckConclusion::Failure)
                }),
                RepoWatchEventKindNameV1::CheckRunCompleted
                | RepoWatchEventKindNameV1::BranchWorkflowRunCompleted => true,
                RepoWatchEventKindNameV1::PullRequestOpened
                | RepoWatchEventKindNameV1::PullRequestClosed
                | RepoWatchEventKindNameV1::PullRequestMerged
                | RepoWatchEventKindNameV1::HeadChanged
                | RepoWatchEventKindNameV1::MergeableStateChanged
                | RepoWatchEventKindNameV1::ReviewSubmitted
                | RepoWatchEventKindNameV1::ThreadOpened
                | RepoWatchEventKindNameV1::ThreadResolved
                | RepoWatchEventKindNameV1::Labeled
                | RepoWatchEventKindNameV1::Unlabeled
                | RepoWatchEventKindNameV1::BaseAdvanced
                | RepoWatchEventKindNameV1::ReactionChanged => false,
            };
        selected && mergeable_applies && conclusion_applies
    }

    pub fn event_kinds(&self) -> &[RepoWatchEventKindNameV1] {
        &self.event_kinds
    }
    pub const fn repository(&self) -> Option<&RepositorySlug> {
        self.repository.as_ref()
    }
    pub const fn base_branch(&self) -> Option<&BranchName> {
        self.base_branch.as_ref()
    }
    pub const fn head_branch(&self) -> Option<&RepoWatchPattern> {
        self.head_branch.as_ref()
    }
    pub const fn title(&self) -> Option<&RepoWatchPattern> {
        self.title.as_ref()
    }
    pub const fn body(&self) -> Option<&RepoWatchPattern> {
        self.body.as_ref()
    }
    pub const fn labels(&self) -> &RepoWatchLabelMatcher {
        &self.labels
    }
    pub const fn draft(&self) -> Option<bool> {
        self.draft
    }
    pub const fn author(&self) -> Option<&RepoWatchAuthorLogin> {
        self.author.as_ref()
    }
    pub fn mergeable_state(&self) -> &[MergeableState] {
        &self.mergeable_state
    }
    pub fn conclusion(&self) -> &[CheckConclusion] {
        &self.conclusion
    }
}

/// Durable singleton key selected independently by each rule.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum RepoWatchSingletonScope {
    #[default]
    PullRequest,
    Stack,
    Rule,
    Repository,
}

impl RepoWatchSingletonScope {
    const fn as_str(self) -> &'static str {
        match self {
            Self::PullRequest => "pull_request",
            Self::Stack => "stack",
            Self::Rule => "rule",
            Self::Repository => "repo",
        }
    }
}

/// Context shape a session template explicitly accepts.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RepoWatchDispatchContextShape {
    PullRequest,
    Branch,
}

impl fmt::Display for RepoWatchDispatchContextShape {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::PullRequest => "pull-request",
            Self::Branch => "branch",
        })
    }
}

/// Template declaration of the repository-watch context shapes it accepts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchTemplateContextDeclaration {
    template: SessionTemplateName,
    accepted: Box<[RepoWatchDispatchContextShape]>,
}

/// Why a template's repository-watch context declaration was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchTemplateContextDeclarationError {
    NoAcceptedContextShape { template: SessionTemplateName },
}

impl fmt::Display for RepoWatchTemplateContextDeclarationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAcceptedContextShape { template } => write!(
                formatter,
                "repository-watch template {} accepts no dispatch context shape",
                template.as_str()
            ),
        }
    }
}

impl Error for RepoWatchTemplateContextDeclarationError {}

impl RepoWatchTemplateContextDeclaration {
    pub fn try_new(
        template: SessionTemplateName,
        accepted: Vec<RepoWatchDispatchContextShape>,
    ) -> Result<Self, RepoWatchTemplateContextDeclarationError> {
        if accepted.is_empty() {
            return Err(
                RepoWatchTemplateContextDeclarationError::NoAcceptedContextShape { template },
            );
        }
        Ok(Self {
            template,
            accepted: accepted.into_boxed_slice(),
        })
    }

    pub const fn template(&self) -> &SessionTemplateName {
        &self.template
    }

    pub fn accepted(&self) -> &[RepoWatchDispatchContextShape] {
        &self.accepted
    }

    pub fn accepts(&self, shape: RepoWatchDispatchContextShape) -> bool {
        self.accepted.contains(&shape)
    }
}

/// Pull-request-shaped parameters injected into one session dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequestContext {
    repository: RepositorySlug,
    number: PullRequestNumber,
    head_sha: CommitSha,
    event: RepoWatchEvent,
}

impl PullRequestContext {
    pub const fn repository(&self) -> &RepositorySlug {
        &self.repository
    }
    pub const fn number(&self) -> PullRequestNumber {
        self.number
    }
    pub const fn head_sha(&self) -> &CommitSha {
        &self.head_sha
    }
    pub const fn event(&self) -> &RepoWatchEvent {
        &self.event
    }
}

/// Branch-shaped parameters injected into one session dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchContext {
    repository: RepositorySlug,
    branch: BranchName,
    workflow: WorkflowName,
    conclusion: CheckConclusion,
    event: RepoWatchEvent,
}

impl BranchContext {
    pub const fn repository(&self) -> &RepositorySlug {
        &self.repository
    }
    pub const fn branch(&self) -> &BranchName {
        &self.branch
    }
    pub const fn workflow(&self) -> &WorkflowName {
        &self.workflow
    }
    pub const fn conclusion(&self) -> CheckConclusion {
        self.conclusion
    }
    pub const fn event(&self) -> &RepoWatchEvent {
        &self.event
    }
}

/// Tagged structured context injected into one session dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DispatchSessionParameters {
    PullRequest(PullRequestContext),
    Branch(BranchContext),
}

impl DispatchSessionParameters {
    pub fn try_from_event(event: RepoWatchEvent) -> Result<Self, RepoWatchDispatchContextError> {
        let repository = event.repository.clone();
        Ok(match &event.target {
            RepoWatchEventTarget::PullRequest(context) => Self::PullRequest(PullRequestContext {
                repository,
                number: context.number,
                head_sha: context.head_sha.clone(),
                event,
            }),
            RepoWatchEventTarget::Branch => {
                let RepoWatchEventKindV1::BranchWorkflowRunCompleted {
                    branch,
                    workflow,
                    conclusion,
                } = &event.kind
                else {
                    return Err(RepoWatchDispatchContextError::InvalidBranchEvent);
                };
                Self::Branch(BranchContext {
                    repository,
                    branch: branch.clone(),
                    workflow: workflow.clone(),
                    conclusion: *conclusion,
                    event,
                })
            }
        })
    }

    pub const fn shape(&self) -> RepoWatchDispatchContextShape {
        match self {
            Self::PullRequest(_) => RepoWatchDispatchContextShape::PullRequest,
            Self::Branch(_) => RepoWatchDispatchContextShape::Branch,
        }
    }

    pub const fn event(&self) -> &RepoWatchEvent {
        match self {
            Self::PullRequest(context) => context.event(),
            Self::Branch(context) => context.event(),
        }
    }
}

/// Why an event could not produce its sealed dispatch context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchDispatchContextError {
    InvalidBranchEvent,
}

impl fmt::Display for RepoWatchDispatchContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("branch repository-watch event has no branch-workflow payload")
    }
}

impl Error for RepoWatchDispatchContextError {}

/// Configured version-one rule action before a triggering event exists.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchRuleActionV1 {
    DispatchSession { template: SessionTemplateName },
}

impl RepoWatchRuleActionV1 {
    pub const fn template(&self) -> &SessionTemplateName {
        match self {
            Self::DispatchSession { template } => template,
        }
    }
}

/// Version-one session-dispatch action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DispatchSessionAction {
    template: SessionTemplateName,
    params: DispatchSessionParameters,
}

impl DispatchSessionAction {
    pub const fn new(template: SessionTemplateName, params: DispatchSessionParameters) -> Self {
        Self { template, params }
    }

    pub const fn template(&self) -> &SessionTemplateName {
        &self.template
    }
    pub const fn params(&self) -> &DispatchSessionParameters {
        &self.params
    }
}

/// Closed version-one action vocabulary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchActionV1 {
    DispatchSession(DispatchSessionAction),
}

/// One complete versioned repository-watch rule.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchRule {
    id: RepoWatchRuleId,
    version: RepoWatchRuleVersion,
    matcher: RepoWatchMatcherV1,
    actions: Box<[RepoWatchRuleActionV1]>,
    singleton_per: RepoWatchSingletonScope,
    cooldown: Duration,
}

/// Why one configured version-one rule was refused before runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchRuleValidationError {
    NoActions,
    BranchEventWithPullRequestSingleton {
        scope: RepoWatchSingletonScope,
    },
    TemplateNotDeclared {
        template: SessionTemplateName,
    },
    TemplateRejectsContext {
        template: SessionTemplateName,
        shape: RepoWatchDispatchContextShape,
    },
}

impl fmt::Display for RepoWatchRuleValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoActions => formatter.write_str("repository-watch rule has no actions"),
            Self::BranchEventWithPullRequestSingleton { scope } => write!(
                formatter,
                "repository-watch branch event cannot use `{}` singleton scope",
                scope.as_str()
            ),
            Self::TemplateNotDeclared { template } => write!(
                formatter,
                "repository-watch template `{}` has no context declaration",
                template.as_str()
            ),
            Self::TemplateRejectsContext { template, shape } => write!(
                formatter,
                "repository-watch template `{}` rejects `{shape}` context",
                template.as_str()
            ),
        }
    }
}

impl Error for RepoWatchRuleValidationError {}

impl RepoWatchRule {
    pub fn try_new(
        id: RepoWatchRuleId,
        matcher: RepoWatchMatcherV1,
        actions: Vec<RepoWatchRuleActionV1>,
        singleton_per: RepoWatchSingletonScope,
        cooldown: Duration,
    ) -> Result<Self, RepoWatchRuleValidationError> {
        if actions.is_empty() {
            return Err(RepoWatchRuleValidationError::NoActions);
        }
        if matcher.produces_branch_context() {
            match singleton_per {
                RepoWatchSingletonScope::PullRequest | RepoWatchSingletonScope::Stack => {
                    return Err(
                        RepoWatchRuleValidationError::BranchEventWithPullRequestSingleton {
                            scope: singleton_per,
                        },
                    );
                }
                RepoWatchSingletonScope::Rule | RepoWatchSingletonScope::Repository => {}
            }
        }
        Ok(Self {
            id,
            version: RepoWatchRuleVersion::V1,
            matcher,
            actions: actions.into_boxed_slice(),
            singleton_per,
            cooldown,
        })
    }

    pub const fn id(&self) -> &RepoWatchRuleId {
        &self.id
    }
    pub const fn version(&self) -> RepoWatchRuleVersion {
        self.version
    }
    pub const fn matcher(&self) -> &RepoWatchMatcherV1 {
        &self.matcher
    }
    pub fn actions(&self) -> &[RepoWatchRuleActionV1] {
        &self.actions
    }
    pub const fn singleton_per(&self) -> RepoWatchSingletonScope {
        self.singleton_per
    }
    pub const fn cooldown(&self) -> Duration {
        self.cooldown
    }

    pub fn required_context_shapes(&self) -> Vec<RepoWatchDispatchContextShape> {
        let branch = self.matcher.produces_branch_context();
        let pull_request = self.matcher.produces_pull_request_context();
        match (pull_request, branch) {
            (true, true) => vec![
                RepoWatchDispatchContextShape::PullRequest,
                RepoWatchDispatchContextShape::Branch,
            ],
            (true, false) => vec![RepoWatchDispatchContextShape::PullRequest],
            (false, true) => vec![RepoWatchDispatchContextShape::Branch],
            (false, false) => Vec::new(),
        }
    }

    pub fn validate_template_contexts(
        &self,
        declarations: &[RepoWatchTemplateContextDeclaration],
    ) -> Result<(), RepoWatchRuleValidationError> {
        let required = self.required_context_shapes();
        for action in &self.actions {
            let template = action.template();
            let declaration = declarations
                .iter()
                .find(|declaration| declaration.template() == template)
                .ok_or_else(|| RepoWatchRuleValidationError::TemplateNotDeclared {
                    template: template.clone(),
                })?;
            for shape in &required {
                if !declaration.accepts(*shape) {
                    return Err(RepoWatchRuleValidationError::TemplateRejectsContext {
                        template: template.clone(),
                        shape: *shape,
                    });
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, num::NonZeroU64, time::Duration};

    use uuid::Uuid;

    use crate::{RepoWatchEventId, SessionTemplateName};

    use super::{
        BranchName, CheckConclusion, CommitSha, LabelName, MergeableState, PullRequestBody,
        PullRequestEventContext, PullRequestEventContextInput, PullRequestNumber, PullRequestTitle,
        RepoWatchAuthorLogin, RepoWatchDispatchContextShape, RepoWatchEvent,
        RepoWatchEventConstructionError, RepoWatchEventKindNameV1, RepoWatchEventKindV1,
        RepoWatchLabelMatcher, RepoWatchLabelMatcherInput, RepoWatchMatcherV1,
        RepoWatchMatcherV1Input, RepoWatchPattern, RepoWatchRule, RepoWatchRuleActionV1,
        RepoWatchRuleId, RepoWatchRuleValidationError, RepoWatchSingletonScope,
        RepoWatchTemplateContextDeclaration, RepoWatchTemplateContextDeclarationError,
        RepoWatchTextError, RepositorySlug,
    };

    const CONTEXT_HEAD_SHA: &str = "1111111111111111111111111111111111111111";
    const EVENT_HEAD_SHA: &str = "2222222222222222222222222222222222222222";
    const PREVIOUS_HEAD_SHA: &str = "3333333333333333333333333333333333333333";
    const VALID_MULTIBYTE_LABEL: &str = "😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀";
    const TOO_MANY_LABEL_CHARACTERS: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// Builds canonical PR context while exposing only coherence-relevant facts.
    fn pull_request_context(
        head_sha: CommitSha,
        base_branch: BranchName,
        labels: Vec<LabelName>,
    ) -> Result<PullRequestEventContext, RepoWatchTextError> {
        Ok(PullRequestEventContext::new(PullRequestEventContextInput {
            number: PullRequestNumber::new(NonZeroU64::MIN),
            head_sha,
            head_repository: RepositorySlug::try_new(String::from("namespace/repo"))?,
            base_branch,
            head_branch: BranchName::try_new(String::from("topic/watch"))?,
            title: PullRequestTitle::try_new(String::from("Watch repositories"))?,
            body: PullRequestBody::try_new(String::new())?,
            labels,
            draft: false,
            author: Some(RepoWatchAuthorLogin::try_new(String::from("maintainer"))?),
        }))
    }

    #[test]
    fn repository_slug_requires_exact_namespace_and_name() -> Result<(), RepoWatchTextError> {
        assert_eq!(
            RepositorySlug::try_new(String::from("namespace")),
            Err(RepoWatchTextError::Malformed)
        );
        assert_eq!(
            RepositorySlug::try_new(String::from("NameSpace/Repo"))?.as_str(),
            "namespace/repo"
        );
        Ok(())
    }

    #[test]
    fn repository_slug_rejects_invalid_segment_characters() {
        assert_eq!(
            RepositorySlug::try_new(String::from("namespace/bad repo")),
            Err(RepoWatchTextError::Malformed)
        );
        assert_eq!(
            RepositorySlug::try_new(String::from("../repo")),
            Err(RepoWatchTextError::Malformed)
        );
    }

    #[test]
    fn actor_login_accepts_human_managed_and_app_bot_forms() {
        assert!(RepoWatchAuthorLogin::try_new(String::from("maintainer-name")).is_ok());
        assert!(RepoWatchAuthorLogin::try_new(String::from("maintainer_SHORT")).is_ok());
        assert!(RepoWatchAuthorLogin::try_new(String::from("github-actions[bot]")).is_ok());
    }

    #[test]
    fn actor_login_canonicalizes_case_insensitive_identity() -> Result<(), RepoWatchTextError> {
        assert_eq!(
            RepoWatchAuthorLogin::try_new(String::from("GitHub-Actions[BOT]"))?.as_str(),
            "github-actions[bot]"
        );
        Ok(())
    }

    #[test]
    fn actor_login_rejects_malformed_provider_values() {
        assert_eq!(
            RepoWatchAuthorLogin::try_new(String::from("bad login")),
            Err(RepoWatchTextError::Malformed)
        );
        assert_eq!(
            RepoWatchAuthorLogin::try_new(String::from("bad\nlogin")),
            Err(RepoWatchTextError::Malformed)
        );
        assert_eq!(
            RepoWatchAuthorLogin::try_new(String::from("-bad")),
            Err(RepoWatchTextError::Malformed)
        );
        assert_eq!(
            RepoWatchAuthorLogin::try_new(String::from("bad--login")),
            Err(RepoWatchTextError::Malformed)
        );
    }

    #[test]
    fn rule_identity_rejects_whitespace_and_control_characters() {
        assert_eq!(
            RepoWatchRuleId::try_new(String::from("bad rule")),
            Err(RepoWatchTextError::Malformed)
        );
        assert_eq!(
            RepoWatchRuleId::try_new(String::from("bad\nrule")),
            Err(RepoWatchTextError::Malformed)
        );
    }

    #[test]
    fn label_name_admits_valid_multibyte_characters_beyond_one_hundred_bytes() {
        assert!(VALID_MULTIBYTE_LABEL.len() > 100);
        assert!(LabelName::try_new(String::from(VALID_MULTIBYTE_LABEL)).is_ok());
    }

    #[test]
    fn label_name_rejects_more_than_fifty_characters() {
        assert_eq!(TOO_MANY_LABEL_CHARACTERS.chars().count(), 51);
        assert_eq!(
            LabelName::try_new(String::from(TOO_MANY_LABEL_CHARACTERS)),
            Err(RepoWatchTextError::TooManyCharacters {
                characters: 51,
                maximum: 50,
            })
        );
    }

    #[test]
    fn matcher_regex_requires_explicit_anchors() {
        assert_eq!(
            RepoWatchPattern::try_new(String::from("topic/.*")),
            Err(RepoWatchTextError::UnanchoredPattern)
        );
    }

    #[test]
    fn matcher_regex_anchors_the_complete_alternation() -> Result<(), RepoWatchTextError> {
        let pattern = RepoWatchPattern::try_new(String::from("^release|hotfix$"))?;

        assert!(pattern.is_match("release"));
        assert!(pattern.is_match("hotfix"));
        assert!(!pattern.is_match("release-candidate"));
        assert!(!pattern.is_match("emergency-hotfix"));
        Ok(())
    }

    #[test]
    fn matcher_regex_supports_unicode_properties_and_case_folding() -> Result<(), RepoWatchTextError>
    {
        let property = RepoWatchPattern::try_new(String::from(r"^\p{Greek}+$"))?;
        let case_fold = RepoWatchPattern::try_new(String::from("^(?i:café)$"))?;

        assert!(property.is_match("Δοκιμή"));
        assert!(case_fold.is_match("CAFÉ"));
        Ok(())
    }

    #[test]
    fn matcher_regex_preserves_compiler_diagnostics() {
        let error = RepoWatchPattern::try_new(String::from("^(?=value)value$"))
            .expect_err("look-around must remain unsupported");

        assert!(error.to_string().contains("look-around"));
    }

    #[test]
    fn branch_name_rejects_invalid_git_ref_shapes() {
        assert_eq!(
            BranchName::try_new(String::from("bad..branch")),
            Err(RepoWatchTextError::Malformed)
        );
        assert_eq!(
            BranchName::try_new(String::from("bad branch")),
            Err(RepoWatchTextError::Malformed)
        );
        assert_eq!(
            BranchName::try_new(String::from("component.lock")),
            Err(RepoWatchTextError::Malformed)
        );
    }

    #[test]
    fn payload_qualifiers_remain_fields_separate_from_event_kinds() {
        let mergeable_state = vec![MergeableState::Conflicting];
        let conclusion = vec![CheckConclusion::Failure];
        let matcher = RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::MergeableStateChanged],
            mergeable_state: mergeable_state.clone(),
            conclusion: conclusion.clone(),
            ..RepoWatchMatcherV1Input::default()
        });

        assert_eq!(matcher.mergeable_state(), mergeable_state);
        assert_eq!(matcher.conclusion(), conclusion);
    }

    #[test]
    fn label_matcher_construction_keeps_predicates_named() -> Result<(), RepoWatchTextError> {
        let any_of = LabelName::try_new(String::from("any"))?;
        let all_of = LabelName::try_new(String::from("all"))?;
        let none_of = LabelName::try_new(String::from("none"))?;
        let matcher = RepoWatchLabelMatcher::new(RepoWatchLabelMatcherInput {
            any_of: vec![any_of.clone()],
            all_of: vec![all_of.clone()],
            none_of: vec![none_of.clone()],
        });

        assert_eq!(matcher.any_of(), [any_of]);
        assert_eq!(matcher.all_of(), [all_of]);
        assert_eq!(matcher.none_of(), [none_of]);
        Ok(())
    }

    #[test]
    fn branch_event_produces_tagged_branch_context() -> Result<(), Box<dyn Error>> {
        let repository = RepositorySlug::try_new(String::from("namespace/repo"))?;
        let branch = BranchName::try_new(String::from("main"))?;
        let conclusion = CheckConclusion::Failure;
        let event = RepoWatchEvent::branch_workflow(
            RepoWatchEventId::from_uuid(Uuid::from_u128(1)),
            repository,
            branch,
            super::WorkflowName::try_new(String::from("ci"))?,
            conclusion,
        );

        let parameters = super::DispatchSessionParameters::try_from_event(event.clone())?;

        assert_eq!(parameters.shape(), RepoWatchDispatchContextShape::Branch);
        assert_eq!(parameters.event(), &event);
        Ok(())
    }

    #[test]
    fn pull_request_event_produces_tagged_pull_request_context() -> Result<(), Box<dyn Error>> {
        let repository = RepositorySlug::try_new(String::from("namespace/repo"))?;
        let number = PullRequestNumber::new(NonZeroU64::MIN);
        let head_sha =
            super::CommitSha::try_new(String::from("0123456789abcdef0123456789abcdef01234567"))?;
        let context = PullRequestEventContext::new(PullRequestEventContextInput {
            number,
            head_sha,
            head_repository: RepositorySlug::try_new(String::from("namespace/repo"))?,
            base_branch: BranchName::try_new(String::from("main"))?,
            head_branch: BranchName::try_new(String::from("topic/watch"))?,
            title: PullRequestTitle::try_new(String::from("Watch repositories"))?,
            body: PullRequestBody::try_new(String::new())?,
            labels: Vec::new(),
            draft: false,
            author: Some(RepoWatchAuthorLogin::try_new(String::from("maintainer"))?),
        });
        let event = RepoWatchEvent::try_pull_request(
            RepoWatchEventId::from_uuid(Uuid::from_u128(2)),
            repository,
            context,
            RepoWatchEventKindV1::PullRequestOpened,
        )?;

        let parameters = super::DispatchSessionParameters::try_from_event(event.clone())?;

        assert_eq!(
            parameters.shape(),
            RepoWatchDispatchContextShape::PullRequest
        );
        assert_eq!(parameters.event(), &event);
        Ok(())
    }

    #[test]
    fn pull_request_context_retains_head_repository_and_missing_author()
    -> Result<(), Box<dyn Error>> {
        let head_repository = RepositorySlug::try_new(String::from("fork-source/repo"))?;
        let context = PullRequestEventContext::new(PullRequestEventContextInput {
            number: PullRequestNumber::new(NonZeroU64::MIN),
            head_sha: CommitSha::try_new(String::from(CONTEXT_HEAD_SHA))?,
            head_repository: head_repository.clone(),
            base_branch: BranchName::try_new(String::from("main"))?,
            head_branch: BranchName::try_new(String::from("topic/watch"))?,
            title: PullRequestTitle::try_new(String::from("Watch repositories"))?,
            body: PullRequestBody::try_new(String::new())?,
            labels: Vec::new(),
            draft: false,
            author: None,
        });

        assert_eq!(context.head_repository(), &head_repository);
        assert_eq!(context.author(), None);
        Ok(())
    }

    #[test]
    fn pull_request_context_canonicalizes_its_complete_label_set() -> Result<(), Box<dyn Error>> {
        let first = LabelName::try_new(String::from("first"))?;
        let second = LabelName::try_new(String::from("second"))?;
        let context = pull_request_context(
            CommitSha::try_new(String::from(CONTEXT_HEAD_SHA))?,
            BranchName::try_new(String::from("main"))?,
            vec![second.clone(), first.clone(), second.clone()],
        )?;

        assert_eq!(context.labels(), [first, second]);
        Ok(())
    }

    #[test]
    fn head_changed_current_must_equal_context_head() -> Result<(), Box<dyn Error>> {
        let context = pull_request_context(
            CommitSha::try_new(String::from(CONTEXT_HEAD_SHA))?,
            BranchName::try_new(String::from("main"))?,
            Vec::new(),
        )?;
        let result = RepoWatchEvent::try_pull_request(
            RepoWatchEventId::from_uuid(Uuid::nil()),
            RepositorySlug::try_new(String::from("namespace/repo"))?,
            context,
            RepoWatchEventKindV1::HeadChanged {
                previous: CommitSha::try_new(String::from(PREVIOUS_HEAD_SHA))?,
                current: CommitSha::try_new(String::from(EVENT_HEAD_SHA))?,
            },
        );

        assert_eq!(
            result,
            Err(RepoWatchEventConstructionError::HeadChangedCurrentMismatch)
        );
        Ok(())
    }

    #[test]
    fn head_changed_previous_must_differ_from_current() -> Result<(), Box<dyn Error>> {
        let head = CommitSha::try_new(String::from(CONTEXT_HEAD_SHA))?;
        let context = pull_request_context(
            head.clone(),
            BranchName::try_new(String::from("main"))?,
            Vec::new(),
        )?;
        let result = RepoWatchEvent::try_pull_request(
            RepoWatchEventId::from_uuid(Uuid::nil()),
            RepositorySlug::try_new(String::from("namespace/repo"))?,
            context,
            RepoWatchEventKindV1::HeadChanged {
                previous: head.clone(),
                current: head,
            },
        );

        assert_eq!(
            result,
            Err(RepoWatchEventConstructionError::HeadChangedWithoutChange)
        );
        Ok(())
    }

    #[test]
    fn base_advanced_branch_must_equal_context_base() -> Result<(), Box<dyn Error>> {
        let context = pull_request_context(
            CommitSha::try_new(String::from(CONTEXT_HEAD_SHA))?,
            BranchName::try_new(String::from("main"))?,
            Vec::new(),
        )?;
        let result = RepoWatchEvent::try_pull_request(
            RepoWatchEventId::from_uuid(Uuid::nil()),
            RepositorySlug::try_new(String::from("namespace/repo"))?,
            context,
            RepoWatchEventKindV1::BaseAdvanced {
                branch: BranchName::try_new(String::from("release"))?,
            },
        );

        assert_eq!(
            result,
            Err(RepoWatchEventConstructionError::BaseAdvancedBranchMismatch)
        );
        Ok(())
    }

    #[test]
    fn labeled_event_label_must_be_present_in_context() -> Result<(), Box<dyn Error>> {
        let label = LabelName::try_new(String::from("ready"))?;
        let context = pull_request_context(
            CommitSha::try_new(String::from(CONTEXT_HEAD_SHA))?,
            BranchName::try_new(String::from("main"))?,
            Vec::new(),
        )?;
        let result = RepoWatchEvent::try_pull_request(
            RepoWatchEventId::from_uuid(Uuid::nil()),
            RepositorySlug::try_new(String::from("namespace/repo"))?,
            context,
            RepoWatchEventKindV1::Labeled { label },
        );

        assert_eq!(
            result,
            Err(RepoWatchEventConstructionError::LabeledContextMissingLabel)
        );
        Ok(())
    }

    #[test]
    fn unlabeled_event_label_must_be_absent_from_context() -> Result<(), Box<dyn Error>> {
        let label = LabelName::try_new(String::from("ready"))?;
        let context = pull_request_context(
            CommitSha::try_new(String::from(CONTEXT_HEAD_SHA))?,
            BranchName::try_new(String::from("main"))?,
            vec![label.clone()],
        )?;
        let result = RepoWatchEvent::try_pull_request(
            RepoWatchEventId::from_uuid(Uuid::nil()),
            RepositorySlug::try_new(String::from("namespace/repo"))?,
            context,
            RepoWatchEventKindV1::Unlabeled { label },
        );

        assert_eq!(
            result,
            Err(RepoWatchEventConstructionError::UnlabeledContextContainsLabel)
        );
        Ok(())
    }

    #[test]
    fn rule_rejects_an_empty_action_list() -> Result<(), RepoWatchTextError> {
        let result = RepoWatchRule::try_new(
            RepoWatchRuleId::try_new(String::from("no-actions"))?,
            RepoWatchMatcherV1::default(),
            Vec::new(),
            RepoWatchSingletonScope::PullRequest,
            Duration::ZERO,
        );

        assert_eq!(result, Err(RepoWatchRuleValidationError::NoActions));
        Ok(())
    }

    #[test]
    fn branch_event_rejects_pull_request_singleton_scope() -> Result<(), Box<dyn Error>> {
        let template = SessionTemplateName::try_new(String::from("branch-handler"))?;
        let result = RepoWatchRule::try_new(
            RepoWatchRuleId::try_new(String::from("invalid-branch-scope"))?,
            RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
                event_kinds: vec![RepoWatchEventKindNameV1::BranchWorkflowRunCompleted],
                ..RepoWatchMatcherV1Input::default()
            }),
            vec![RepoWatchRuleActionV1::DispatchSession { template }],
            RepoWatchSingletonScope::PullRequest,
            Duration::ZERO,
        );

        assert_eq!(
            result,
            Err(
                RepoWatchRuleValidationError::BranchEventWithPullRequestSingleton {
                    scope: RepoWatchSingletonScope::PullRequest,
                }
            )
        );
        Ok(())
    }

    #[test]
    fn empty_event_matcher_rejects_stack_singleton_scope() -> Result<(), Box<dyn Error>> {
        let template = SessionTemplateName::try_new(String::from("event-handler"))?;
        let result = RepoWatchRule::try_new(
            RepoWatchRuleId::try_new(String::from("invalid-everything-scope"))?,
            RepoWatchMatcherV1::default(),
            vec![RepoWatchRuleActionV1::DispatchSession { template }],
            RepoWatchSingletonScope::Stack,
            Duration::ZERO,
        );

        assert_eq!(
            result,
            Err(
                RepoWatchRuleValidationError::BranchEventWithPullRequestSingleton {
                    scope: RepoWatchSingletonScope::Stack,
                }
            )
        );
        Ok(())
    }

    #[test]
    fn mergeable_qualifier_with_all_kinds_produces_only_pull_request_context()
    -> Result<(), Box<dyn Error>> {
        let template = SessionTemplateName::try_new(String::from("conflict-handler"))?;
        let rule = RepoWatchRule::try_new(
            RepoWatchRuleId::try_new(String::from("mergeable-only"))?,
            RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
                mergeable_state: vec![MergeableState::Conflicting],
                ..RepoWatchMatcherV1Input::default()
            }),
            vec![RepoWatchRuleActionV1::DispatchSession { template }],
            RepoWatchSingletonScope::PullRequest,
            Duration::ZERO,
        )?;

        assert_eq!(
            rule.required_context_shapes(),
            [RepoWatchDispatchContextShape::PullRequest]
        );
        Ok(())
    }

    #[test]
    fn pull_request_matcher_fields_exclude_branch_context() -> Result<(), Box<dyn Error>> {
        let template = SessionTemplateName::try_new(String::from("author-handler"))?;
        let rule = RepoWatchRule::try_new(
            RepoWatchRuleId::try_new(String::from("author-only"))?,
            RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
                author: Some(RepoWatchAuthorLogin::try_new(String::from("maintainer"))?),
                ..RepoWatchMatcherV1Input::default()
            }),
            vec![RepoWatchRuleActionV1::DispatchSession { template }],
            RepoWatchSingletonScope::PullRequest,
            Duration::ZERO,
        )?;

        assert_eq!(
            rule.required_context_shapes(),
            [RepoWatchDispatchContextShape::PullRequest]
        );
        Ok(())
    }

    #[test]
    fn conclusion_qualifier_with_all_kinds_produces_both_context_shapes()
    -> Result<(), Box<dyn Error>> {
        let template = SessionTemplateName::try_new(String::from("failure-handler"))?;
        let rule = RepoWatchRule::try_new(
            RepoWatchRuleId::try_new(String::from("conclusion-only"))?,
            RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
                conclusion: vec![CheckConclusion::Failure],
                ..RepoWatchMatcherV1Input::default()
            }),
            vec![RepoWatchRuleActionV1::DispatchSession { template }],
            RepoWatchSingletonScope::Repository,
            Duration::ZERO,
        )?;

        assert_eq!(
            rule.required_context_shapes(),
            [
                RepoWatchDispatchContextShape::PullRequest,
                RepoWatchDispatchContextShape::Branch,
            ]
        );
        Ok(())
    }

    #[test]
    fn aggregate_checks_exclude_unavailable_conclusion_values() -> Result<(), Box<dyn Error>> {
        let template = SessionTemplateName::try_new(String::from("neutral-handler"))?;
        let rule = RepoWatchRule::try_new(
            RepoWatchRuleId::try_new(String::from("neutral-aggregate"))?,
            RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
                event_kinds: vec![RepoWatchEventKindNameV1::ChecksCompleted],
                conclusion: vec![CheckConclusion::Neutral],
                ..RepoWatchMatcherV1Input::default()
            }),
            vec![RepoWatchRuleActionV1::DispatchSession { template }],
            RepoWatchSingletonScope::PullRequest,
            Duration::ZERO,
        )?;

        assert!(rule.required_context_shapes().is_empty());
        Ok(())
    }

    #[test]
    fn rule_validation_rejects_a_template_context_mismatch() -> Result<(), Box<dyn Error>> {
        let template = SessionTemplateName::try_new(String::from("branch-handler"))?;
        let rule = RepoWatchRule::try_new(
            RepoWatchRuleId::try_new(String::from("branch-failure"))?,
            RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
                event_kinds: vec![RepoWatchEventKindNameV1::BranchWorkflowRunCompleted],
                conclusion: vec![CheckConclusion::Failure],
                ..RepoWatchMatcherV1Input::default()
            }),
            vec![RepoWatchRuleActionV1::DispatchSession {
                template: template.clone(),
            }],
            RepoWatchSingletonScope::Repository,
            Duration::ZERO,
        )?;
        let declarations = [RepoWatchTemplateContextDeclaration::try_new(
            template.clone(),
            vec![RepoWatchDispatchContextShape::PullRequest],
        )?];

        assert_eq!(
            rule.validate_template_contexts(&declarations),
            Err(RepoWatchRuleValidationError::TemplateRejectsContext {
                template,
                shape: RepoWatchDispatchContextShape::Branch,
            })
        );
        Ok(())
    }

    #[test]
    fn template_context_declaration_rejects_an_empty_shape_list() -> Result<(), Box<dyn Error>> {
        let template = SessionTemplateName::try_new(String::from("empty-context"))?;

        assert_eq!(
            RepoWatchTemplateContextDeclaration::try_new(template.clone(), Vec::new()),
            Err(
                RepoWatchTemplateContextDeclarationError::NoAcceptedContextShape {
                    template: template.clone(),
                }
            )
        );
        assert!(
            RepoWatchTemplateContextDeclarationError::NoAcceptedContextShape { template }
                .to_string()
                .contains("empty-context")
        );
        Ok(())
    }

    #[test]
    fn rule_validation_diagnostics_retain_template_and_shape() -> Result<(), Box<dyn Error>> {
        let template = SessionTemplateName::try_new(String::from("branch-handler"))?;
        let missing_declaration = RepoWatchRuleValidationError::TemplateNotDeclared {
            template: template.clone(),
        }
        .to_string();
        let rejected_context = RepoWatchRuleValidationError::TemplateRejectsContext {
            template: template.clone(),
            shape: RepoWatchDispatchContextShape::Branch,
        }
        .to_string();

        assert!(missing_declaration.contains(template.as_str()));
        assert!(rejected_context.contains(template.as_str()));
        assert!(rejected_context.contains(&RepoWatchDispatchContextShape::Branch.to_string()));
        Ok(())
    }
}
