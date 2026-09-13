//! GitHub's required-check classification for a pull request's current head.

use crate::provider::{GitHubObservationRead, ObservationError};
use serde_json::{Value, json};
use signalbox_session_ownership::{CheckConclusion, CommitSha, PullRequestNumber, RepositorySlug};

const QUERY: &str = r#"
query RequiredChecks($owner: String!, $name: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      headRefOid
      commits(last: 1) { nodes { commit { oid statusCheckRollup {
        contexts(first: 100, after: $after) {
          nodes {
            __typename
            ... on CheckRun { isRequired(pullRequestNumber: $number) conclusion }
            ... on StatusContext { isRequired(pullRequestNumber: $number) state }
          }
          pageInfo { hasNextPage endCursor }
        }
      } } } }
    }
  }
}"#;

/// Content-free required-check facts from one GitHub connection page.
#[derive(Clone, Debug)]
pub struct RequiredCheckPage {
    pub head: CommitSha,
    pub conclusions: Vec<CheckConclusion>,
    pub after: Option<String>,
}

impl RequiredCheckPage {
    pub(crate) fn encode(&self) -> Value {
        json!({"head":self.head.as_str(),"conclusions":self.conclusions.iter().map(|value| crate::baseline::check_conclusion_storage(*value)).collect::<Vec<_>>(),"after":self.after})
    }

    pub(crate) fn decode(value: &Value) -> Option<Self> {
        Some(Self {
            head: CommitSha::try_new(value["head"].as_str()?.to_owned()).ok()?,
            conclusions: crate::observation_decode::array(
                value.get("conclusions")?,
                crate::observation_decode::conclusion,
            )?,
            after: match value.get("after")? {
                Value::Null => None,
                value => Some(value.as_str()?.to_owned()),
            },
        })
    }
}

pub(crate) fn decode_response(value: &Value) -> Option<RequiredCheckPage> {
    if value
        .get("errors")
        .is_some_and(|v| v.as_array().is_none_or(|errors| !errors.is_empty()))
    {
        return None;
    }
    let pull = &value["data"]["repository"]["pullRequest"];
    let head = CommitSha::try_new(pull["headRefOid"].as_str()?.to_owned()).ok()?;
    let commits = pull["commits"]["nodes"].as_array()?;
    let [commit] = commits.as_slice() else {
        return None;
    };
    if commit["commit"]["oid"].as_str()? != head.as_str() {
        return None;
    }
    let rollup = commit["commit"].get("statusCheckRollup")?;
    if rollup.is_null() {
        return Some(RequiredCheckPage {
            head,
            conclusions: vec![],
            after: None,
        });
    }
    let connection = &rollup["contexts"];
    let mut conclusions = Vec::new();
    for node in connection["nodes"].as_array()? {
        if !node["isRequired"].as_bool()? {
            continue;
        }
        let conclusion = match node["__typename"].as_str()? {
            "CheckRun" => match node.get("conclusion")?.as_str() {
                None if node["conclusion"].is_null() => None,
                Some("SUCCESS") => Some(CheckConclusion::Success),
                Some("NEUTRAL") => Some(CheckConclusion::Neutral),
                Some("SKIPPED") => Some(CheckConclusion::Skipped),
                Some("FAILURE") => Some(CheckConclusion::Failure),
                Some("CANCELLED") => Some(CheckConclusion::Cancelled),
                Some("TIMED_OUT") => Some(CheckConclusion::TimedOut),
                Some("ACTION_REQUIRED") => Some(CheckConclusion::ActionRequired),
                Some("STALE") => Some(CheckConclusion::Stale),
                Some("STARTUP_FAILURE") => Some(CheckConclusion::StartupFailure),
                _ => return None,
            },
            "StatusContext" => match node["state"].as_str()? {
                "SUCCESS" => Some(CheckConclusion::Success),
                "PENDING" | "EXPECTED" => None,
                "ERROR" | "FAILURE" => Some(CheckConclusion::Failure),
                _ => return None,
            },
            _ => return None,
        };
        conclusions.extend(conclusion);
    }
    conclusions.sort_unstable();
    conclusions.dedup();
    let after = if connection["pageInfo"]["hasNextPage"].as_bool()? {
        Some(connection["pageInfo"]["endCursor"].as_str()?.to_owned())
    } else {
        None
    };
    Some(RequiredCheckPage {
        head,
        conclusions,
        after,
    })
}

