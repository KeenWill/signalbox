//! Repository watch rule for `docs/spec/repo-watch.md`.

use super::action::RepoWatchRuleActionV1;
use super::dispatch::RepoWatchSingletonScope;
use super::event::{CheckConclusion, MergeableState, RepoWatchEventKindNameV1};
use super::matcher::RepoWatchMatcherV1;
use super::values::{
    BranchName, LabelName, RepoWatchAuthorLogin, RepoWatchPattern, RepoWatchRuleId,
    RepoWatchRuleVersion, RepositorySlug,
};
use sha2::{Digest, Sha256};
use std::{error::Error, fmt, hash::Hash, time::Duration};

/// Domain-separated digest of one rule's complete versioned semantics.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct RepoWatchRuleContentDigest([u8; 32]);

impl RepoWatchRuleContentDigest {
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// One configuration field whose value belongs to a durable rule identity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RepoWatchRuleIdentityField {
    /// The event kinds a matching fact is one of.
    MatcherEventKinds,
    /// The repository a matching fact belongs to.
    MatcherRepository,
    /// The base branch a matching pull request targets.
    MatcherBaseBranch,
    /// The pattern a matching pull request's head branch satisfies.
    MatcherHeadBranchRegex,
    /// The pattern a matching pull request's title satisfies.
    MatcherTitleRegex,
    /// The pattern a matching pull request's body satisfies.
    MatcherBodyRegex,
    /// The labels a matching pull request carries at least one of.
    MatcherLabelsAnyOf,
    /// The labels a matching pull request carries all of.
    MatcherLabelsAllOf,
    /// The labels a matching pull request carries none of.
    MatcherLabelsNoneOf,
    /// The draft state a matching pull request is in.
    MatcherDraft,
    /// The author a matching pull request has.
    MatcherAuthor,
    /// The mergeable states a matching fact reports one of.
    MatcherMergeableStateAnyOf,
    /// The check conclusions a matching fact reports one of.
    MatcherConclusionAnyOf,
    /// The ordered actions a match dispatches.
    Actions,
    /// The scope a dispatch holds its singleton over.
    SingletonPer,
    /// The interval a dispatch suppresses further matches for.
    CooldownSeconds,
}

impl RepoWatchRuleIdentityField {
    /// The head of the inventory the fingerprint order follows.
    pub(super) const fn first() -> Self {
        Self::MatcherEventKinds
    }

    /// The inventory successor of `self`, or `None` for the tail.
    ///
    /// The chain is the first guard: each arm names its successor, so adding a
    /// field makes this `match` non-exhaustive and the crate stops compiling
    /// until the new field is slotted into it.
    ///
    /// Exhaustiveness alone constrains the *arms*, not reachability from the
    /// head: a field added as `NewField => None` that no arm points at compiles
    /// while never contributing a fingerprint chunk, which would let an
    /// identity-relevant value change without a revision bump. The paired
    /// `inventory_predecessor` match is the second guard, and
    /// `every_identity_field_is_linked_into_the_inventory` checks the two are
    /// mutual inverses, so a link written in one direction only fails rather
    /// than silently shortening the inventory. That pairing is test-gated, so
    /// it is named here in plain text rather than linked: a rustdoc link would
    /// resolve only under `cfg(test)` and break the documentation build.
    ///
    /// The residual limit is the same one the event-kind inventory records: a
    /// field orphaned in *both* directions cannot be detected here, because
    /// safe Rust offers no way to enumerate an enum's variants without a
    /// derive, and this crate deliberately takes no `strum`/`EnumIter`
    /// dependency. Closing that last case is a dependency decision, not a code
    /// change.
    pub(super) const fn next(self) -> Option<Self> {
        match self {
            Self::MatcherEventKinds => Some(Self::MatcherRepository),
            Self::MatcherRepository => Some(Self::MatcherBaseBranch),
            Self::MatcherBaseBranch => Some(Self::MatcherHeadBranchRegex),
            Self::MatcherHeadBranchRegex => Some(Self::MatcherTitleRegex),
            Self::MatcherTitleRegex => Some(Self::MatcherBodyRegex),
            Self::MatcherBodyRegex => Some(Self::MatcherLabelsAnyOf),
            Self::MatcherLabelsAnyOf => Some(Self::MatcherLabelsAllOf),
            Self::MatcherLabelsAllOf => Some(Self::MatcherLabelsNoneOf),
            Self::MatcherLabelsNoneOf => Some(Self::MatcherDraft),
            Self::MatcherDraft => Some(Self::MatcherAuthor),
            Self::MatcherAuthor => Some(Self::MatcherMergeableStateAnyOf),
            Self::MatcherMergeableStateAnyOf => Some(Self::MatcherConclusionAnyOf),
            Self::MatcherConclusionAnyOf => Some(Self::Actions),
            Self::Actions => Some(Self::SingletonPer),
            Self::SingletonPer => Some(Self::CooldownSeconds),
            Self::CooldownSeconds => None,
        }
    }

