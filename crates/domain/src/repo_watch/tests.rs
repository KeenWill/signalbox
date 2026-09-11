//! Repository watch tests for `docs/spec/repo-watch.md`.

/// The inventory is pinned as a literal, and every edge of the successor
/// chain is checked against its predecessor one assertion at a time.
///
/// Straight-line on purpose: each edge stays independently attributable,
/// so a broken link names itself instead of surfacing as one loop
/// iteration. The literal also pins order, membership, and count at once —
/// adding a variant fails here until the claim is revisited, which is the
/// point.
#[test]
fn every_event_kind_is_linked_into_the_inventory() {
    assert_eq!(
        RepoWatchEventKindNameV1::all(),
        vec![
            RepoWatchEventKindNameV1::PullRequestOpened,
            RepoWatchEventKindNameV1::PullRequestClosed,
            RepoWatchEventKindNameV1::PullRequestMerged,
            RepoWatchEventKindNameV1::HeadChanged,
            RepoWatchEventKindNameV1::MergeableStateChanged,
            RepoWatchEventKindNameV1::ChecksCompleted,
            RepoWatchEventKindNameV1::CheckRunCompleted,
            RepoWatchEventKindNameV1::BranchWorkflowRunCompleted,
            RepoWatchEventKindNameV1::ReviewSubmitted,
            RepoWatchEventKindNameV1::ThreadOpened,
            RepoWatchEventKindNameV1::ThreadResolved,
            RepoWatchEventKindNameV1::Labeled,
            RepoWatchEventKindNameV1::Unlabeled,
            RepoWatchEventKindNameV1::BaseAdvanced,
            RepoWatchEventKindNameV1::ReactionChanged,
        ]
    );

    assert_eq!(
        RepoWatchEventKindNameV1::PullRequestOpened.inventory_predecessor(),
        None
    );
    assert_eq!(
        RepoWatchEventKindNameV1::PullRequestClosed.inventory_predecessor(),
        Some(RepoWatchEventKindNameV1::PullRequestOpened)
    );
    assert_eq!(
        RepoWatchEventKindNameV1::PullRequestMerged.inventory_predecessor(),
        Some(RepoWatchEventKindNameV1::PullRequestClosed)
    );
    assert_eq!(
        RepoWatchEventKindNameV1::HeadChanged.inventory_predecessor(),
        Some(RepoWatchEventKindNameV1::PullRequestMerged)
    );
    assert_eq!(
        RepoWatchEventKindNameV1::MergeableStateChanged.inventory_predecessor(),
        Some(RepoWatchEventKindNameV1::HeadChanged)
    );
    assert_eq!(
        RepoWatchEventKindNameV1::ChecksCompleted.inventory_predecessor(),
        Some(RepoWatchEventKindNameV1::MergeableStateChanged)
    );
    assert_eq!(
        RepoWatchEventKindNameV1::CheckRunCompleted.inventory_predecessor(),
        Some(RepoWatchEventKindNameV1::ChecksCompleted)
    );
    assert_eq!(
        RepoWatchEventKindNameV1::BranchWorkflowRunCompleted.inventory_predecessor(),
        Some(RepoWatchEventKindNameV1::CheckRunCompleted)
    );
    assert_eq!(
        RepoWatchEventKindNameV1::ReviewSubmitted.inventory_predecessor(),
        Some(RepoWatchEventKindNameV1::BranchWorkflowRunCompleted)
    );
    assert_eq!(
        RepoWatchEventKindNameV1::ThreadOpened.inventory_predecessor(),
        Some(RepoWatchEventKindNameV1::ReviewSubmitted)
    );
    assert_eq!(
        RepoWatchEventKindNameV1::ThreadResolved.inventory_predecessor(),
        Some(RepoWatchEventKindNameV1::ThreadOpened)
    );
    assert_eq!(
        RepoWatchEventKindNameV1::Labeled.inventory_predecessor(),
        Some(RepoWatchEventKindNameV1::ThreadResolved)
    );
    assert_eq!(
        RepoWatchEventKindNameV1::Unlabeled.inventory_predecessor(),
        Some(RepoWatchEventKindNameV1::Labeled)
    );
    assert_eq!(
        RepoWatchEventKindNameV1::BaseAdvanced.inventory_predecessor(),
        Some(RepoWatchEventKindNameV1::Unlabeled)
    );
    assert_eq!(
        RepoWatchEventKindNameV1::ReactionChanged.inventory_predecessor(),
        Some(RepoWatchEventKindNameV1::BaseAdvanced)
    );
}

