//! Repository watch matcher for `docs/spec/repo-watch.md`.

use super::dispatch::RepoWatchDispatchContextShape;
use super::event::{
    CheckConclusion, MergeableState, PullRequestEventContext, RepoWatchEvent,
    RepoWatchEventKindNameV1, RepoWatchEventKindV1, RepoWatchEventTarget,
};
use super::values::{
    BranchName, LabelName, RepoWatchAuthorLogin, RepoWatchPattern, RepositorySlug,
};

/// Label predicates for one version-one rule. Empty lists impose no condition.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RepoWatchLabelMatcher {
    pub(super) any_of: Box<[LabelName]>,
    pub(super) all_of: Box<[LabelName]>,
    pub(super) none_of: Box<[LabelName]>,
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
    pub(super) event_kinds: Box<[RepoWatchEventKindNameV1]>,
    pub(super) repository: Option<RepositorySlug>,
    pub(super) base_branch: Option<BranchName>,
    pub(super) head_branch: Option<RepoWatchPattern>,
    pub(super) title: Option<RepoWatchPattern>,
    pub(super) body: Option<RepoWatchPattern>,
    pub(super) labels: RepoWatchLabelMatcher,
    pub(super) draft: Option<bool>,
    pub(super) author: Option<RepoWatchAuthorLogin>,
    pub(super) mergeable_state: Box<[MergeableState]>,
    pub(super) conclusion: Box<[CheckConclusion]>,
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

    pub(super) fn produces_branch_context(&self) -> bool {
        self.base_branch.is_none()
            && self.head_branch.is_none()
            && self.title.is_none()
            && self.body.is_none()
            && self.labels.any_of.is_empty()
            && self.labels.all_of.is_empty()
            && self.labels.none_of.is_empty()
            && self.draft.is_none()
            && self.author.is_none()
            && self.produces_context_shape(RepoWatchDispatchContextShape::Branch)
    }

    fn produces_context_shape(&self, shape: RepoWatchDispatchContextShape) -> bool {
        if self.event_kinds.is_empty() {
            return match shape {
                RepoWatchDispatchContextShape::PullRequest => {
                    self.mergeable_state.is_empty() || self.conclusion.is_empty()
                }
                RepoWatchDispatchContextShape::Branch => self.mergeable_state.is_empty(),
            };
        }
        self.event_kinds
            .iter()
            .copied()
            .any(|kind| kind.dispatch_context_shape() == shape && self.kind_can_match(kind))
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

    /// Reports whether one closed durable fact satisfies every configured field.
    pub fn matches(&self, event: &RepoWatchEvent) -> bool {
        if !self.event_kinds.is_empty() && !self.event_kinds.contains(&event.kind().name()) {
            return false;
        }
        if self
            .repository
            .as_ref()
            .is_some_and(|repository| repository != event.repository())
        {
            return false;
        }
        if !self.mergeable_state_matches(event.kind()) || !self.conclusion_matches(event.kind()) {
            return false;
        }
        match event.target() {
            RepoWatchEventTarget::PullRequest(context) => self.pull_request_fields_match(context),
            RepoWatchEventTarget::Branch => self.has_no_pull_request_fields(),
        }
    }

    fn has_no_pull_request_fields(&self) -> bool {
        self.base_branch.is_none()
            && self.head_branch.is_none()
            && self.title.is_none()
            && self.body.is_none()
            && self.labels.any_of.is_empty()
            && self.labels.all_of.is_empty()
            && self.labels.none_of.is_empty()
            && self.draft.is_none()
            && self.author.is_none()
            && self.mergeable_state.is_empty()
    }

    fn pull_request_fields_match(&self, context: &PullRequestEventContext) -> bool {
        self.base_branch
            .as_ref()
            .is_none_or(|branch| branch == context.base_branch())
            && self
                .head_branch
                .as_ref()
                .is_none_or(|pattern| pattern.is_match(context.head_branch().as_str()))
            && self
                .title
                .as_ref()
                .is_none_or(|pattern| pattern.is_match(context.title().as_str()))
            && self
                .body
                .as_ref()
                .is_none_or(|pattern| pattern.is_match(context.body().as_str()))
            && (self.labels.any_of.is_empty()
                || self
                    .labels
                    .any_of
                    .iter()
                    .any(|label| context.labels().contains(label)))
            && self
                .labels
                .all_of
                .iter()
                .all(|label| context.labels().contains(label))
            && self
                .labels
                .none_of
                .iter()
                .all(|label| !context.labels().contains(label))
            && self.draft.is_none_or(|draft| draft == context.draft())
            && self
                .author
                .as_ref()
                .is_none_or(|author| context.author() == Some(author))
    }

    fn mergeable_state_matches(&self, kind: &RepoWatchEventKindV1) -> bool {
        if self.mergeable_state.is_empty() {
            return true;
        }
        matches!(
            kind,
            RepoWatchEventKindV1::MergeableStateChanged { current }
                if self.mergeable_state.contains(current)
        )
    }

    fn conclusion_matches(&self, kind: &RepoWatchEventKindV1) -> bool {
        if self.conclusion.is_empty() {
            return true;
        }
        let conclusion = match kind {
            RepoWatchEventKindV1::ChecksCompleted { outcome } => (*outcome).into(),
            RepoWatchEventKindV1::CheckRunCompleted { conclusion, .. }
            | RepoWatchEventKindV1::BranchWorkflowRunCompleted { conclusion, .. } => *conclusion,
            _ => return false,
        };
        self.conclusion.contains(&conclusion)
    }
}