    /// The inventory predecessor of `self`, or `None` for the head.
    ///
    /// Paired with the successor chain so linkage is checked rather than
    /// assumed. Exhaustive for the same reason that one is, and test-gated
    /// because checking the pairing is its only purpose — CI always compiles
    /// the tests, so a new field still cannot skip this match.
    #[cfg(test)]
    pub(super) const fn inventory_predecessor(self) -> Option<Self> {
        match self {
            Self::MatcherEventKinds => None,
            Self::MatcherRepository => Some(Self::MatcherEventKinds),
            Self::MatcherBaseBranch => Some(Self::MatcherRepository),
            Self::MatcherHeadBranchRegex => Some(Self::MatcherBaseBranch),
            Self::MatcherTitleRegex => Some(Self::MatcherHeadBranchRegex),
            Self::MatcherBodyRegex => Some(Self::MatcherTitleRegex),
            Self::MatcherLabelsAnyOf => Some(Self::MatcherBodyRegex),
            Self::MatcherLabelsAllOf => Some(Self::MatcherLabelsAnyOf),
            Self::MatcherLabelsNoneOf => Some(Self::MatcherLabelsAllOf),
            Self::MatcherDraft => Some(Self::MatcherLabelsNoneOf),
            Self::MatcherAuthor => Some(Self::MatcherDraft),
            Self::MatcherMergeableStateAnyOf => Some(Self::MatcherAuthor),
            Self::MatcherConclusionAnyOf => Some(Self::MatcherMergeableStateAnyOf),
            Self::Actions => Some(Self::MatcherConclusionAnyOf),
            Self::SingletonPer => Some(Self::Actions),
            Self::CooldownSeconds => Some(Self::SingletonPer),
        }
    }

    /// Exact TOML path an operator changes to revise this semantic field.
    pub const fn configuration_path(self) -> &'static str {
        match self {
            Self::MatcherEventKinds => "matcher.event_kinds",
            Self::MatcherRepository => "matcher.repo",
            Self::MatcherBaseBranch => "matcher.base_branch",
            Self::MatcherHeadBranchRegex => "matcher.head_branch_regex",
            Self::MatcherTitleRegex => "matcher.title_regex",
            Self::MatcherBodyRegex => "matcher.body_regex",
            Self::MatcherLabelsAnyOf => "matcher.labels.any_of",
            Self::MatcherLabelsAllOf => "matcher.labels.all_of",
            Self::MatcherLabelsNoneOf => "matcher.labels.none_of",
            Self::MatcherDraft => "matcher.draft",
            Self::MatcherAuthor => "matcher.author",
            Self::MatcherMergeableStateAnyOf => "matcher.mergeable_state.any_of",
            Self::MatcherConclusionAnyOf => "matcher.conclusion.any_of",
            Self::Actions => "actions",
            Self::SingletonPer => "singleton_per",
            Self::CooldownSeconds => "cooldown_seconds",
        }
    }
}

/// Domain-separated digest of one identity-relevant rule field.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct RepoWatchRuleIdentityFieldDigest([u8; 32]);

impl RepoWatchRuleIdentityFieldDigest {
    /// The digest bytes covering this field's configured value.
    ///
    /// Persistence stores them as one fixed-width chunk of a rule's durable
    /// fingerprint and compares chunks positionally, so these bytes are a
    /// stored identity rather than an in-process hash: they reveal nothing
    /// about the value they cover, and changing how they are derived
    /// invalidates every fingerprint already recorded.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for RepoWatchRuleIdentityFieldDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RepoWatchRuleIdentityFieldDigest([digest])")
    }
}

impl fmt::Debug for RepoWatchRuleContentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RepoWatchRuleContentDigest([digest])")
    }
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
    SubsecondCooldown,
    BranchEventWithPullRequestSingleton { scope: RepoWatchSingletonScope },
}

