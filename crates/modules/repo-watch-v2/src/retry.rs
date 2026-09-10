//! Cooldown retries of unfinished pull-request work, without inventing provider facts.
use crate::{DispatchAdmission, PlannedCommand, RepoWatchStore, StoreError};
use rust_decimal::Decimal;
use serde_json::Value;
use signalbox_session_ownership::{
    CheckConclusion, ChecksOutcome, LifecycleEventSource, MergeableState, OffsetDateTime,
    RepoWatchEvent, RepoWatchEventKindV1, RepoWatchEventTarget, RepoWatchPullRequestLifecycle,
    RepoWatchPullRequestState, RepoWatchRule, RepoWatchThreadState, RepositorySlug, SessionId,
};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

pub(crate) struct RetryAdmission {
    pub parent: Uuid,
    pub event: Vec<u8>,
    rule: RepoWatchRule,
}

#[derive(sqlx::FromRow)]
struct Candidate {
    dispatch_ref: Uuid,
    event_id: Uuid,
    event: Vec<u8>,
    sessions: Vec<Uuid>,
}

impl RepoWatchStore {
    /// Retries the latest ended dispatch when its pull-request work still matches.
    #[expect(
        clippy::too_many_arguments,
        reason = "Retry planning also reads push evidence through the lifecycle boundary."
    )]
    pub async fn retry_due<Ids, Factory, Codec>(
        &self,
        repository: &RepositorySlug,
        rule: &RepoWatchRule,
        ids: &mut Ids,
        factory: &mut Factory,
        codec: &mut Codec,
        source: &LifecycleEventSource,
        now: OffsetDateTime,
    ) -> Result<bool, crate::dispatch::EvaluationError<Factory::Error>>
    where
        Ids: crate::DispatchReferenceGenerator,
        Factory: crate::CreateSessionCommandFactory,
        Codec: crate::SessionCommandCodec,
    {
        use crate::dispatch::EvaluationError;
        let rows: Vec<Candidate> = sqlx::query_as(
            "WITH latest AS (
               SELECT DISTINCT ON (e.pull_request_number) d.* FROM dispatch_ledger d JOIN gh_event e USING(event_id)
               WHERE d.repository = $1 AND d.rule_id = $2 AND d.rule_revision = $3
                 AND d.command_kind = 'create_session' AND d.singleton_key IS NOT NULL AND e.pull_request_number IS NOT NULL
               ORDER BY e.pull_request_number, d.issued_at DESC, d.dispatch_ref DESC, d.action_ordinal
             )
             SELECT d.dispatch_ref, d.event_id, COALESCE(d.retry_event,e.normalized_payload) AS event,
               ARRAY(SELECT a.created_session_id FROM dispatch_ledger a WHERE a.dispatch_ref=d.dispatch_ref AND a.command_kind='create_session' AND a.created_session_id IS NOT NULL) AS sessions
             FROM latest d JOIN gh_event e USING(event_id)
             WHERE e.pull_request_number IS NOT NULL AND d.status='applied'
               AND NOT EXISTS (SELECT 1 FROM dispatch_ledger a WHERE a.dispatch_ref=d.dispatch_ref AND a.command_kind='create_session' AND (a.session_terminal_at IS NULL OR a.singleton_released_at IS NULL OR EXTRACT(EPOCH FROM ($4::timestamptz-a.singleton_released_at)) < $5))
             ORDER BY d.issued_at, d.dispatch_ref")
            .bind(repository.as_str()).bind(rule.id().as_str()).bind(Decimal::from(rule.version().get()))
            .bind(now).bind(Decimal::from(rule.cooldown().as_secs()))
            .fetch_all(&self.pool).await.map_err(StoreError::from).map_err(EvaluationError::Store)?;
        for candidate in rows {
            let mut pushed = false;
            for session in &candidate.sessions {
                pushed |= source
                    .session_pushed(SessionId::from_uuid(*session))
                    .await
                    .map_err(|error| EvaluationError::Store(StoreError::Lifecycle(error)))?;
            }
            if pushed {
                continue;
            }
            let previous = crate::event_decode::event(
                signalbox_session_ownership::RepoWatchEventId::from_uuid(candidate.event_id),
                &candidate.event,
            )
            .ok_or(EvaluationError::Store(StoreError::InvalidRetainedEvent))?;
            let baseline = self
                .ingest_baseline(repository)
                .await
                .map_err(EvaluationError::Store)?;
            let Some(observation) = baseline.observation.as_ref() else {
                continue;
            };
            let Some(event) = retry_event(rule, &previous, observation.state().pull_requests())
            else {
                continue;
            };
            let key =
                crate::dispatch::singleton_key(rule.singleton_per(), &event, Some(observation))
                    .ok_or(EvaluationError::Store(StoreError::InvalidDispatchBatch))?;
            let admission = RetryAdmission {
                parent: candidate.dispatch_ref,
                event: serde_json::to_vec(&crate::normalized_event_payload(&event))
                    .map_err(|_| EvaluationError::Store(StoreError::InvalidRetainedEvent))?,
                rule: rule.clone(),
            };
            let batches =
                crate::plan_repository_event(std::slice::from_ref(rule), &event, ids, factory)
                    .map_err(EvaluationError::Plan)?;
            let Some(batch) = batches.first() else {
                continue;
            };
            let mut tx = self
                .pool
                .begin()
                .await
                .map_err(StoreError::from)
                .map_err(EvaluationError::Store)?;
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
                .bind(crate::CONFIGURATION_LOCK)
                .execute(&mut *tx)
                .await
                .map_err(StoreError::from)
                .map_err(EvaluationError::Store)?;
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('frontier:' || $1,0))")
                .bind(repository.as_str())
                .execute(&mut *tx)
                .await
                .map_err(StoreError::from)
                .map_err(EvaluationError::Store)?;
            let outcome = self
                .record_commands_transaction(
                    &mut tx,
                    batch,
                    now,
                    codec,
                    Some((&key, rule.cooldown())),
                    Some(&admission),
                )
                .await
                .map_err(EvaluationError::Store)?;
            if matches!(outcome, DispatchAdmission::Inserted) {
                tx.commit()
                    .await
                    .map_err(StoreError::from)
                    .map_err(EvaluationError::Store)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) async fn retry_still_due(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        first: &PlannedCommand,
        retry: &RetryAdmission,
        now: OffsetDateTime,
        admission: Option<(&str, std::time::Duration)>,
    ) -> Result<bool, StoreError> {
        let Some((key, cooldown)) = admission else {
            return Ok(false);
        };
        let eligible: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM dispatch_ledger d WHERE d.dispatch_ref=$1 AND d.repository=$2 AND d.rule_id=$3 AND d.rule_revision=$4 AND d.command_kind='create_session' AND d.status='applied')
             AND NOT EXISTS (SELECT 1 FROM rule_evaluation_cursor WHERE repository=$2 AND rule_id=$3 AND rule_revision=$4 AND effect_id IS NOT NULL)
             AND NOT EXISTS (SELECT 1 FROM dispatch_ledger a WHERE a.dispatch_ref=$1 AND a.command_kind='create_session' AND (a.session_terminal_at IS NULL OR a.singleton_released_at IS NULL OR EXTRACT(EPOCH FROM ($5::timestamptz-a.singleton_released_at)) < $6))
             AND NOT EXISTS (SELECT 1 FROM dispatch_ledger later JOIN dispatch_ledger parent ON parent.dispatch_ref=$1 AND parent.command_kind='create_session' WHERE later.repository=$2 AND later.rule_id=$3 AND later.rule_revision=$4 AND EXISTS (SELECT 1 FROM gh_event le JOIN gh_event pe ON pe.event_id=parent.event_id WHERE le.event_id=later.event_id AND le.pull_request_number=pe.pull_request_number) AND later.command_kind='create_session' AND (later.issued_at,later.dispatch_ref) > (parent.issued_at,parent.dispatch_ref))")
            .bind(retry.parent).bind(first.repository().as_str()).bind(first.rule_id().as_str())
            .bind(Decimal::from(first.rule_revision().get())).bind(now).bind(Decimal::from(cooldown.as_secs()))
            .fetch_one(&mut **tx).await?;
        if !eligible {
            return Ok(false);
        }
        let payload: Option<Value> = sqlx::query_scalar(
            "SELECT comparison_baseline FROM repository_state WHERE repository=$1",
        )
        .bind(first.repository().as_str())
        .fetch_optional(&mut **tx)
        .await?
        .flatten();
        let Some(payload) = payload else {
            return Ok(false);
        };
        let observation = crate::observation_decode::observation(&payload)
            .ok_or(StoreError::InvalidRetainedEvent)?;
        let candidate = crate::event_decode::event(first.event_id(), &retry.event)
            .ok_or(StoreError::InvalidRetainedEvent)?;
        Ok(
            retry_event(&retry.rule, &candidate, observation.state().pull_requests()).as_ref()
                == Some(&candidate)
                && crate::dispatch::singleton_key(
                    retry.rule.singleton_per(),
                    &candidate,
                    Some(&observation),
                )
                .as_deref()
                    == Some(key),
        )
    }
}

