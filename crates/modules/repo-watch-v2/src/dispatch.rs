//! Durable rule evaluation and replayable session-command delivery.

use crate::{
    CreateSessionCommandFactory, DispatchAdmission, DispatchReferenceGenerator,
    PlanRepositoryEventError, RepoWatchStore, SessionCommandCodec, StoreError,
    plan_repository_event, plan_retained_lifecycle_reaction,
};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use signalbox_ownership_seam::{
    CreateSessionOutcome, DescendantTerminationScope, DurableCommandId, GoalEventKind,
    LifecycleEvent, LifecycleEventKind, OffsetDateTime, RepoWatchEvent, RepoWatchEventId,
    RepoWatchEventTarget, RepoWatchObservation, RepoWatchPullRequestLifecycle, RepoWatchRule,
    RepoWatchSingletonScope, RepositorySlug, SessionCommand, SessionId, SessionLifecycleCommand,
    SessionLifecycleOperation, SessionTerminalOutcome, StopStickiness,
};
use std::{collections::BTreeSet, future::Future};
use uuid::Uuid;

/// Core reserves identities for lifecycle commands just as it does for creation.
pub trait LifecycleCommandFactory {
    fn lifecycle(
        &mut self,
        session: SessionId,
        operation: SessionLifecycleOperation,
    ) -> SessionLifecycleCommand;
}

/// Synchronous delivery result; accepted commands settle through the lifecycle source.
pub enum CommandSubmission {
    Creation(CreateSessionOutcome),
    Accepted,
    ConflictingReuse,
}

/// Daemon-owned adapter to ordinary core command handlers.
pub trait SessionCommandSink {
    type Error;
    fn submit(
        &mut self,
        command: SessionCommand,
    ) -> impl Future<Output = Result<CommandSubmission, Self::Error>> + Send;
}

#[derive(Debug)]
pub enum EvaluationError<E> {
    Store(StoreError),
    Plan(PlanRepositoryEventError<E>),
    ConflictingDispatch,
}

#[derive(Debug)]
pub enum SubmissionError<E> {
    Store(StoreError),
    Sink(E),
}

/// The durable event and its repository-local ordinal selected for a rule revision.
pub struct RuleEvent {
    pub ordinal: u64,
    pub event: RepoWatchEvent,
}