/// The revision bound is the durable one, checked at both edges.
#[test]
fn a_revision_beyond_the_durable_range_is_refused() {
    let highest = NonZeroU64::new(i64::MAX.unsigned_abs()).expect("the bound is positive");
    let beyond = NonZeroU64::new(i64::MAX.unsigned_abs() + 1).expect("one past is positive");

    assert_eq!(
        RepoWatchRuleVersion::new(highest).map(RepoWatchRuleVersion::get),
        Some(highest.get())
    );
    assert_eq!(RepoWatchRuleVersion::new(beyond), None);
    assert_eq!(
        RepoWatchRuleVersion::new(NonZeroU64::MIN),
        Some(RepoWatchRuleVersion::V1)
    );
}

/// Every edge of the successor chain is checked against its predecessor
/// one assertion at a time, from a head this also pins.
///
/// Straight-line for the same reason the event-kind inventory beside it
/// is: each edge stays independently attributable, so a broken link names
/// itself instead of surfacing as one loop iteration, and a chain edited
/// into a cycle fails an assertion rather than hanging the test. Naming
/// every field in both directions pins order, membership, and count at
/// once — adding a field fails here until the claim is revisited, which is
/// the point. Order is load-bearing because it is the order stored
/// fingerprints are compared in.
#[test]
fn every_identity_field_is_linked_into_the_inventory() {
    assert_eq!(
        RepoWatchRuleIdentityField::first(),
        RepoWatchRuleIdentityField::MatcherEventKinds
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherEventKinds.next(),
        Some(RepoWatchRuleIdentityField::MatcherRepository)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherRepository.next(),
        Some(RepoWatchRuleIdentityField::MatcherBaseBranch)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherBaseBranch.next(),
        Some(RepoWatchRuleIdentityField::MatcherHeadBranchRegex)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherHeadBranchRegex.next(),
        Some(RepoWatchRuleIdentityField::MatcherTitleRegex)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherTitleRegex.next(),
        Some(RepoWatchRuleIdentityField::MatcherBodyRegex)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherBodyRegex.next(),
        Some(RepoWatchRuleIdentityField::MatcherLabelsAnyOf)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherLabelsAnyOf.next(),
        Some(RepoWatchRuleIdentityField::MatcherLabelsAllOf)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherLabelsAllOf.next(),
        Some(RepoWatchRuleIdentityField::MatcherLabelsNoneOf)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherLabelsNoneOf.next(),
        Some(RepoWatchRuleIdentityField::MatcherDraft)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherDraft.next(),
        Some(RepoWatchRuleIdentityField::MatcherAuthor)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherAuthor.next(),
        Some(RepoWatchRuleIdentityField::MatcherMergeableStateAnyOf)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherMergeableStateAnyOf.next(),
        Some(RepoWatchRuleIdentityField::MatcherConclusionAnyOf)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherConclusionAnyOf.next(),
        Some(RepoWatchRuleIdentityField::Actions)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::Actions.next(),
        Some(RepoWatchRuleIdentityField::SingletonPer)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::SingletonPer.next(),
        Some(RepoWatchRuleIdentityField::CooldownSeconds)
    );

    assert_eq!(RepoWatchRuleIdentityField::CooldownSeconds.next(), None);

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherEventKinds.inventory_predecessor(),
        None
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherRepository.inventory_predecessor(),
        Some(RepoWatchRuleIdentityField::MatcherEventKinds)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherBaseBranch.inventory_predecessor(),
        Some(RepoWatchRuleIdentityField::MatcherRepository)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherHeadBranchRegex.inventory_predecessor(),
        Some(RepoWatchRuleIdentityField::MatcherBaseBranch)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherTitleRegex.inventory_predecessor(),
        Some(RepoWatchRuleIdentityField::MatcherHeadBranchRegex)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherBodyRegex.inventory_predecessor(),
        Some(RepoWatchRuleIdentityField::MatcherTitleRegex)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherLabelsAnyOf.inventory_predecessor(),
        Some(RepoWatchRuleIdentityField::MatcherBodyRegex)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherLabelsAllOf.inventory_predecessor(),
        Some(RepoWatchRuleIdentityField::MatcherLabelsAnyOf)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherLabelsNoneOf.inventory_predecessor(),
        Some(RepoWatchRuleIdentityField::MatcherLabelsAllOf)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherDraft.inventory_predecessor(),
        Some(RepoWatchRuleIdentityField::MatcherLabelsNoneOf)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherAuthor.inventory_predecessor(),
        Some(RepoWatchRuleIdentityField::MatcherDraft)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherMergeableStateAnyOf.inventory_predecessor(),
        Some(RepoWatchRuleIdentityField::MatcherAuthor)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::MatcherConclusionAnyOf.inventory_predecessor(),
        Some(RepoWatchRuleIdentityField::MatcherMergeableStateAnyOf)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::Actions.inventory_predecessor(),
        Some(RepoWatchRuleIdentityField::MatcherConclusionAnyOf)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::SingletonPer.inventory_predecessor(),
        Some(RepoWatchRuleIdentityField::Actions)
    );

    assert_eq!(
        RepoWatchRuleIdentityField::CooldownSeconds.inventory_predecessor(),
        Some(RepoWatchRuleIdentityField::SingletonPer)
    );
}
use std::{error::Error, num::NonZeroU64, time::Duration};