fn retry_event(
    rule: &RepoWatchRule,
    previous: &RepoWatchEvent,
    current: &[RepoWatchPullRequestState],
) -> Option<RepoWatchEvent> {
    let RepoWatchEventTarget::PullRequest(origin) = previous.target() else {
        return None;
    };
    let current = current
        .iter()
        .find(|p| p.context().number() == origin.number())?;
    if current.lifecycle() != RepoWatchPullRequestLifecycle::Open {
        return None;
    }
    let failing = current
        .completed_check_suites()
        .iter()
        .any(|c| c.outcome() == ChecksOutcome::Failure)
        || current
            .completed_check_runs()
            .iter()
            .any(|c| failing_conclusion(c.conclusion()));
    let unresolved = current
        .threads()
        .iter()
        .any(|t| t.state() == RepoWatchThreadState::Open);
    let conflicting = current.mergeable_state() == MergeableState::Conflicting;
    if !(conflicting || unresolved || failing) {
        return None;
    }
    let kind = match previous.kind() {
        RepoWatchEventKindV1::HeadChanged {
            previous: before,
            current: dispatched,
        } => RepoWatchEventKindV1::HeadChanged {
            previous: if current.context().head_sha() == dispatched {
                before
            } else {
                dispatched
            }
            .clone(),
            current: current.context().head_sha().clone(),
        },
        RepoWatchEventKindV1::BaseAdvanced { .. } => RepoWatchEventKindV1::BaseAdvanced {
            branch: current.context().base_branch().clone(),
        },
        RepoWatchEventKindV1::MergeableStateChanged { .. } => {
            RepoWatchEventKindV1::MergeableStateChanged {
                current: current.mergeable_state(),
            }
        }
        RepoWatchEventKindV1::Labeled { label } if !current.context().labels().contains(label) => {
            return None;
        }
        RepoWatchEventKindV1::Unlabeled { label } if current.context().labels().contains(label) => {
            return None;
        }
        RepoWatchEventKindV1::ChecksCompleted { .. } => RepoWatchEventKindV1::ChecksCompleted {
            outcome: if failing {
                ChecksOutcome::Failure
            } else {
                ChecksOutcome::Success
            },
        },
        _ => previous.kind().clone(),
    };
    let event = RepoWatchEvent::try_pull_request(
        previous.id(),
        previous.repository().clone(),
        current.context().clone(),
        kind,
    )
    .ok()?;
    rule.matcher().matches(&event).then_some(event)
}