impl fmt::Display for RepoWatchRuleValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoActions => formatter.write_str("repository-watch rule has no actions"),
            Self::SubsecondCooldown => {
                formatter.write_str("repository-watch cooldown must use whole seconds")
            }
            Self::BranchEventWithPullRequestSingleton { scope } => write!(
                formatter,
                "repository-watch branch event cannot use `{}` singleton scope",
                scope.as_str()
            ),
        }
    }
}

impl Error for RepoWatchRuleValidationError {}

impl RepoWatchRule {
    pub fn try_new(
        id: RepoWatchRuleId,
        version: RepoWatchRuleVersion,
        matcher: RepoWatchMatcherV1,
        actions: Vec<RepoWatchRuleActionV1>,
        singleton_per: RepoWatchSingletonScope,
        cooldown: Duration,
    ) -> Result<Self, RepoWatchRuleValidationError> {
        if actions.is_empty() {
            return Err(RepoWatchRuleValidationError::NoActions);
        }
        if cooldown.subsec_nanos() != 0 {
            return Err(RepoWatchRuleValidationError::SubsecondCooldown);
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
            version,
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

    /// Derives the stable identity of every matcher, action, and admission field.
    pub fn content_digest(&self) -> RepoWatchRuleContentDigest {
        let mut digest = Sha256::new();
        update_rule_digest_frame(&mut digest, b"signalbox/repo-watch/rule-content-digest/v1");
        update_rule_digest_frame(&mut digest, &self.version.get().to_be_bytes());
        update_rule_digest_set(
            &mut digest,
            b"event_kinds",
            self.matcher
                .event_kinds
                .iter()
                .map(|kind| repo_watch_event_kind_name(*kind)),
        );
        update_rule_digest_option(
            &mut digest,
            b"repository",
            self.matcher.repository.as_ref().map(RepositorySlug::as_str),
        );
        update_rule_digest_option(
            &mut digest,
            b"base_branch",
            self.matcher.base_branch.as_ref().map(BranchName::as_str),
        );
        update_rule_digest_option(
            &mut digest,
            b"head_branch",
            self.matcher
                .head_branch
                .as_ref()
                .map(RepoWatchPattern::as_str),
        );
        update_rule_digest_option(
            &mut digest,
            b"title",
            self.matcher.title.as_ref().map(RepoWatchPattern::as_str),
        );
        update_rule_digest_option(
            &mut digest,
            b"body",
            self.matcher.body.as_ref().map(RepoWatchPattern::as_str),
        );
        update_rule_digest_set(
            &mut digest,
            b"labels_any_of",
            self.matcher.labels.any_of.iter().map(LabelName::as_str),
        );
        update_rule_digest_set(
            &mut digest,
            b"labels_all_of",
            self.matcher.labels.all_of.iter().map(LabelName::as_str),
        );
        update_rule_digest_set(
            &mut digest,
            b"labels_none_of",
            self.matcher.labels.none_of.iter().map(LabelName::as_str),
        );
        update_rule_digest_frame(&mut digest, b"draft");
        update_rule_digest_frame(
            &mut digest,
            match self.matcher.draft {
                Some(true) => &b"true"[..],
                Some(false) => &b"false"[..],
                None => &b"none"[..],
            },
        );
        update_rule_digest_option(
            &mut digest,
            b"author",
            self.matcher
                .author
                .as_ref()
                .map(RepoWatchAuthorLogin::as_str),
        );
        update_rule_digest_set(
            &mut digest,
            b"mergeable_state",
            self.matcher
                .mergeable_state
                .iter()
                .map(|state| mergeable_state_name(*state)),
        );
        update_rule_digest_set(
            &mut digest,
            b"conclusion",
            self.matcher
                .conclusion
                .iter()
                .map(|conclusion| check_conclusion_name(*conclusion)),
        );
        update_rule_digest_frame(&mut digest, b"actions");
        update_rule_digest_frame(
            &mut digest,
            &u64::try_from(self.actions.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        for action in &self.actions {
            match action {
                RepoWatchRuleActionV1::DispatchSession { template } => {
                    update_rule_digest_frame(&mut digest, b"dispatch_session");
                    update_rule_digest_frame(&mut digest, template.as_str().as_bytes());
                }
            }
        }
        update_rule_digest_frame(&mut digest, b"singleton_per");
        update_rule_digest_frame(&mut digest, self.singleton_per.as_str().as_bytes());
        update_rule_digest_frame(&mut digest, b"cooldown_seconds");
        update_rule_digest_frame(&mut digest, &self.cooldown.as_secs().to_be_bytes());
        update_rule_digest_frame(&mut digest, b"cooldown_nanoseconds");
        update_rule_digest_frame(&mut digest, &self.cooldown.subsec_nanos().to_be_bytes());
        RepoWatchRuleContentDigest(digest.finalize().into())
    }

    /// Derives stable, field-labeled fingerprints for configuration diagnostics.
    /// The digest of every identity-relevant field, in storage order.
    ///
    /// Each entry labels one configuration field with a content-free digest of
    /// its value, so a stored fingerprint can name the exact field an operator
    /// changed without retaining the configured value itself. The order is the
    /// inventory order and is durable: stored fingerprints are compared
    /// positionally against it.
    pub fn identity_field_digests(
        &self,
    ) -> Vec<(RepoWatchRuleIdentityField, RepoWatchRuleIdentityFieldDigest)> {
        let mut fields = Vec::new();
        let mut field = Some(RepoWatchRuleIdentityField::first());
        while let Some(current) = field {
            fields.push((current, self.identity_field_digest(current)));
            field = current.next();
        }
        fields
    }

    fn identity_field_digest(
        &self,
        field: RepoWatchRuleIdentityField,
    ) -> RepoWatchRuleIdentityFieldDigest {
        let mut digest = Sha256::new();
        update_rule_digest_frame(
            &mut digest,
            b"signalbox/repo-watch/rule-identity-field-digest/v1",
        );
        update_rule_digest_frame(&mut digest, field.configuration_path().as_bytes());
        match field {
            RepoWatchRuleIdentityField::MatcherEventKinds => update_rule_digest_set(
                &mut digest,
                b"value",
                self.matcher
                    .event_kinds
                    .iter()
                    .map(|kind| repo_watch_event_kind_name(*kind)),
            ),
            RepoWatchRuleIdentityField::MatcherRepository => update_rule_digest_option(
                &mut digest,
                b"value",
                self.matcher.repository.as_ref().map(RepositorySlug::as_str),
            ),
            RepoWatchRuleIdentityField::MatcherBaseBranch => update_rule_digest_option(
                &mut digest,
                b"value",
                self.matcher.base_branch.as_ref().map(BranchName::as_str),
            ),
            RepoWatchRuleIdentityField::MatcherHeadBranchRegex => update_rule_digest_option(
                &mut digest,
                b"value",
                self.matcher
                    .head_branch
                    .as_ref()
                    .map(RepoWatchPattern::as_str),
            ),
            RepoWatchRuleIdentityField::MatcherTitleRegex => update_rule_digest_option(
                &mut digest,
                b"value",
                self.matcher.title.as_ref().map(RepoWatchPattern::as_str),
            ),
            RepoWatchRuleIdentityField::MatcherBodyRegex => update_rule_digest_option(
                &mut digest,
                b"value",
                self.matcher.body.as_ref().map(RepoWatchPattern::as_str),
            ),
            RepoWatchRuleIdentityField::MatcherLabelsAnyOf => update_rule_digest_set(
                &mut digest,
                b"value",
                self.matcher.labels.any_of.iter().map(LabelName::as_str),
            ),
            RepoWatchRuleIdentityField::MatcherLabelsAllOf => update_rule_digest_set(
                &mut digest,
                b"value",
                self.matcher.labels.all_of.iter().map(LabelName::as_str),
            ),
            RepoWatchRuleIdentityField::MatcherLabelsNoneOf => update_rule_digest_set(
                &mut digest,
                b"value",
                self.matcher.labels.none_of.iter().map(LabelName::as_str),
            ),
            RepoWatchRuleIdentityField::MatcherDraft => {
                update_rule_digest_frame(
                    &mut digest,
                    match self.matcher.draft {
                        Some(true) => &b"true"[..],
                        Some(false) => &b"false"[..],
                        None => &b"none"[..],
                    },
                );
            }
            RepoWatchRuleIdentityField::MatcherAuthor => update_rule_digest_option(
                &mut digest,
                b"value",
                self.matcher
                    .author
                    .as_ref()
                    .map(RepoWatchAuthorLogin::as_str),
            ),
            RepoWatchRuleIdentityField::MatcherMergeableStateAnyOf => update_rule_digest_set(
                &mut digest,
                b"value",
                self.matcher
                    .mergeable_state
                    .iter()
                    .map(|state| mergeable_state_name(*state)),
            ),
            RepoWatchRuleIdentityField::MatcherConclusionAnyOf => update_rule_digest_set(
                &mut digest,
                b"value",
                self.matcher
                    .conclusion
                    .iter()
                    .map(|conclusion| check_conclusion_name(*conclusion)),
            ),
            RepoWatchRuleIdentityField::Actions => {
                update_rule_digest_frame(
                    &mut digest,
                    &u64::try_from(self.actions.len())
                        .unwrap_or(u64::MAX)
                        .to_be_bytes(),
                );
                for action in &self.actions {
                    match action {
                        RepoWatchRuleActionV1::DispatchSession { template } => {
                            update_rule_digest_frame(&mut digest, b"dispatch_session");
                            update_rule_digest_frame(&mut digest, template.as_str().as_bytes());
                        }
                    }
                }
            }
            RepoWatchRuleIdentityField::SingletonPer => {
                update_rule_digest_frame(&mut digest, self.singleton_per.as_str().as_bytes());
            }
            RepoWatchRuleIdentityField::CooldownSeconds => {
                update_rule_digest_frame(&mut digest, &self.cooldown.as_secs().to_be_bytes());
            }
        }
        RepoWatchRuleIdentityFieldDigest(digest.finalize().into())
    }
}

fn update_rule_digest_frame(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn update_rule_digest_option(digest: &mut Sha256, field: &[u8], value: Option<&str>) {
    update_rule_digest_frame(digest, field);
    match value {
        Some(value) => {
            update_rule_digest_frame(digest, b"some");
            update_rule_digest_frame(digest, value.as_bytes());
        }
        None => update_rule_digest_frame(digest, b"none"),
    }
}

fn update_rule_digest_set<'value>(
    digest: &mut Sha256,
    field: &[u8],
    values: impl Iterator<Item = &'value str>,
) {
    let mut values = values.collect::<Vec<_>>();
    values.sort_unstable();
    update_rule_digest_frame(digest, field);
    update_rule_digest_frame(
        digest,
        &u64::try_from(values.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for value in values {
        update_rule_digest_frame(digest, value.as_bytes());
    }
}

const fn repo_watch_event_kind_name(kind: RepoWatchEventKindNameV1) -> &'static str {
    match kind {
        RepoWatchEventKindNameV1::PullRequestOpened => "pull_request_opened",
        RepoWatchEventKindNameV1::PullRequestClosed => "pull_request_closed",
        RepoWatchEventKindNameV1::PullRequestMerged => "pull_request_merged",
        RepoWatchEventKindNameV1::HeadChanged => "head_changed",
        RepoWatchEventKindNameV1::MergeableStateChanged => "mergeable_state_changed",
        RepoWatchEventKindNameV1::ChecksCompleted => "checks_completed",
        RepoWatchEventKindNameV1::CheckRunCompleted => "check_run_completed",
        RepoWatchEventKindNameV1::BranchWorkflowRunCompleted => "branch_workflow_run_completed",
        RepoWatchEventKindNameV1::ReviewSubmitted => "review_submitted",
        RepoWatchEventKindNameV1::ThreadOpened => "thread_opened",
        RepoWatchEventKindNameV1::ThreadResolved => "thread_resolved",
        RepoWatchEventKindNameV1::Labeled => "labeled",
        RepoWatchEventKindNameV1::Unlabeled => "unlabeled",
        RepoWatchEventKindNameV1::BaseAdvanced => "base_advanced",
        RepoWatchEventKindNameV1::ReactionChanged => "reaction_changed",
    }
}

const fn mergeable_state_name(state: MergeableState) -> &'static str {
    match state {
        MergeableState::Mergeable => "mergeable",
        MergeableState::Conflicting => "conflicting",
        MergeableState::Unknown => "unknown",
    }
}

const fn check_conclusion_name(conclusion: CheckConclusion) -> &'static str {
    match conclusion {
        CheckConclusion::Success => "success",
        CheckConclusion::Failure => "failure",
        CheckConclusion::Neutral => "neutral",
        CheckConclusion::Cancelled => "cancelled",
        CheckConclusion::Skipped => "skipped",
        CheckConclusion::TimedOut => "timed_out",
        CheckConclusion::ActionRequired => "action_required",
        CheckConclusion::Stale => "stale",
        CheckConclusion::StartupFailure => "startup_failure",
    }
}