use uuid::Uuid;

use crate::{RepoWatchEventId, SessionTemplateName};

use super::{
    BranchName, CheckConclusion, CommitSha, LabelName, MergeableState, PullRequestBody,
    PullRequestEventContext, PullRequestEventContextInput, PullRequestNumber, PullRequestTitle,
    RepoWatchAuthorLogin, RepoWatchEvent, RepoWatchEventConstructionError,
    RepoWatchEventKindNameV1, RepoWatchEventKindV1, RepoWatchLabelMatcher,
    RepoWatchLabelMatcherInput, RepoWatchMatcherV1, RepoWatchMatcherV1Input, RepoWatchPattern,
    RepoWatchRule, RepoWatchRuleActionV1, RepoWatchRuleId, RepoWatchRuleIdentityField,
    RepoWatchRuleValidationError, RepoWatchRuleVersion, RepoWatchSingletonScope,
    RepoWatchTextError, RepositorySlug,
};

const CONTEXT_HEAD_SHA: &str = "1111111111111111111111111111111111111111";
const EVENT_HEAD_SHA: &str = "2222222222222222222222222222222222222222";
const PREVIOUS_HEAD_SHA: &str = "3333333333333333333333333333333333333333";
const MAIN_BRANCH: &str = "main";
const MAIN_BRANCH_FULL_REF: &str = "refs/heads/main";
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

fn digest_rule(
    matcher: RepoWatchMatcherV1,
    actions: Vec<RepoWatchRuleActionV1>,
    singleton_per: RepoWatchSingletonScope,
    cooldown: Duration,
) -> Result<RepoWatchRule, Box<dyn Error>> {
    Ok(RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(String::from("digest-rule"))?,
        RepoWatchRuleVersion::V1,
        matcher,
        actions,
        singleton_per,
        cooldown,
    )?)
}

