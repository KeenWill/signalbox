//! Repository-watch events, matchers, and dispatch action values.
//!
//! The normative cross-component contract is `docs/spec/repo-watch.md`.

use std::{error::Error, fmt, num::NonZeroU64, time::Duration};

use regex::Regex;

use crate::{RepoWatchEventId, SessionTemplateName};

const MAX_REPOSITORY_BYTES: usize = 201;
const MAX_BRANCH_BYTES: usize = 255;
const MAX_LOGIN_BYTES: usize = 39;
const MAX_LABEL_BYTES: usize = 100;
const MAX_NAME_BYTES: usize = 256;
const MAX_REACTION_BYTES: usize = 64;
const MAX_RULE_ID_BYTES: usize = 128;
const MAX_PATTERN_BYTES: usize = 1_024;
const MAX_TITLE_BYTES: usize = 1_024;
const MAX_BODY_BYTES: usize = 262_144;

/// Why one repository-watch text value was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchTextError {
    Empty,
    ContainsNull,
    TooLong { bytes: usize, maximum: usize },
    Malformed,
    UnanchoredPattern,
    InvalidPattern,
}

impl fmt::Display for RepoWatchTextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "repository-watch value is empty",
            Self::ContainsNull => "repository-watch value contains U+0000",
            Self::TooLong { .. } => "repository-watch value exceeds its byte bound",
            Self::Malformed => "repository-watch value has an invalid shape",
            Self::UnanchoredPattern => "repository-watch regex must be anchored with ^ and $",
            Self::InvalidPattern => "repository-watch regex is invalid",
        })
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
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError> {
        validate_text(&value, MAX_REPOSITORY_BYTES)?;
        let mut parts = value.split('/');
        let namespace = parts.next().unwrap_or_default();
        let repository = parts.next().unwrap_or_default();
        if namespace.is_empty() || repository.is_empty() || parts.next().is_some() {
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

bounded_text!(/// One exact repository branch name.
    BranchName, MAX_BRANCH_BYTES);
bounded_text!(/// One exact repository label name.
    LabelName, MAX_LABEL_BYTES);
bounded_text!(/// One GitHub actor login.
    RepoWatchAuthorLogin, MAX_LOGIN_BYTES);
bounded_text!(/// One check-run name.
    CheckRunName, MAX_NAME_BYTES);
bounded_text!(/// One workflow name.
    WorkflowName, MAX_NAME_BYTES);
bounded_text!(/// One reaction content spelling retained as event evidence.
    ReactionContent, MAX_REACTION_BYTES);
bounded_text!(/// One stable operator-defined rule name.
    RepoWatchRuleId, MAX_RULE_ID_BYTES);
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
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RepoWatchPattern(String);

impl RepoWatchPattern {
    pub const MAX_UTF8_BYTES: usize = MAX_PATTERN_BYTES;

    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError> {
        validate_text(&value, MAX_PATTERN_BYTES)?;
        if !value.starts_with('^') || !value.ends_with('$') {
            return Err(RepoWatchTextError::UnanchoredPattern);
        }
        Regex::new(&value).map_err(|_| RepoWatchTextError::InvalidPattern)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_match(&self, candidate: &str) -> bool {
        Regex::new(&self.0).is_ok_and(|pattern| pattern.is_match(candidate))
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
    Dismissed,
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
    base_branch: BranchName,
    head_branch: BranchName,
    title: PullRequestTitle,
    body: PullRequestBody,
    labels: Box<[LabelName]>,
    draft: bool,
    author: RepoWatchAuthorLogin,
}

impl PullRequestEventContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        number: PullRequestNumber,
        head_sha: CommitSha,
        base_branch: BranchName,
        head_branch: BranchName,
        title: PullRequestTitle,
        body: PullRequestBody,
        labels: Vec<LabelName>,
        draft: bool,
        author: RepoWatchAuthorLogin,
    ) -> Self {
        Self {
            number,
            head_sha,
            base_branch,
            head_branch,
            title,
            body,
            labels: labels.into_boxed_slice(),
            draft,
            author,
        }
    }

    pub const fn number(&self) -> PullRequestNumber {
        self.number
    }
    pub const fn head_sha(&self) -> &CommitSha {
        &self.head_sha
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
    pub const fn author(&self) -> &RepoWatchAuthorLogin {
        &self.author
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
        if matches!(
            kind,
            RepoWatchEventKindV1::BranchWorkflowRunCompleted { .. }
        ) {
            return Err(RepoWatchEventConstructionError::BranchKindOnPullRequest);
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
}

impl fmt::Display for RepoWatchEventConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("branch-workflow event cannot carry a pull-request target")
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

impl RepoWatchLabelMatcher {
    pub fn new(any_of: Vec<LabelName>, all_of: Vec<LabelName>, none_of: Vec<LabelName>) -> Self {
        Self {
            any_of: any_of.into_boxed_slice(),
            all_of: all_of.into_boxed_slice(),
            none_of: none_of.into_boxed_slice(),
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

impl RepoWatchMatcherV1 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        event_kinds: Vec<RepoWatchEventKindNameV1>,
        repository: Option<RepositorySlug>,
        base_branch: Option<BranchName>,
        head_branch: Option<RepoWatchPattern>,
        title: Option<RepoWatchPattern>,
        body: Option<RepoWatchPattern>,
        labels: RepoWatchLabelMatcher,
        draft: Option<bool>,
        author: Option<RepoWatchAuthorLogin>,
        mergeable_state: Vec<MergeableState>,
        conclusion: Vec<CheckConclusion>,
    ) -> Self {
        Self {
            event_kinds: event_kinds.into_boxed_slice(),
            repository,
            base_branch,
            head_branch,
            title,
            body,
            labels,
            draft,
            author,
            mergeable_state: mergeable_state.into_boxed_slice(),
            conclusion: conclusion.into_boxed_slice(),
        }
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

/// Context shape a session template explicitly accepts.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RepoWatchDispatchContextShape {
    PullRequest,
    Branch,
}

/// Template declaration of the repository-watch context shapes it accepts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchTemplateContextDeclaration {
    template: SessionTemplateName,
    accepted: Box<[RepoWatchDispatchContextShape]>,
}

impl RepoWatchTemplateContextDeclaration {
    pub fn new(
        template: SessionTemplateName,
        accepted: Vec<RepoWatchDispatchContextShape>,
    ) -> Self {
        Self {
            template,
            accepted: accepted.into_boxed_slice(),
        }
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
        Ok(match event.target.clone() {
            RepoWatchEventTarget::PullRequest(context) => Self::PullRequest(PullRequestContext {
                repository,
                number: context.number,
                head_sha: context.head_sha,
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
        formatter.write_str(match self {
            Self::NoActions => "repository-watch rule has no actions",
            Self::TemplateNotDeclared { .. } => {
                "repository-watch action names a template without a context declaration"
            }
            Self::TemplateRejectsContext { .. } => {
                "repository-watch action template rejects an event context shape"
            }
        })
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
        let branch = self.matcher.event_kinds.is_empty()
            || self
                .matcher
                .event_kinds
                .contains(&RepoWatchEventKindNameV1::BranchWorkflowRunCompleted);
        let pull_request = self.matcher.event_kinds.is_empty()
            || self
                .matcher
                .event_kinds
                .iter()
                .any(|kind| *kind != RepoWatchEventKindNameV1::BranchWorkflowRunCompleted);
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
        BranchName, CheckConclusion, MergeableState, PullRequestBody, PullRequestEventContext,
        PullRequestNumber, PullRequestTitle, RepoWatchAuthorLogin, RepoWatchDispatchContextShape,
        RepoWatchEvent, RepoWatchEventKindNameV1, RepoWatchEventKindV1, RepoWatchLabelMatcher,
        RepoWatchMatcherV1, RepoWatchPattern, RepoWatchRule, RepoWatchRuleActionV1,
        RepoWatchRuleId, RepoWatchRuleValidationError, RepoWatchSingletonScope,
        RepoWatchTemplateContextDeclaration, RepoWatchTextError, RepositorySlug,
    };

    #[test]
    fn repository_slug_requires_exact_namespace_and_name() {
        assert_eq!(
            RepositorySlug::try_new(String::from("namespace")),
            Err(RepoWatchTextError::Malformed)
        );
        assert!(RepositorySlug::try_new(String::from("namespace/repo")).is_ok());
    }

    #[test]
    fn matcher_regex_is_anchored_and_linear_time() -> Result<(), RepoWatchTextError> {
        assert_eq!(
            RepoWatchPattern::try_new(String::from("topic/.*")),
            Err(RepoWatchTextError::UnanchoredPattern)
        );
        let pattern = RepoWatchPattern::try_new(String::from("^topic/[a-z]+$"))?;
        assert!(pattern.is_match("topic/watch"));
        assert!(!pattern.is_match("other/watch"));
        Ok(())
    }

    #[test]
    fn payload_qualifiers_remain_fields_separate_from_event_kinds() {
        let mergeable_state = vec![MergeableState::Conflicting];
        let conclusion = vec![CheckConclusion::Failure];
        let matcher = RepoWatchMatcherV1::new(
            vec![RepoWatchEventKindNameV1::MergeableStateChanged],
            None,
            None,
            None,
            None,
            None,
            RepoWatchLabelMatcher::default(),
            None,
            None,
            mergeable_state.clone(),
            conclusion.clone(),
        );

        assert_eq!(matcher.mergeable_state(), mergeable_state);
        assert_eq!(matcher.conclusion(), conclusion);
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
        let context = PullRequestEventContext::new(
            number,
            head_sha,
            BranchName::try_new(String::from("main"))?,
            BranchName::try_new(String::from("topic/watch"))?,
            PullRequestTitle::try_new(String::from("Watch repositories"))?,
            PullRequestBody::try_new(String::new())?,
            Vec::new(),
            false,
            RepoWatchAuthorLogin::try_new(String::from("maintainer"))?,
        );
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
    fn rule_validation_rejects_a_template_context_mismatch() -> Result<(), Box<dyn Error>> {
        let template = SessionTemplateName::try_new(String::from("branch-handler"))?;
        let rule = RepoWatchRule::try_new(
            RepoWatchRuleId::try_new(String::from("branch-failure"))?,
            RepoWatchMatcherV1::new(
                vec![RepoWatchEventKindNameV1::BranchWorkflowRunCompleted],
                None,
                None,
                None,
                None,
                None,
                RepoWatchLabelMatcher::default(),
                None,
                None,
                Vec::new(),
                vec![CheckConclusion::Failure],
            ),
            vec![RepoWatchRuleActionV1::DispatchSession {
                template: template.clone(),
            }],
            RepoWatchSingletonScope::Repository,
            Duration::ZERO,
        )?;
        let declarations = [RepoWatchTemplateContextDeclaration::new(
            template.clone(),
            vec![RepoWatchDispatchContextShape::PullRequest],
        )];

        assert_eq!(
            rule.validate_template_contexts(&declarations),
            Err(RepoWatchRuleValidationError::TemplateRejectsContext {
                template,
                shape: RepoWatchDispatchContextShape::Branch,
            })
        );
        Ok(())
    }
}