pub(crate) async fn fetch(
    io: &impl GitHubObservationRead,
    repository: &RepositorySlug,
    number: PullRequestNumber,
    head: &CommitSha,
) -> Result<Vec<CheckConclusion>, ObservationError> {
    let (owner, name) = repository
        .as_str()
        .split_once('/')
        .ok_or(ObservationError::InvalidResponse)?;
    let mut after: Option<String> = None;
    let mut conclusions = Vec::new();
    loop {
        let page = io
            .required_checks(json!({"query":QUERY,"variables":{
                "owner":owner,"name":name,"number":number.get(),"after":after,
            }}))
            .await?;
        if &page.head != head {
            return Err(ObservationError::HeadChanged);
        }
        conclusions.extend(page.conclusions);
        conclusions.sort_unstable();
        conclusions.dedup();
        match page.after {
            Some(next) if after.as_ref() != Some(&next) => after = Some(next),
            Some(_) => return Err(ObservationError::InvalidResponse),
            None => return Ok(conclusions),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    const HEAD: &str = "1111111111111111111111111111111111111111";

    fn response(nodes: Vec<Value>) -> Value {
        json!({"data":{"repository":{"pullRequest":{"headRefOid":HEAD,"commits":{"nodes":[{"commit":{"oid":HEAD,"statusCheckRollup":{"contexts":{"nodes":nodes,"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}]}}}}})
    }

    #[test]
    fn only_github_required_classification_admits_a_failed_check() {
        let value = response(vec![
            json!({"__typename":"CheckRun","name":"validate","isRequired":false,"conclusion":"FAILURE"}),
        ]);
        assert!(
            decode_response(&value)
                .expect("non-required check")
                .conclusions
                .is_empty()
        );
        let value = response(vec![
            json!({"__typename":"CheckRun","name":"report only","isRequired":true,"conclusion":"FAILURE"}),
        ]);
        assert_eq!(
            decode_response(&value).expect("required check").conclusions,
            vec![CheckConclusion::Failure]
        );
    }

    #[test]
    fn required_commit_status_failures_are_included() {
        let value = response(vec![
            json!({"__typename":"StatusContext","isRequired":true,"state":"ERROR"}),
        ]);
        assert_eq!(
            decode_response(&value)
                .expect("required status")
                .conclusions,
            vec![CheckConclusion::Failure]
        );
    }

    #[test]
    fn required_cancellation_and_timeout_keep_their_conclusions() {
        let value = response(vec![
            json!({"__typename":"CheckRun","isRequired":true,"conclusion":"CANCELLED"}),
            json!({"__typename":"CheckRun","isRequired":true,"conclusion":"TIMED_OUT"}),
            json!({"__typename":"CheckRun","isRequired":false,"conclusion":"FAILURE"}),
        ]);
        assert_eq!(
            decode_response(&value)
                .expect("required conclusions")
                .conclusions,
            vec![CheckConclusion::Cancelled, CheckConclusion::TimedOut]
        );
    }

    #[test]
    fn pending_required_checks_are_not_failed_checks() {
        let value = response(vec![
            json!({"__typename":"CheckRun","isRequired":true,"conclusion":null}),
            json!({"__typename":"StatusContext","isRequired":true,"state":"PENDING"}),
        ]);
        assert!(
            decode_response(&value)
                .expect("pending checks")
                .conclusions
                .is_empty()
        );
    }

    #[test]
    fn a_partial_graphql_error_does_not_admit_required_check_facts() {
        let mut value = response(vec![]);
        value["errors"] = json!([{"message":"partial response"}]);
        assert!(decode_response(&value).is_none());
    }

    struct Pages(Mutex<std::collections::VecDeque<RequiredCheckPage>>);
    impl GitHubObservationRead for Pages {
        async fn page(&self, _: &str) -> Result<(Value, bool), ObservationError> {
            panic!("no REST request in required-check observation")
        }
        async fn threads(&self, _: Value) -> Result<Value, ObservationError> {
            panic!("no thread request in required-check observation")
        }
        async fn required_checks(&self, _: Value) -> Result<RequiredCheckPage, ObservationError> {
            Ok(self
                .0
                .lock()
                .expect("pages")
                .pop_front()
                .expect("fixture page"))
        }
    }

    #[tokio::test]
    async fn a_required_failure_on_a_later_page_is_observed() {
        let head = CommitSha::try_new(HEAD.to_owned()).expect("head");
        let pages = Pages(Mutex::new(
            [
                RequiredCheckPage {
                    head: head.clone(),
                    conclusions: vec![],
                    after: Some("next".to_owned()),
                },
                RequiredCheckPage {
                    head: head.clone(),
                    conclusions: vec![CheckConclusion::Failure],
                    after: None,
                },
            ]
            .into(),
        ));
        let repository = RepositorySlug::try_new("example/project".to_owned()).expect("repository");
        assert_eq!(
            fetch(
                &pages,
                &repository,
                PullRequestNumber::new(std::num::NonZeroU64::MIN),
                &head
            )
            .await
            .expect("all pages"),
            vec![CheckConclusion::Failure]
        );
        assert!(pages.0.lock().expect("pages").is_empty());
    }

    #[tokio::test]
    async fn a_head_change_cannot_attach_required_check_facts_to_the_old_head() {
        let retained = CommitSha::try_new(HEAD.to_owned()).expect("retained head");
        let pages = Pages(Mutex::new(
            [RequiredCheckPage {
                head: CommitSha::try_new("2222222222222222222222222222222222222222".to_owned())
                    .expect("new head"),
                conclusions: vec![CheckConclusion::Failure],
                after: None,
            }]
            .into(),
        ));
        let repository = RepositorySlug::try_new("example/project".to_owned()).expect("repository");
        assert!(matches!(
            fetch(
                &pages,
                &repository,
                PullRequestNumber::new(std::num::NonZeroU64::MIN),
                &retained
            )
            .await,
            Err(ObservationError::HeadChanged)
        ));
    }
}