fn dispatch_rule_action(template: &str) -> Result<RepoWatchRuleActionV1, Box<dyn Error>> {
    Ok(RepoWatchRuleActionV1::DispatchSession {
        template: SessionTemplateName::try_new(template.to_owned())?,
    })
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
fn branch_name_canonicalizes_a_full_head_ref() -> Result<(), RepoWatchTextError> {
    let branch = BranchName::try_new(String::from(MAIN_BRANCH_FULL_REF))?;

    assert_eq!(branch.as_str(), MAIN_BRANCH);
    Ok(())
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
fn matcher_regex_supports_unicode_properties_and_case_folding() -> Result<(), RepoWatchTextError> {
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
fn pull_request_context_retains_head_repository_and_missing_author() -> Result<(), Box<dyn Error>> {
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
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::default(),
        Vec::new(),
        RepoWatchSingletonScope::PullRequest,
        Duration::ZERO,
    );

    assert_eq!(result, Err(RepoWatchRuleValidationError::NoActions));
    Ok(())
}

#[test]
fn rule_rejects_a_subsecond_cooldown() -> Result<(), Box<dyn Error>> {
    let result = RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(String::from("subsecond-cooldown"))?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::default(),
        vec![dispatch_rule_action("handler")?],
        RepoWatchSingletonScope::PullRequest,
        Duration::from_nanos(1),
    );

    assert_eq!(result, Err(RepoWatchRuleValidationError::SubsecondCooldown));
    Ok(())
}

#[test]
fn rule_content_digest_covers_every_semantic_field_group() -> Result<(), Box<dyn Error>> {
    let matcher = RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
        event_kinds: vec![RepoWatchEventKindNameV1::PullRequestOpened],
        ..RepoWatchMatcherV1Input::default()
    });
    let actions = vec![
        dispatch_rule_action("first-handler")?,
        dispatch_rule_action("second-handler")?,
    ];
    let base = digest_rule(
        matcher.clone(),
        actions.clone(),
        RepoWatchSingletonScope::PullRequest,
        Duration::ZERO,
    )?;
    let changed_matcher = digest_rule(
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::PullRequestClosed],
            ..RepoWatchMatcherV1Input::default()
        }),
        actions.clone(),
        RepoWatchSingletonScope::PullRequest,
        Duration::ZERO,
    )?;
    let changed_actions = digest_rule(
        matcher.clone(),
        vec![actions[1].clone(), actions[0].clone()],
        RepoWatchSingletonScope::PullRequest,
        Duration::ZERO,
    )?;
    let changed_scope = digest_rule(
        matcher.clone(),
        actions.clone(),
        RepoWatchSingletonScope::Rule,
        Duration::ZERO,
    )?;
    let changed_cooldown = digest_rule(
        matcher,
        actions,
        RepoWatchSingletonScope::PullRequest,
        Duration::from_secs(1),
    )?;

    assert_ne!(base.content_digest(), changed_matcher.content_digest());
    assert_ne!(base.content_digest(), changed_actions.content_digest());
    assert_ne!(base.content_digest(), changed_scope.content_digest());
    assert_ne!(base.content_digest(), changed_cooldown.content_digest());
    Ok(())
}

#[test]
fn branch_event_rejects_pull_request_singleton_scope() -> Result<(), Box<dyn Error>> {
    let template = SessionTemplateName::try_new(String::from("branch-handler"))?;
    let result = RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(String::from("invalid-branch-scope"))?,
        RepoWatchRuleVersion::V1,
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
        RepoWatchRuleVersion::V1,
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
fn conflict_qualifier_matches_only_the_conflicting_payload() -> Result<(), Box<dyn Error>> {
    let matcher = RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
        event_kinds: vec![RepoWatchEventKindNameV1::MergeableStateChanged],
        mergeable_state: vec![MergeableState::Conflicting],
        ..RepoWatchMatcherV1Input::default()
    });
    let event = RepoWatchEvent::try_pull_request(
        RepoWatchEventId::from_uuid(Uuid::from_u128(30)),
        RepositorySlug::try_new(String::from("namespace/repo"))?,
        pull_request_context(
            CommitSha::try_new(String::from(CONTEXT_HEAD_SHA))?,
            BranchName::try_new(String::from("main"))?,
            Vec::new(),
        )?,
        RepoWatchEventKindV1::MergeableStateChanged {
            current: MergeableState::Conflicting,
        },
    )?;

    assert!(matcher.matches(&event));
    Ok(())
}

