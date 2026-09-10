//! Checked workflow operations and receipts on the authoritative module records.
//! Governed by docs/spec/repo-watch.md.

use crate::{
    CreateSessionCommandFactory, DispatchAdmission, DispatchReferenceGenerator, RepoWatchStore,
    SessionCommandCodec, StoreError, plan_repository_event,
};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde_json::{Value, json};
use signalbox_session_ownership::{
    OffsetDateTime, RepoWatchDispatchId, RepoWatchEvent, RepoWatchEventId, RepoWatchRule,
    RepoWatchRuleActionV1, RepoWatchRuleId, RepoWatchRuleVersion, RepoWatchSingletonScope,
    RepositorySlug, SessionTemplateName,
};
use sqlx::{Postgres, Transaction};
use std::num::NonZeroU64;
use uuid::Uuid;

mod codec;

/// Checked revision and event delivered to pure program planning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuleContext {
    pub rule: RepoWatchRule,
    pub event: RepoWatchEvent,
    pub ordinal: u64,
    pub singleton_key: Option<String>,
}

impl RuleContext {
    /// The exact ordered templates selected by the existing pure matcher.
    pub fn plan(&self) -> Vec<SessionTemplateName> {
        if self.rule.matcher().matches(&self.event) {
            self.rule
                .actions()
                .iter()
                .map(|action| {
                    let RepoWatchRuleActionV1::DispatchSession { template } = action;
                    template.clone()
                })
                .collect()
        } else {
            Vec::new()
        }
    }
}

/// Stable module identity; the exact request bytes include its method and checked input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectReceipt {
    pub effect: Uuid,
    pub input: Vec<u8>,
    pub result: Vec<u8>,
}

/// Checked result of one rule evaluation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvaluationOutcome {
    Nonmatch,
    Suppressed,
    Dispatched(RepoWatchDispatchId),
}

impl EvaluationOutcome {
    pub fn encode(self) -> Vec<u8> {
        match self {
            Self::Nonmatch => b"nonmatch".to_vec(),
            Self::Suppressed => b"suppressed".to_vec(),
            Self::Dispatched(id) => format!("dispatched:{}", id.into_uuid()).into_bytes(),
        }
    }
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        match bytes {
            b"nonmatch" => Some(Self::Nonmatch),
            b"suppressed" => Some(Self::Suppressed),
            _ => Some(Self::Dispatched(RepoWatchDispatchId::from_uuid(
                Uuid::parse_str(
                    std::str::from_utf8(bytes)
                        .ok()?
                        .strip_prefix("dispatched:")?,
                )
                .ok()?,
            ))),
        }
    }
}

/// One checked evaluation invocation with host-owned time and exact encoded input.
pub struct EvaluationInvocation<'a> {
    pub effect: Uuid,
    pub input: &'a [u8],
    pub context: &'a RuleContext,
    pub plan: &'a [SessionTemplateName],
    pub now: OffsetDateTime,
}

/// One submission bound to an existing dispatch and stable effect identity.
pub struct SubmissionInvocation<'a> {
    pub effect: Uuid,
    pub input: &'a [u8],
    pub dispatch: RepoWatchDispatchId,
}

/// Retained submission binding, including an interrupted operation with no result yet.
pub struct SubmissionReceipt {
    pub effect: Uuid,
    pub input: Vec<u8>,
    pub result: Option<Vec<u8>>,
}