fn failing_conclusion(conclusion: CheckConclusion) -> bool {
    matches!(
        conclusion,
        CheckConclusion::Failure
            | CheckConclusion::Cancelled
            | CheckConclusion::TimedOut
            | CheckConclusion::ActionRequired
            | CheckConclusion::Stale
            | CheckConclusion::StartupFailure
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_session_ownership::{
        BranchName, CommitSha, LabelName, PullRequestBody, PullRequestEventContext,
        PullRequestEventContextInput, PullRequestNumber, PullRequestTitle, RepoWatchAuthorLogin,
        RepoWatchEventId, RepoWatchMatcherV1, RepoWatchMatcherV1Input,
        RepoWatchPullRequestStateInput, RepoWatchRuleActionV1, RepoWatchRuleId,
        RepoWatchRuleVersion, RepoWatchSingletonScope, RepoWatchThreadObservation, ReviewThreadId,
        SessionTemplateName,
    };
    use std::num::NonZeroU64;

    fn context(labels: Vec<LabelName>) -> PullRequestEventContext {
        PullRequestEventContext::new(context_input(labels))
    }

    fn context_input(labels: Vec<LabelName>) -> PullRequestEventContextInput {
        PullRequestEventContextInput {
            number: PullRequestNumber::new(NonZeroU64::MIN),
            head_sha: CommitSha::try_new("1111111111111111111111111111111111111111".to_owned())
                .expect("head"),
            head_repository: RepositorySlug::try_new("retry/project".to_owned())
                .expect("repository"),
            base_branch: BranchName::try_new("main".to_owned()).expect("base"),
            head_branch: BranchName::try_new("agent/retry".to_owned()).expect("head branch"),
            title: PullRequestTitle::try_new("Retry work".to_owned()).expect("title"),
            body: PullRequestBody::try_new(String::new()).expect("body"),
            labels,
            draft: false,
            author: None,
        }
    }

    fn input() -> RepoWatchPullRequestStateInput {
        RepoWatchPullRequestStateInput {
            context: context(vec![]),
            lifecycle: RepoWatchPullRequestLifecycle::Open,
            mergeable_state: MergeableState::Mergeable,
            completed_check_suites: vec![],
            completed_check_runs: vec![],
            reviews: vec![],
            threads: vec![],
            reactions: vec![],
        }
    }

    fn origin(kind: RepoWatchEventKindV1) -> (RepoWatchRule, RepoWatchEvent) {
        let labels = match &kind {
            RepoWatchEventKindV1::Labeled { label } => vec![label.clone()],
            _ => vec![],
        };
        let event = RepoWatchEvent::try_pull_request(
            RepoWatchEventId::from_uuid(Uuid::from_u128(1)),
            input().context.head_repository().clone(),
            context(labels),
            kind,
        )
        .expect("event");
        let rule = RepoWatchRule::try_new(
            RepoWatchRuleId::try_new("retry".to_owned()).expect("rule"),
            RepoWatchRuleVersion::V1,
            RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
                event_kinds: vec![event.kind().name()],
                ..Default::default()
            }),
            vec![RepoWatchRuleActionV1::DispatchSession {
                template: SessionTemplateName::try_new("retry".to_owned()).expect("template"),
            }],
            RepoWatchSingletonScope::PullRequest,
            std::time::Duration::from_secs(5),
        )
        .expect("rule");
        (rule, event)
    }

    fn thread_author() -> RepoWatchAuthorLogin {
        RepoWatchAuthorLogin::try_new("reviewer".to_owned()).expect("author")
    }

    #[test]
    fn head_change_retries_track_the_latest_observed_head() {
        let before = CommitSha::try_new("2222222222222222222222222222222222222222".to_owned())
            .expect("previous head");
        let dispatched = context(vec![]).head_sha().clone();
        let advanced = CommitSha::try_new("3333333333333333333333333333333333333333".to_owned())
            .expect("advanced head");
        let (rule, event) = origin(RepoWatchEventKindV1::HeadChanged {
            previous: before.clone(),
            current: dispatched.clone(),
        });
        for (head, previous) in [
            (advanced, dispatched.clone()),
            (before.clone(), dispatched.clone()),
            (dispatched, before),
        ] {
            let mut current = input();
            current.context = PullRequestEventContext::new(PullRequestEventContextInput {
                head_sha: head.clone(),
                ..context_input(vec![])
            });
            current.mergeable_state = MergeableState::Conflicting;
            let retry = retry_event(
                &rule,
                &event,
                &[RepoWatchPullRequestState::try_new(current).expect("pull")],
            )
            .expect("unfinished merge retries at the observed head");
            assert_eq!(
                retry.kind(),
                &RepoWatchEventKindV1::HeadChanged {
                    previous,
                    current: head,
                }
            );
        }
    }

    #[test]
    fn base_advance_retries_track_the_latest_target_branch() {
        let (rule, event) = origin(RepoWatchEventKindV1::BaseAdvanced {
            branch: context(vec![]).base_branch().clone(),
        });
        let base = BranchName::try_new("release".to_owned()).expect("new target branch");
        let mut current = input();
        current.context = PullRequestEventContext::new(PullRequestEventContextInput {
            base_branch: base.clone(),
            ..context_input(vec![])
        });
        current.mergeable_state = MergeableState::Conflicting;
        let retry = retry_event(
            &rule,
            &event,
            &[RepoWatchPullRequestState::try_new(current).expect("pull")],
        )
        .expect("unfinished merge retries after retargeting");
        assert_eq!(
            retry.kind(),
            &RepoWatchEventKindV1::BaseAdvanced { branch: base }
        );
    }

    #[test]
    fn resolved_mergeability_stops_the_conflict_retry() {
        let (rule, event) = origin(RepoWatchEventKindV1::MergeableStateChanged {
            current: MergeableState::Conflicting,
        });
        let mut current = input();
        current.mergeable_state = MergeableState::Mergeable;
        assert!(
            retry_event(
                &rule,
                &event,
                &[RepoWatchPullRequestState::try_new(current).expect("pull")]
            )
            .is_none()
        );
    }

    #[test]
    fn unresolved_threads_keep_a_resolved_conflict_dispatch_eligible() {
        let (rule, event) = origin(RepoWatchEventKindV1::MergeableStateChanged {
            current: MergeableState::Conflicting,
        });
        let mut current = input();
        current.mergeable_state = MergeableState::Mergeable;
        current.threads = vec![RepoWatchThreadObservation::open(
            ReviewThreadId::try_new("remaining-review".to_owned()).expect("thread"),
            thread_author(),
        )];
        let retry = retry_event(
            &rule,
            &event,
            &[RepoWatchPullRequestState::try_new(current).expect("pull")],
        )
        .expect("unfinished review work");
        assert_eq!(
            retry.kind(),
            &RepoWatchEventKindV1::MergeableStateChanged {
                current: MergeableState::Mergeable
            }
        );
    }

    #[test]
    fn failing_checks_keep_a_resolved_conflict_dispatch_eligible() {
        use signalbox_session_ownership::{
            GitHubObjectId, RepoWatchCheckCompletionGeneration, RepoWatchCheckSuiteObservation,
        };
        let (rule, event) = origin(RepoWatchEventKindV1::MergeableStateChanged {
            current: MergeableState::Conflicting,
        });
        let mut current = input();
        current.mergeable_state = MergeableState::Mergeable;
        current.completed_check_suites = vec![RepoWatchCheckSuiteObservation::new(
            GitHubObjectId::new(NonZeroU64::MIN),
            RepoWatchCheckCompletionGeneration::try_new("remaining-failure".to_owned())
                .expect("generation"),
            ChecksOutcome::Failure,
        )];
        assert!(
            retry_event(
                &rule,
                &event,
                &[RepoWatchPullRequestState::try_new(current).expect("pull")]
            )
            .is_some()
        );
    }

    #[test]
    fn a_conflict_keeps_a_passed_checks_dispatch_eligible() {
        let (rule, event) = origin(RepoWatchEventKindV1::ChecksCompleted {
            outcome: ChecksOutcome::Failure,
        });
        let mut current = input();
        current.mergeable_state = MergeableState::Conflicting;
        let retry = retry_event(
            &rule,
            &event,
            &[RepoWatchPullRequestState::try_new(current).expect("pull")],
        )
        .expect("unfinished merge work");
        assert_eq!(
            retry.kind(),
            &RepoWatchEventKindV1::ChecksCompleted {
                outcome: ChecksOutcome::Success
            }
        );
    }

    #[test]
    fn resolved_threads_stop_the_review_retry() {
        let thread = ReviewThreadId::try_new("thread".to_owned()).expect("thread");
        let (rule, event) = origin(RepoWatchEventKindV1::ThreadOpened {
            thread: thread.clone(),
            author: thread_author(),
        });
        let mut current = input();
        current.threads = vec![RepoWatchThreadObservation::resolved(
            thread,
            thread_author(),
            thread_author(),
        )];
        assert!(
            retry_event(
                &rule,
                &event,
                &[RepoWatchPullRequestState::try_new(current).expect("pull")]
            )
            .is_none()
        );
    }

    #[test]
    fn an_open_thread_keeps_the_review_retry_eligible() {
        let thread = ReviewThreadId::try_new("thread".to_owned()).expect("thread");
        let (rule, event) = origin(RepoWatchEventKindV1::ThreadOpened {
            thread: thread.clone(),
            author: thread_author(),
        });
        let mut current = input();
        current.threads = vec![RepoWatchThreadObservation::open(thread, thread_author())];
        assert!(
            retry_event(
                &rule,
                &event,
                &[RepoWatchPullRequestState::try_new(current).expect("pull")]
            )
            .is_some()
        );
    }

    #[test]
    fn removed_labels_stop_a_labeled_retry_even_while_conflicting() {
        let (rule, event) = origin(RepoWatchEventKindV1::Labeled {
            label: LabelName::try_new("repo-watch".to_owned()).expect("label"),
        });
        let mut current = input();
        assert!(current.context.labels().is_empty());
        current.mergeable_state = MergeableState::Conflicting;
        assert!(
            retry_event(
                &rule,
                &event,
                &[RepoWatchPullRequestState::try_new(current).expect("pull")]
            )
            .is_none()
        );
    }

    #[test]
    fn restored_labels_stop_an_unlabeled_retry_even_while_conflicting() {
        let label = LabelName::try_new("repo-watch".to_owned()).expect("label");
        let (rule, event) = origin(RepoWatchEventKindV1::Unlabeled {
            label: label.clone(),
        });
        for (labels, eligible) in [(vec![], true), (vec![label], false)] {
            let mut current = input();
            current.context = context(labels);
            current.mergeable_state = MergeableState::Conflicting;
            assert_eq!(
                retry_event(
                    &rule,
                    &event,
                    &[RepoWatchPullRequestState::try_new(current).expect("pull")]
                )
                .is_some(),
                eligible
            );
        }
    }

    #[test]
    fn passing_checks_stop_the_checks_retry() {
        use signalbox_session_ownership::{
            GitHubObjectId, RepoWatchCheckCompletionGeneration, RepoWatchCheckSuiteObservation,
        };
        let (rule, event) = origin(RepoWatchEventKindV1::ChecksCompleted {
            outcome: ChecksOutcome::Failure,
        });
        for (outcome, eligible) in [
            (ChecksOutcome::Failure, true),
            (ChecksOutcome::Success, false),
        ] {
            let mut current = input();
            current.completed_check_suites = vec![RepoWatchCheckSuiteObservation::new(
                GitHubObjectId::new(NonZeroU64::MIN),
                RepoWatchCheckCompletionGeneration::try_new("suite".to_owned())
                    .expect("generation"),
                outcome,
            )];
            assert_eq!(
                retry_event(
                    &rule,
                    &event,
                    &[RepoWatchPullRequestState::try_new(current).expect("pull")]
                )
                .is_some(),
                eligible
            );
        }
    }
}