impl RepoWatchStore {
    /// Reads only the next unevaluated fact after this revision's activation tail.
    pub async fn next_rule_event(
        &self,
        repository: &RepositorySlug,
        rule: &RepoWatchRule,
    ) -> Result<Option<RuleEvent>, StoreError> {
        let row: Option<(Decimal, Uuid, Vec<u8>)> = sqlx::query_as(
            "SELECT event.repository_event_ordinal, event.event_id, event.normalized_payload
             FROM rule AS active JOIN rule_revision AS revision
               ON revision.repository = active.repository AND revision.rule_id = active.rule_id
                AND revision.revision = active.active_revision
             LEFT JOIN rule_evaluation_cursor AS cursor
               ON cursor.repository = active.repository AND cursor.rule_id = active.rule_id
                AND cursor.rule_revision = active.active_revision
             JOIN gh_event AS event ON event.repository = active.repository
              AND event.repository_event_ordinal > GREATEST(revision.activated_after_event_ordinal, COALESCE(cursor.event_ordinal, 0))
             WHERE active.repository = $1 AND active.rule_id = $2 AND active.active_revision = $3
             ORDER BY event.repository_event_ordinal LIMIT 1")
            .bind(repository.as_str()).bind(rule.id().as_str()).bind(Decimal::from(rule.version().get()))
            .fetch_optional(&self.pool).await?;
        row.map(|(ordinal, id, payload)| {
            Ok(RuleEvent {
                ordinal: ordinal.to_u64().ok_or(StoreError::InvalidRetainedEvent)?,
                event: crate::event_decode::event(RepoWatchEventId::from_uuid(id), &payload)
                    .ok_or(StoreError::InvalidRetainedEvent)?,
            })
        })
        .transpose()
    }

    /// Evaluates a fact, durably records commands or suppression, then advances this revision.
    pub async fn evaluate_next<
        Ids: DispatchReferenceGenerator,
        Factory: CreateSessionCommandFactory,
        Codec: SessionCommandCodec,
    >(
        &self,
        repository: &RepositorySlug,
        rule: &RepoWatchRule,
        ids: &mut Ids,
        factory: &mut Factory,
        codec: &mut Codec,
        now: OffsetDateTime,
    ) -> Result<bool, EvaluationError<Factory::Error>> {
        let Some(next) = self
            .next_rule_event(repository, rule)
            .await
            .map_err(EvaluationError::Store)?
        else {
            return Ok(false);
        };
        let batches = plan_repository_event(std::slice::from_ref(rule), &next.event, ids, factory)
            .map_err(EvaluationError::Plan)?;
        for batch in batches {
            let baseline = self
                .ingest_baseline(repository)
                .await
                .map_err(EvaluationError::Store)?;
            let key = singleton_key(
                rule.singleton_per(),
                &next.event,
                baseline.observation.as_ref(),
            )
            .ok_or(EvaluationError::Store(StoreError::InvalidDispatchBatch))?;
            let outcome = self
                .record_rule_commands(&batch, now, codec, &key, rule.cooldown())
                .await
                .map_err(EvaluationError::Store)?;
            if matches!(outcome, DispatchAdmission::ConflictingReuse) {
                return Err(EvaluationError::ConflictingDispatch);
            }
        }
        sqlx::query("INSERT INTO rule_evaluation_cursor(repository, rule_id, rule_revision, event_ordinal)
            VALUES ($1,$2,$3,$4) ON CONFLICT (repository,rule_id,rule_revision)
            DO UPDATE SET event_ordinal = GREATEST(rule_evaluation_cursor.event_ordinal, EXCLUDED.event_ordinal)")
            .bind(repository.as_str()).bind(rule.id().as_str()).bind(Decimal::from(rule.version().get())).bind(Decimal::from(next.ordinal))
            .execute(&self.pool).await.map_err(StoreError::from).map_err(EvaluationError::Store)?;
        Ok(true)
    }

    /// Applies one lifecycle fact and commits any reaction before acknowledging its source.
    pub async fn react_to_lifecycle<
        Factory: LifecycleCommandFactory,
        Codec: SessionCommandCodec,
    >(
        &self,
        event: &LifecycleEvent,
        factory: &mut Factory,
        codec: &mut Codec,
    ) -> Result<(), StoreError> {
        self.apply_lifecycle_event(event).await?;
        if let Some(session) = event.session() {
            if matches!(event.kind(), LifecycleEventKind::SessionTerminal(terminal)
                if !matches!(terminal.outcome, SessionTerminalOutcome::Stopped { sticky: StopStickiness::Sticky }))
            {
                sqlx::query("UPDATE dispatch_ledger SET singleton_released_at = $2 WHERE created_session_id = $1 AND singleton_released_at IS NULL")
                    .bind(session.into_uuid()).bind(event.recorded_at()).execute(&self.pool).await?;
            }
            let operation = match event.kind() {
                LifecycleEventKind::GoalChanged(change) => match change.kind {
                    GoalEventKind::Commissioned | GoalEventKind::Resumed => {
                        Some(SessionLifecycleOperation::ReleaseStart)
                    }
                    GoalEventKind::Achieved | GoalEventKind::UserStopped => {
                        Some(SessionLifecycleOperation::Stop {
                            sticky: StopStickiness::Sticky,
                            descendant_scope: DescendantTerminationScope::ParentAlone,
                        })
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some(operation) = operation
                && let Some(origin) = self.reaction_origin_for_session(session).await?
            {
                let planned = plan_retained_lifecycle_reaction(
                    event,
                    &origin,
                    factory.lifecycle(session, operation),
                )
                .map_err(|_| StoreError::InvalidDispatchBatch)?;
                if matches!(
                    self.record_commands(&[planned], event.recorded_at(), codec)
                        .await?,
                    DispatchAdmission::ConflictingReuse
                ) {
                    return Err(StoreError::InvalidDispatchBatch);
                }
            }
        }
        let prior: Decimal =
            sqlx::query_scalar("SELECT applied_through FROM core_event_cursor WHERE singleton")
                .fetch_one(&self.pool)
                .await?;
        self.advance_core_event(
            prior
                .to_u64()
                .ok_or(StoreError::InvalidEventEvaluationPosition)?,
            event.sequence(),
        )
        .await?;
        Ok(())
    }

    /// Resubmits the exact committed payloads, including actions of removed rules.
    pub async fn submit_pending<Codec: SessionCommandCodec, Sink: SessionCommandSink>(
        &self,
        codec: &mut Codec,
        sink: &mut Sink,
    ) -> Result<(), SubmissionError<Sink::Error>> {
        for planned in self
            .recover_pending_commands(codec)
            .await
            .map_err(SubmissionError::Store)?
        {
            let id = planned.command().command_id();
            self.set_submission_pending(id, true)
                .await
                .map_err(SubmissionError::Store)?;
            match sink
                .submit(planned.into_command())
                .await
                .map_err(SubmissionError::Sink)?
            {
                CommandSubmission::Creation(outcome) => {
                    self.apply_create_session_outcome(&outcome, OffsetDateTime::now_utc())
                        .await
                        .map_err(SubmissionError::Store)?;
                }
                CommandSubmission::Accepted => {}
                CommandSubmission::ConflictingReuse => {
                    self.reject_command_conflict(id)
                        .await
                        .map_err(SubmissionError::Store)?;
                }
            }
            self.set_submission_pending(id, false)
                .await
                .map_err(SubmissionError::Store)?;
        }
        Ok(())
    }

    async fn set_submission_pending(
        &self,
        command: DurableCommandId,
        pending: bool,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE dispatch_ledger SET submission_pending = $2 WHERE command_id = $1")
            .bind(command.into_uuid())
            .bind(pending)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn reject_command_conflict(&self, command: DurableCommandId) -> Result<(), StoreError> {
        sqlx::query("UPDATE dispatch_ledger SET status = 'rejected', rejection_kind = 'conflicting_reuse', settled_at = $2
            WHERE command_id = $1 AND status = 'pending'")
            .bind(command.into_uuid()).bind(OffsetDateTime::now_utc()).execute(&self.pool).await?;
        Ok(())
    }
}

fn singleton_key(
    scope: RepoWatchSingletonScope,
    event: &RepoWatchEvent,
    observation: Option<&RepoWatchObservation>,
) -> Option<String> {
    let repository = event.repository().as_str();
    Some(match scope {
        RepoWatchSingletonScope::Rule => String::from("rule"),
        RepoWatchSingletonScope::Repository => format!("repo:{repository}"),
        RepoWatchSingletonScope::PullRequest | RepoWatchSingletonScope::Stack => {
            let RepoWatchEventTarget::PullRequest(context) = event.target() else {
                return None;
            };
            let number = if scope == RepoWatchSingletonScope::Stack {
                stack_root(event.repository(), context, observation)
            } else {
                context.number()
            };
            format!("pr:{repository}:{}", number.get())
        }
    })
}

fn stack_root(
    repository: &RepositorySlug,
    context: &signalbox_ownership_seam::PullRequestEventContext,
    observation: Option<&RepoWatchObservation>,
) -> signalbox_ownership_seam::PullRequestNumber {
    let open = observation
        .into_iter()
        .flat_map(|o| o.state().pull_requests())
        .filter(|p| p.lifecycle() == RepoWatchPullRequestLifecycle::Open)
        .map(|p| p.context())
        .collect::<Vec<_>>();
    let mut pending = BTreeSet::from([context.number()]);
    let mut visited = BTreeSet::new();
    let mut component = BTreeSet::new();
    while let Some(number) = pending.pop_first() {
        if !visited.insert(number) {
            continue;
        }
        let found = open.iter().find(|p| p.number() == number).copied();
        let candidate = found.unwrap_or(context);
        if found.is_some() {
            component.insert(number);
        }
        pending.extend(
            open.iter()
                .filter(|p| {
                    p.head_repository() == repository && p.head_branch() == candidate.base_branch()
                })
                .map(|p| p.number()),
        );
        if found.is_some() && candidate.head_repository() == repository {
            pending.extend(
                open.iter()
                    .filter(|p| p.base_branch() == candidate.head_branch())
                    .map(|p| p.number()),
            );
        }
    }
    component
        .iter()
        .filter_map(|number| {
            open.iter()
                .find(|p| p.number() == *number)
                .map(|candidate| (*number, candidate))
        })
        .find(|(_, candidate)| {
            !open.iter().any(|p| {
                p.head_repository() == repository && p.head_branch() == candidate.base_branch()
            })
        })
        .map(|(number, _)| number)
        .or_else(|| component.first().copied())
        .unwrap_or(context.number())
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_ownership_seam::{
        BranchName, CommitSha, MergeableState, PullRequestBody, PullRequestEventContext,
        PullRequestEventContextInput, PullRequestNumber, PullRequestTitle, RepoWatchEventKindV1,
        RepoWatchPullRequestState, RepoWatchPullRequestStateInput, RepoWatchRepositoryState,
        RepoWatchRepositoryStateInput,
    };
    use std::num::NonZeroU64;

    fn context(
        number: u64,
        head_repository: &str,
        base: &str,
        head: &str,
    ) -> PullRequestEventContext {
        PullRequestEventContext::new(PullRequestEventContextInput {
            number: PullRequestNumber::new(
                NonZeroU64::new(number).expect("positive fixture number"),
            ),
            head_sha: CommitSha::try_new(String::from("1111111111111111111111111111111111111111"))
                .expect("fixture head"),
            head_repository: RepositorySlug::try_new(head_repository.to_owned())
                .expect("repository"),
            base_branch: BranchName::try_new(base.to_owned()).expect("base"),
            head_branch: BranchName::try_new(head.to_owned()).expect("head"),
            title: PullRequestTitle::try_new(String::from("Stack fixture")).expect("title"),
            body: PullRequestBody::try_new(String::new()).expect("body"),
            labels: Vec::new(),
            draft: false,
            author: None,
        })
    }

    #[test]
    fn stack_singletons_share_the_open_root_but_do_not_join_a_fork_branch() {
        let repository =
            RepositorySlug::try_new(String::from("owner/project")).expect("repository");
        // PR identities are arbitrary; matching branch names form the parent-child relation.
        let parent = context(1, "owner/project", "main", "parent");
        let child = context(2, "owner/project", "parent", "child");
        let fork = context(3, "fork/project", "main", "child");
        let observation = RepoWatchObservation::new(
            Vec::new(),
            RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
                pull_requests: [&parent, &child, &fork]
                    .into_iter()
                    .map(|context| {
                        RepoWatchPullRequestState::try_new(RepoWatchPullRequestStateInput {
                            context: context.clone(),
                            lifecycle: RepoWatchPullRequestLifecycle::Open,
                            mergeable_state: MergeableState::Unknown,
                            completed_check_suites: Vec::new(),
                            completed_check_runs: Vec::new(),
                            reviews: Vec::new(),
                            threads: Vec::new(),
                            reactions: Vec::new(),
                        })
                        .expect("pull request")
                    })
                    .collect(),
                workflow_runs: Vec::new(),
                branch_heads: Vec::new(),
            })
            .expect("observation"),
        );
        let event = |context| {
            RepoWatchEvent::try_pull_request(
                RepoWatchEventId::from_uuid(Uuid::from_u128(10)),
                repository.clone(),
                context,
                RepoWatchEventKindV1::PullRequestOpened,
            )
            .expect("event")
        };
        let parent = event(parent);
        let child = event(child);
        let fork = event(fork);
        let key = |scope, event: &RepoWatchEvent| singleton_key(scope, event, Some(&observation));
        assert_eq!(
            key(RepoWatchSingletonScope::Stack, &parent),
            key(RepoWatchSingletonScope::Stack, &child)
        );
        assert_ne!(
            key(RepoWatchSingletonScope::Stack, &child),
            key(RepoWatchSingletonScope::Stack, &fork)
        );
        assert_ne!(
            key(RepoWatchSingletonScope::PullRequest, &parent),
            key(RepoWatchSingletonScope::PullRequest, &child)
        );
        assert_eq!(
            key(RepoWatchSingletonScope::Repository, &parent),
            key(RepoWatchSingletonScope::Repository, &fork)
        );
    }
}