impl RepoWatchStore {
    /// Reads checked context without reserving a dispatch or replacing an unadopted receipt.
    pub async fn next_rule_context(
        &self,
        repository: &RepositorySlug,
        rule: &RepoWatchRule,
    ) -> Result<Option<RuleContext>, StoreError> {
        let mut tx = self.pool.begin().await?;
        configuration_lock(&mut tx).await?;
        frontier_lock(&mut tx, repository).await?;
        let eligible: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM rule active JOIN rule_revision revision ON revision.repository = active.repository AND revision.rule_id = active.rule_id AND revision.revision = active.active_revision WHERE active.repository = $1 AND active.rule_id = $2 AND active.active_revision = $3 AND revision.content_digest = $4) AND NOT EXISTS (SELECT 1 FROM rule_evaluation_cursor WHERE repository = $1 AND rule_id = $2 AND rule_revision = $3 AND effect_id IS NOT NULL)")
            .bind(repository.as_str()).bind(rule.id().as_str()).bind(Decimal::from(rule.version().get())).bind(rule.content_digest().as_bytes().as_slice()).fetch_one(&mut *tx).await?;
        if !eligible {
            return Ok(None);
        }
        let next: Option<(Decimal, Uuid, Vec<u8>)> = sqlx::query_as("SELECT event.repository_event_ordinal, event.event_id, event.normalized_payload FROM rule_revision revision LEFT JOIN rule_evaluation_cursor cursor ON cursor.repository = revision.repository AND cursor.rule_id = revision.rule_id AND cursor.rule_revision = revision.revision JOIN gh_event event ON event.repository = revision.repository AND event.repository_event_ordinal > GREATEST(revision.activated_after_event_ordinal, COALESCE(cursor.event_ordinal, 0)) WHERE revision.repository = $1 AND revision.rule_id = $2 AND revision.revision = $3 ORDER BY event.repository_event_ordinal LIMIT 1")
            .bind(repository.as_str()).bind(rule.id().as_str()).bind(Decimal::from(rule.version().get())).fetch_optional(&mut *tx).await?;
        let Some((ordinal, id, bytes)) = next else {
            return Ok(None);
        };
        let event = crate::event_decode::event(RepoWatchEventId::from_uuid(id), &bytes)
            .ok_or(StoreError::InvalidRetainedEvent)?;
        let observation = baseline(&mut tx, repository).await?;
        let key =
            crate::dispatch::singleton_key(rule.singleton_per(), &event, observation.as_ref());
        Ok(Some(RuleContext {
            rule: rule.clone(),
            event,
            ordinal: ordinal
                .to_u64()
                .ok_or(StoreError::InvalidEventEvaluationPosition)?,
            singleton_key: key,
        }))
    }

    /// Finds committed evaluations before consulting configuration, including nonmatches.
    pub async fn evaluation_receipts(&self) -> Result<Vec<EffectReceipt>, StoreError> {
        let rows: Vec<(Uuid, Vec<u8>, Vec<u8>)> = sqlx::query_as("SELECT effect_id, effect_input, effect_result FROM rule_evaluation_cursor WHERE effect_id IS NOT NULL ORDER BY repository, rule_id, rule_revision").fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|(effect, input, result)| EffectReceipt {
                effect,
                input,
                result,
            })
            .collect())
    }

    /// Equal recovery adopts the retained result; changed input conflicts.
    pub async fn adopt_evaluation(
        &self,
        effect: Uuid,
        input: &[u8],
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let mut tx = self.pool.begin().await?;
        receipt_lock(&mut tx, effect).await?;
        evaluation_receipt(&mut tx, effect, input).await
    }

    /// Retains the verified result while releasing its pending evaluation slot.
    pub async fn release_evaluation(&self, receipt: &EffectReceipt) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        receipt_lock(&mut tx, receipt.effect).await?;
        sqlx::query("WITH released AS (UPDATE rule_evaluation_cursor SET effect_id = NULL, effect_input = NULL, effect_result = NULL WHERE effect_id = $1 AND effect_input = $2 AND effect_result = $3 RETURNING $1::uuid AS effect_id, $2::bytea AS effect_input, $3::bytea AS effect_result) INSERT INTO workflow_effect_result(effect_id, method, effect_input, effect_result) SELECT effect_id, 'repo.commitEvaluation', effect_input, effect_result FROM released")
            .bind(receipt.effect).bind(&receipt.input).bind(&receipt.result).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Revalidates planning, freezes host commands and commits the outcome and cursor together.
    pub async fn commit_evaluation<
        Ids: DispatchReferenceGenerator,
        Factory: CreateSessionCommandFactory,
        Codec: SessionCommandCodec,
    >(
        &self,
        invocation: EvaluationInvocation<'_>,
        ids: &mut Ids,
        factory: &mut Factory,
        codec: &mut Codec,
    ) -> Result<Vec<u8>, StoreError> {
        let EvaluationInvocation {
            effect,
            input,
            context,
            plan,
            now,
        } = invocation;
        let mut tx = self.pool.begin().await?;
        receipt_lock(&mut tx, effect).await?;
        if let Some(result) = evaluation_receipt(&mut tx, effect, input).await? {
            return Ok(result);
        }
        configuration_lock(&mut tx).await?;
        let rule = &context.rule;
        let event = &context.event;
        frontier_lock(&mut tx, event.repository()).await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(length($1)::text || ':' || $1 || $2, 0))")
            .bind(event.repository().as_str()).bind(rule.id().as_str()).execute(&mut *tx).await?;
        let next: Option<(Decimal, Uuid, Vec<u8>)> = sqlx::query_as("SELECT event.repository_event_ordinal, event.event_id, event.normalized_payload FROM rule active JOIN rule_revision revision ON revision.repository = active.repository AND revision.rule_id = active.rule_id AND revision.revision = active.active_revision LEFT JOIN rule_evaluation_cursor cursor ON cursor.repository = active.repository AND cursor.rule_id = active.rule_id AND cursor.rule_revision = active.active_revision JOIN gh_event event ON event.repository = active.repository AND event.repository_event_ordinal > GREATEST(revision.activated_after_event_ordinal, COALESCE(cursor.event_ordinal, 0)) WHERE active.repository = $1 AND active.rule_id = $2 AND active.active_revision = $3 AND revision.content_digest = $4 AND cursor.effect_id IS NULL ORDER BY event.repository_event_ordinal LIMIT 1")
            .bind(event.repository().as_str()).bind(rule.id().as_str()).bind(Decimal::from(rule.version().get())).bind(rule.content_digest().as_bytes().as_slice()).fetch_optional(&mut *tx).await?;
        let Some((ordinal, id, bytes)) = next else {
            return Err(StoreError::WorkflowInputRejected);
        };
        let retained_ordinal = ordinal
            .to_u64()
            .ok_or(StoreError::InvalidEventEvaluationPosition)?;
        let retained_event = crate::event_decode::event(RepoWatchEventId::from_uuid(id), &bytes)
            .ok_or(StoreError::InvalidRetainedEvent)?;
        if retained_ordinal != context.ordinal || &retained_event != event || context.plan() != plan
        {
            return Err(StoreError::WorkflowInputRejected);
        }
        let observation = baseline(&mut tx, event.repository()).await?;
        if crate::dispatch::singleton_key(rule.singleton_per(), event, observation.as_ref())
            != context.singleton_key
        {
            return Err(StoreError::WorkflowInputRejected);
        }
        let batches = plan_repository_event(std::slice::from_ref(rule), event, ids, factory)
            .map_err(|_| StoreError::InvalidDispatchBatch)?;
        let outcome = if let Some(batch) = batches.first() {
            match self
                .record_commands_transaction(
                    &mut tx,
                    batch,
                    now,
                    codec,
                    Some((
                        context
                            .singleton_key
                            .as_deref()
                            .ok_or(StoreError::InvalidDispatchBatch)?,
                        rule.cooldown(),
                    )),
                    None,
                )
                .await?
            {
                DispatchAdmission::Inserted => EvaluationOutcome::Dispatched(batch[0].dispatch()),
                DispatchAdmission::Suppressed => EvaluationOutcome::Suppressed,
                DispatchAdmission::Replayed { .. }
                | DispatchAdmission::InactiveRule
                | DispatchAdmission::ConflictingReuse => {
                    return Err(StoreError::InvalidDispatchBatch);
                }
            }
        } else {
            EvaluationOutcome::Nonmatch
        };
        let result = outcome.encode();
        sqlx::query("INSERT INTO rule_evaluation_cursor(repository, rule_id, rule_revision, event_ordinal, effect_id, effect_input, effect_result) VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT (repository,rule_id,rule_revision) DO UPDATE SET event_ordinal = EXCLUDED.event_ordinal, effect_id = EXCLUDED.effect_id, effect_input = EXCLUDED.effect_input, effect_result = EXCLUDED.effect_result")
            .bind(event.repository().as_str()).bind(rule.id().as_str()).bind(Decimal::from(rule.version().get())).bind(ordinal).bind(effect).bind(input).bind(&result).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(result)
    }
}

