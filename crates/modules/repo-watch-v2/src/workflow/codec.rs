//! Checked rule and event payloads for repository-watch workflow effects.
//! Governed by docs/spec/repo-watch.md.

use super::*;
use crate::observation_decode::{array, conclusion, mergeable, text};
use signalbox_session_ownership::{
    BranchName, LabelName, RepoWatchAuthorLogin, RepoWatchLabelMatcher, RepoWatchLabelMatcherInput,
    RepoWatchMatcherV1, RepoWatchMatcherV1Input, RepoWatchPattern,
};

pub(super) fn rule_value(rule: &RepoWatchRule) -> Value {
    let m = rule.matcher();
    fn labels(v: &[LabelName]) -> Vec<&str> {
        v.iter().map(LabelName::as_str).collect()
    }
    json!({
        "id": rule.id().as_str(), "version": rule.version().get().to_string(),
        "matcher": {
            "events": m.event_kinds().iter().copied().map(crate::event_kind_storage).collect::<Vec<_>>(),
            "repository": m.repository().map(RepositorySlug::as_str),
            "base": m.base_branch().map(BranchName::as_str),
            "head": m.head_branch().map(RepoWatchPattern::as_str),
            "title": m.title().map(RepoWatchPattern::as_str),
            "body": m.body().map(RepoWatchPattern::as_str),
            "any": labels(m.labels().any_of()), "all": labels(m.labels().all_of()), "none": labels(m.labels().none_of()),
            "draft": m.draft(), "author": m.author().map(RepoWatchAuthorLogin::as_str),
            "mergeable": m.mergeable_state().iter().copied().map(crate::mergeable_state_storage).collect::<Vec<_>>(),
            "conclusion": m.conclusion().iter().copied().map(crate::check_conclusion_storage).collect::<Vec<_>>()
        },
        "actions": rule.actions().iter().map(|a| { let RepoWatchRuleActionV1::DispatchSession { template } = a; template.as_str() }).collect::<Vec<_>>(),
        "scope": match rule.singleton_per() { RepoWatchSingletonScope::PullRequest => "pull_request", RepoWatchSingletonScope::Stack => "stack", RepoWatchSingletonScope::Rule => "rule", RepoWatchSingletonScope::Repository => "repository" },
        "cooldown": rule.cooldown().as_secs().to_string()
    })
}

fn optional<T>(v: &Value, f: impl FnOnce(&Value) -> Option<T>) -> Option<Option<T>> {
    if v.is_null() {
        Some(None)
    } else {
        f(v).map(Some)
    }
}

pub(super) fn decimal(v: &Value) -> Option<u64> {
    let s = v.as_str()?;
    let n: u64 = s.parse().ok()?;
    (n.to_string() == s).then_some(n)
}

pub(super) fn rule(v: &Value) -> Option<RepoWatchRule> {
    let m = &v["matcher"];
    let pattern = |v: &Value| RepoWatchPattern::try_new(text(v)?).ok();
    let labels = |v: &Value| array(v, |v| LabelName::try_new(text(v)?).ok());
    let checked = RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(text(&v["id"])?).ok()?,
        RepoWatchRuleVersion::new(NonZeroU64::new(decimal(&v["version"])?)?)?,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: array(&m["events"], |v| {
                crate::event_kind_from_storage(v.as_str()?)
            })?,
            repository: optional(&m["repository"], |v| RepositorySlug::try_new(text(v)?).ok())?,
            base_branch: optional(&m["base"], |v| BranchName::try_new(text(v)?).ok())?,
            head_branch: optional(&m["head"], pattern)?,
            title: optional(&m["title"], pattern)?,
            body: optional(&m["body"], pattern)?,
            labels: RepoWatchLabelMatcher::new(RepoWatchLabelMatcherInput {
                any_of: labels(&m["any"])?,
                all_of: labels(&m["all"])?,
                none_of: labels(&m["none"])?,
            }),
            draft: optional(&m["draft"], Value::as_bool)?,
            author: optional(&m["author"], |v| {
                RepoWatchAuthorLogin::try_new(text(v)?).ok()
            })?,
            mergeable_state: array(&m["mergeable"], mergeable)?,
            conclusion: array(&m["conclusion"], conclusion)?,
        }),
        array(&v["actions"], |v| {
            Some(RepoWatchRuleActionV1::DispatchSession {
                template: SessionTemplateName::try_new(text(v)?).ok()?,
            })
        })?,
        match v["scope"].as_str()? {
            "pull_request" => RepoWatchSingletonScope::PullRequest,
            "stack" => RepoWatchSingletonScope::Stack,
            "rule" => RepoWatchSingletonScope::Rule,
            "repository" => RepoWatchSingletonScope::Repository,
            _ => return None,
        },
        std::time::Duration::from_secs(decimal(&v["cooldown"])?),
    )
    .ok()?;
    (rule_value(&checked) == *v).then_some(checked)
}

impl RuleContext {
    pub fn encode(&self) -> Result<Vec<u8>, StoreError> {
        serde_json::to_vec(&self.value()).map_err(|_| StoreError::InvalidRetainedEvent)
    }

    fn value(&self) -> Value {
        json!({"rule": rule_value(&self.rule), "event_id": self.event.id().into_uuid().to_string(),
            "event": crate::normalized_event_payload(&self.event), "ordinal": self.ordinal.to_string(), "singleton_key": self.singleton_key })
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let v: Value = serde_json::from_slice(bytes).ok()?;
        let checked = Self {
            rule: rule(&v["rule"])?,
            event: crate::event_decode::event(
                RepoWatchEventId::from_uuid(Uuid::parse_str(v["event_id"].as_str()?).ok()?),
                &serde_json::to_vec(&v["event"]).ok()?,
            )?,
            ordinal: decimal(&v["ordinal"]).filter(|n| *n > 0)?,
            singleton_key: optional(&v["singleton_key"], text)?,
        };
        (checked.value() == v).then_some(checked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_codec_retains_matchers_and_refuses_unadmitted_fields() {
        let rule = RepoWatchRule::try_new(
            RepoWatchRuleId::try_new("review".into()).expect("rule"),
            RepoWatchRuleVersion::V1,
            RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
                event_kinds: vec![
                    signalbox_session_ownership::RepoWatchEventKindNameV1::PullRequestOpened,
                ],
                head_branch: Some(
                    RepoWatchPattern::try_new("^feature/.*$".into()).expect("pattern"),
                ),
                draft: Some(false),
                labels: RepoWatchLabelMatcher::new(RepoWatchLabelMatcherInput {
                    all_of: vec![LabelName::try_new("review".into()).expect("label")],
                    ..Default::default()
                }),
                ..Default::default()
            }),
            vec![RepoWatchRuleActionV1::DispatchSession {
                template: SessionTemplateName::try_new("review".into()).expect("template"),
            }],
            RepoWatchSingletonScope::Stack,
            std::time::Duration::from_secs(10),
        )
        .expect("rule");
        let encoded = rule_value(&rule);
        assert_eq!(super::rule(&encoded), Some(rule));
        let mut extra = encoded.clone();
        extra["matcher"]["unadmitted"] = json!(true);
        assert!(super::rule(&extra).is_none());
        let mut imprecise = encoded;
        imprecise["version"] = json!(1);
        assert!(super::rule(&imprecise).is_none());
    }
}