#[test]
fn conflict_qualifier_rejects_the_mergeable_payload() -> Result<(), Box<dyn Error>> {
    let matcher = RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
        event_kinds: vec![RepoWatchEventKindNameV1::MergeableStateChanged],
        mergeable_state: vec![MergeableState::Conflicting],
        ..RepoWatchMatcherV1Input::default()
    });
    let event = RepoWatchEvent::try_pull_request(
        RepoWatchEventId::from_uuid(Uuid::from_u128(31)),
        RepositorySlug::try_new(String::from("namespace/repo"))?,
        pull_request_context(
            CommitSha::try_new(String::from(CONTEXT_HEAD_SHA))?,
            BranchName::try_new(String::from("main"))?,
            Vec::new(),
        )?,
        RepoWatchEventKindV1::MergeableStateChanged {
            current: MergeableState::Mergeable,
        },
    )?;

    assert!(!matcher.matches(&event));
    Ok(())
}

#[test]
fn branch_conclusion_qualifier_matches_without_pull_request_fields() -> Result<(), Box<dyn Error>> {
    let matcher = RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
        event_kinds: vec![RepoWatchEventKindNameV1::BranchWorkflowRunCompleted],
        conclusion: vec![CheckConclusion::Failure],
        ..RepoWatchMatcherV1Input::default()
    });
    let event = RepoWatchEvent::branch_workflow(
        RepoWatchEventId::from_uuid(Uuid::from_u128(32)),
        RepositorySlug::try_new(String::from("namespace/repo"))?,
        BranchName::try_new(String::from("main"))?,
        super::WorkflowName::try_new(String::from("ci"))?,
        CheckConclusion::Failure,
    );

    assert!(matcher.matches(&event));
    Ok(())
}

#[test]
fn activation_matching_uses_current_labels_and_mergeability() -> Result<(), Box<dyn Error>> {
    let repository = RepositorySlug::try_new(String::from("namespace/repo"))?;
    let excluded = LabelName::try_new(String::from("no-auto"))?;
    let context = pull_request_context(
        CommitSha::try_new(String::from(CONTEXT_HEAD_SHA))?,
        BranchName::try_new(String::from("main"))?,
        Vec::new(),
    )?;
    let excluded_context = pull_request_context(
        context.head_sha().clone(),
        context.base_branch().clone(),
        vec![excluded.clone()],
    )?;
    let matcher = RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
        event_kinds: vec![RepoWatchEventKindNameV1::MergeableStateChanged],
        mergeable_state: vec![MergeableState::Conflicting],
        labels: RepoWatchLabelMatcher::new(RepoWatchLabelMatcherInput {
            none_of: vec![excluded],
            ..Default::default()
        }),
        ..Default::default()
    });
    assert!(matcher.matches_activation(
        &repository,
        &context,
        MergeableState::Conflicting,
        std::iter::empty()
    ));
    assert!(!matcher.matches_activation(
        &repository,
        &excluded_context,
        MergeableState::Conflicting,
        std::iter::empty()
    ));
    assert!(!matcher.matches_activation(
        &repository,
        &context,
        MergeableState::Mergeable,
        std::iter::empty()
    ));
    Ok(())
}

#[test]
fn activation_matching_uses_check_conclusions_and_excludes_branch_only_rules()
-> Result<(), Box<dyn Error>> {
    let repository = RepositorySlug::try_new(String::from("namespace/repo"))?;
    let context = pull_request_context(
        CommitSha::try_new(String::from(CONTEXT_HEAD_SHA))?,
        BranchName::try_new(String::from("main"))?,
        Vec::new(),
    )?;
    let matcher = RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
        event_kinds: vec![RepoWatchEventKindNameV1::CheckRunCompleted],
        conclusion: vec![CheckConclusion::Failure],
        ..Default::default()
    });
    assert!(matcher.matches_activation(
        &repository,
        &context,
        MergeableState::Mergeable,
        [CheckConclusion::Failure].into_iter()
    ));
    assert!(!matcher.matches_activation(
        &repository,
        &context,
        MergeableState::Mergeable,
        [CheckConclusion::Success].into_iter()
    ));
    let branch_only = RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
        event_kinds: vec![RepoWatchEventKindNameV1::BranchWorkflowRunCompleted],
        ..Default::default()
    });
    assert!(!branch_only.matches_activation(
        &repository,
        &context,
        MergeableState::Conflicting,
        [CheckConclusion::Failure].into_iter()
    ));
    Ok(())
}