pub(crate) async fn completed_receipt(
    tx: &mut Transaction<'_, Postgres>,
    effect: Uuid,
    method: &str,
    input: &[u8],
) -> Result<Option<Vec<u8>>, StoreError> {
    let conflicting: bool = sqlx::query_scalar("SELECT CASE WHEN $2 = 'repo.observe' THEN EXISTS (SELECT 1 FROM rule_evaluation_cursor WHERE effect_id=$1) OR EXISTS (SELECT 1 FROM dispatch_ledger WHERE effect_id=$1) ELSE EXISTS (SELECT 1 FROM repository_state WHERE observation_effect_id=$1) END")
        .bind(effect).bind(method).fetch_one(&mut **tx).await?;
    if conflicting {
        return Err(StoreError::WorkflowInputRejected);
    }
    let row: Option<(String, Vec<u8>, Vec<u8>)> = sqlx::query_as(
        "SELECT method, effect_input, effect_result FROM workflow_effect_result WHERE effect_id = $1",
    )
    .bind(effect)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|(retained_method, retained_input, result)| {
        if retained_method == method && retained_input == input {
            Ok(result)
        } else {
            Err(StoreError::WorkflowInputRejected)
        }
    })
    .transpose()
}

pub(crate) async fn receipt_lock(
    tx: &mut Transaction<'_, Postgres>,
    effect: Uuid,
) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('repo-effect:' || $1::text, 0))")
        .bind(effect)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn configuration_lock(tx: &mut Transaction<'_, Postgres>) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(crate::CONFIGURATION_LOCK)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn frontier_lock(
    tx: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('frontier:' || $1, 0))")
        .bind(repository.as_str())
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn evaluation_receipt(
    tx: &mut Transaction<'_, Postgres>,
    effect: Uuid,
    input: &[u8],
) -> Result<Option<Vec<u8>>, StoreError> {
    if let Some(result) = completed_receipt(tx, effect, "repo.commitEvaluation", input).await? {
        return Ok(Some(result));
    }
    let submission: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM dispatch_ledger WHERE effect_id = $1)")
            .bind(effect)
            .fetch_one(&mut **tx)
            .await?;
    if submission {
        return Err(StoreError::WorkflowInputRejected);
    }
    let row: Option<(Vec<u8>, Vec<u8>)> = sqlx::query_as(
        "SELECT effect_input, effect_result FROM rule_evaluation_cursor WHERE effect_id = $1",
    )
    .bind(effect)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|(retained, result)| {
        if retained == input {
            Ok(result)
        } else {
            Err(StoreError::WorkflowInputRejected)
        }
    })
    .transpose()
}

impl RepoWatchStore {
    /// Binds submission to a retained dispatch before invoking its existing recovery path.
    /// A bound, incomplete receipt is resumable; the entire operation is not idempotent.
    pub async fn submit_dispatch<
        Codec: SessionCommandCodec,
        Sink: crate::dispatch::SessionCommandSink,
    >(
        &self,
        invocation: SubmissionInvocation<'_>,
        codec: &mut Codec,
        sink: &mut Sink,
        source: &signalbox_session_ownership::LifecycleEventSource,
    ) -> Result<Vec<u8>, crate::dispatch::SubmissionError<Sink::Error>> {
        let SubmissionInvocation {
            effect,
            input,
            dispatch,
        } = invocation;
        use crate::dispatch::SubmissionError;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(StoreError::from)
            .map_err(SubmissionError::Store)?;
        receipt_lock(&mut tx, effect)
            .await
            .map_err(SubmissionError::Store)?;
        if let Some(result) = completed_receipt(&mut tx, effect, "repo.submitPending", input)
            .await
            .map_err(SubmissionError::Store)?
        {
            return Ok(result);
        }
        let evaluation: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM rule_evaluation_cursor WHERE effect_id = $1)",
        )
        .bind(effect)
        .fetch_one(&mut *tx)
        .await
        .map_err(StoreError::from)
        .map_err(SubmissionError::Store)?;
        if evaluation {
            return Err(SubmissionError::Store(StoreError::WorkflowInputRejected));
        }
        let retained: Option<(Uuid, Vec<u8>, Option<Vec<u8>>)> = sqlx::query_as("SELECT dispatch_ref, effect_input, effect_result FROM dispatch_ledger WHERE effect_id = $1").bind(effect).fetch_optional(&mut *tx).await.map_err(StoreError::from).map_err(SubmissionError::Store)?;
        if let Some((retained_dispatch, retained_input, result)) = retained {
            if retained_dispatch != dispatch.into_uuid() || retained_input != input {
                return Err(SubmissionError::Store(StoreError::WorkflowInputRejected));
            }
            if let Some(result) = result {
                return Ok(result);
            }
        } else {
            let changed = sqlx::query("UPDATE dispatch_ledger SET effect_id = $1, effect_input = $2 WHERE command_id = (SELECT command_id FROM dispatch_ledger WHERE dispatch_ref = $3 AND trigger_sequence IS NULL AND retirement_event_id IS NULL ORDER BY action_ordinal LIMIT 1) AND effect_id IS NULL")
                .bind(effect).bind(input).bind(dispatch.into_uuid()).execute(&mut *tx).await.map_err(StoreError::from).map_err(SubmissionError::Store)?.rows_affected();
            if changed != 1 {
                return Err(SubmissionError::Store(StoreError::WorkflowInputRejected));
            }
        }
        tx.commit()
            .await
            .map_err(StoreError::from)
            .map_err(SubmissionError::Store)?;
        self.submit_selected_pending(codec, sink, source, Some(dispatch))
            .await?;
        let result = b"submitted".to_vec();
        sqlx::query("UPDATE dispatch_ledger SET effect_result = $2 WHERE effect_id = $1 AND effect_input = $3").bind(effect).bind(&result).bind(input).execute(&self.pool).await.map_err(StoreError::from).map_err(SubmissionError::Store)?;
        Ok(result)
    }

    /// Checks receipt binding before a host recovery resumes pending core follow-ups.
    pub async fn submission_receipt(
        &self,
        effect: Uuid,
        input: &[u8],
    ) -> Result<Option<SubmissionReceipt>, StoreError> {
        let mut tx = self.pool.begin().await?;
        receipt_lock(&mut tx, effect).await?;
        if let Some(result) =
            completed_receipt(&mut tx, effect, "repo.submitPending", input).await?
        {
            return Ok(Some(SubmissionReceipt {
                effect,
                input: input.to_vec(),
                result: Some(result),
            }));
        }
        let conflicting: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM rule_evaluation_cursor WHERE effect_id = $1)",
        )
        .bind(effect)
        .fetch_one(&mut *tx)
        .await?;
        if conflicting {
            return Err(StoreError::WorkflowInputRejected);
        }
        let row: Option<(Vec<u8>, Option<Vec<u8>>)> = sqlx::query_as(
            "SELECT effect_input, effect_result FROM dispatch_ledger WHERE effect_id = $1",
        )
        .bind(effect)
        .fetch_optional(&mut *tx)
        .await?;
        row.map(|(retained, result)| {
            if retained == input {
                Ok(SubmissionReceipt {
                    effect,
                    input: retained,
                    result,
                })
            } else {
                Err(StoreError::WorkflowInputRejected)
            }
        })
        .transpose()
    }
}

async fn baseline(
    tx: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
) -> Result<Option<signalbox_session_ownership::RepoWatchObservation>, StoreError> {
    let value: Option<Value> = sqlx::query_scalar(
        "SELECT comparison_baseline FROM repository_state WHERE repository = $1",
    )
    .bind(repository.as_str())
    .fetch_optional(&mut **tx)
    .await?
    .flatten();
    value
        .as_ref()
        .map(|value| {
            crate::observation_decode::observation(value)
                .ok_or(StoreError::InvalidComparisonBaseline)
        })
        .transpose()
}

impl RepoWatchStore {
    /// Lists submission bindings for successor recovery without active configuration.
    pub async fn submission_receipts(&self) -> Result<Vec<SubmissionReceipt>, StoreError> {
        let rows: Vec<(Uuid, Vec<u8>, Option<Vec<u8>>)> = sqlx::query_as("SELECT effect_id, effect_input, effect_result FROM dispatch_ledger WHERE effect_id IS NOT NULL ORDER BY issued_at, action_ordinal").fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|(effect, input, result)| SubmissionReceipt {
                effect,
                input,
                result,
            })
            .collect())
    }
    /// Retains the verified result while releasing its pending submission slot.
    pub async fn release_submission(&self, receipt: &EffectReceipt) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        receipt_lock(&mut tx, receipt.effect).await?;
        sqlx::query("WITH released AS (UPDATE dispatch_ledger SET effect_id = NULL, effect_input = NULL, effect_result = NULL WHERE effect_id = $1 AND effect_input = $2 AND effect_result = $3 RETURNING $1::uuid AS effect_id, $2::bytea AS effect_input, $3::bytea AS effect_result) INSERT INTO workflow_effect_result(effect_id, method, effect_input, effect_result) SELECT effect_id, 'repo.submitPending', effect_input, effect_result FROM released")
            .bind(receipt.effect).bind(&receipt.input).bind(&receipt.result).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
}
