//! Observation receipts retained with the module's frontier commits.
//! Governed by docs/spec/repo-watch.md.

use crate::{RepoWatchStore, StoreError, workflow::EffectReceipt};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde_json::json;
use signalbox_session_ownership::RepositorySlug;
use sqlx::{Postgres, Transaction};
use std::sync::Arc;
use uuid::Uuid;

/// Exact invocation bound to every complete stage committed during an attempt.
#[derive(Clone, Debug)]
pub struct ObservationInvocation {
    pub effect: Uuid,
    pub input: Vec<u8>,
    pub repository: RepositorySlug,
}

/// The observed portion of a bounded reconciliation attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationOutcome {
    Succeeded,
    Partial,
    Failed,
}

/// Latest committed frontier and accepted event interval (after, through].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservationResult {
    pub generation: u64,
    pub after: u64,
    pub through: u64,
    pub outcome: ObservationOutcome,
}

impl ObservationResult {
    pub fn encode(&self) -> Vec<u8> {
        json!({
            "generation": self.generation,
            "after": self.after,
            "through": self.through,
            "outcome": match self.outcome {
                ObservationOutcome::Succeeded => "succeeded",
                ObservationOutcome::Partial => "partial",
                ObservationOutcome::Failed => "failed",
            },
        })
        .to_string()
        .into_bytes()
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
        let fields = value.as_object()?;
        if fields.len() != 4 {
            return None;
        }
        let result = Self {
            generation: fields.get("generation")?.as_u64()?,
            after: fields.get("after")?.as_u64()?,
            through: fields.get("through")?.as_u64()?,
            outcome: match fields.get("outcome")?.as_str()? {
                "succeeded" => ObservationOutcome::Succeeded,
                "partial" => ObservationOutcome::Partial,
                "failed" => ObservationOutcome::Failed,
                _ => return None,
            },
        };
        (result.after <= result.through).then_some(result)
    }
}

#[derive(sqlx::FromRow)]
struct ReceiptRow {
    effect: Uuid,
    input: Vec<u8>,
    result: Vec<u8>,
}

impl ReceiptRow {
    fn checked(self) -> Result<EffectReceipt, StoreError> {
        ObservationResult::decode(&self.result).ok_or(StoreError::InvalidObservationReceipt)?;
        Ok(EffectReceipt {
            effect: self.effect,
            input: self.input,
            result: self.result,
        })
    }
}

impl RepoWatchStore {
    /// Serializes provider attempts and receipt adoption across workflow runs.
    pub async fn lock_observation(
        &self,
        repository: &RepositorySlug,
    ) -> Result<Transaction<'static, Postgres>, StoreError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('observation:' || $1, 0))")
            .bind(repository.as_str())
            .execute(&mut *transaction)
            .await?;
        Ok(transaction)
    }

    /// Binds frontier commits made by this adapter to its exact effect request.
    pub fn with_observation_invocation(&self, invocation: ObservationInvocation) -> Self {
        let mut store = self.clone();
        store.observation_invocation = Some(Arc::new(invocation));
        store
    }

    pub async fn observation_receipt(
        &self,
        repository: &RepositorySlug,
    ) -> Result<Option<EffectReceipt>, StoreError> {
        sqlx::query_as::<_, ReceiptRow>("SELECT observation_effect_id AS effect, observation_effect_input AS input, observation_effect_result AS result FROM repository_state WHERE repository=$1 AND observation_effect_id IS NOT NULL")
            .bind(repository.as_str()).fetch_optional(&self.pool).await?
            .map(ReceiptRow::checked).transpose()
    }

    pub async fn observation_receipts(&self) -> Result<Vec<EffectReceipt>, StoreError> {
        sqlx::query_as::<_, ReceiptRow>("SELECT observation_effect_id AS effect, observation_effect_input AS input, observation_effect_result AS result FROM repository_state WHERE observation_effect_id IS NOT NULL ORDER BY repository")
            .fetch_all(&self.pool).await?.into_iter().map(ReceiptRow::checked).collect()
    }

    pub async fn observation_receipt_by_effect(
        &self,
        effect: Uuid,
    ) -> Result<Option<EffectReceipt>, StoreError> {
        sqlx::query_as::<_, ReceiptRow>("SELECT observation_effect_id AS effect, observation_effect_input AS input, observation_effect_result AS result FROM repository_state WHERE observation_effect_id=$1")
            .bind(effect).fetch_optional(&self.pool).await?.map(ReceiptRow::checked).transpose()
    }

    /// Releases only the receipt whose exact answer has reached a durable journal.
    pub async fn release_observation(&self, receipt: &EffectReceipt) -> Result<(), StoreError> {
        sqlx::query("UPDATE repository_state SET observation_effect_id=NULL, observation_effect_input=NULL, observation_effect_result=NULL WHERE observation_effect_id=$1 AND observation_effect_input=$2 AND observation_effect_result=$3")
            .bind(receipt.effect).bind(&receipt.input).bind(&receipt.result).execute(&self.pool).await?;
        Ok(())
    }

    /// Freezes the outcome after all completed stages; interruption retains their partial receipt.
    pub async fn finish_observation(
        &self,
        outcome: ObservationOutcome,
    ) -> Result<ObservationResult, StoreError> {
        let invocation = self
            .observation_invocation
            .as_ref()
            .ok_or(StoreError::InvalidObservationReceipt)?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('frontier:' || $1, 0))")
            .bind(invocation.repository.as_str())
            .execute(&mut *transaction)
            .await?;
        let mut result =
            match checked_receipt(&mut transaction, &invocation.repository, Some(invocation))
                .await?
            {
                Some(receipt) => ObservationResult::decode(&receipt.result)
                    .ok_or(StoreError::InvalidObservationReceipt)?,
                None => {
                    let position =
                        observation_position(&mut transaction, &invocation.repository).await?;
                    ObservationResult {
                        generation: position.generation,
                        after: position.through,
                        through: position.through,
                        outcome,
                    }
                }
            };
        result.outcome = outcome;
        sqlx::query("UPDATE repository_state SET observation_effect_result=$3 WHERE observation_effect_id=$1 AND observation_effect_input=$2")
            .bind(invocation.effect).bind(&invocation.input).bind(result.encode()).execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(result)
    }
}

pub(crate) async fn checked_receipt(
    transaction: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
    invocation: Option<&ObservationInvocation>,
) -> Result<Option<EffectReceipt>, StoreError> {
    if invocation.is_some_and(|invocation| &invocation.repository != repository) {
        return Err(StoreError::InvalidObservationReceipt);
    }
    let row = sqlx::query_as::<_, ReceiptRow>("SELECT observation_effect_id AS effect, observation_effect_input AS input, observation_effect_result AS result FROM repository_state WHERE repository=$1 AND observation_effect_id IS NOT NULL")
        .bind(repository.as_str()).fetch_optional(&mut **transaction).await?;
    let receipt = row.map(ReceiptRow::checked).transpose()?;
    if let Some(receipt) = &receipt
        && !invocation.is_some_and(|invocation| {
            invocation.effect == receipt.effect && invocation.input == receipt.input
        })
    {
        return Err(StoreError::InvalidObservationReceipt);
    }
    Ok(receipt)
}

#[derive(sqlx::FromRow)]
struct PositionRow {
    generation: Decimal,
    through: Decimal,
}
pub(crate) struct ObservationPosition {
    pub generation: u64,
    pub through: u64,
}

pub(crate) async fn observation_position(
    transaction: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
) -> Result<ObservationPosition, StoreError> {
    let position = sqlx::query_as::<_, PositionRow>("SELECT COALESCE((SELECT frontier_generation FROM repository_state WHERE repository=$1), 0) AS generation, COALESCE((SELECT max(repository_event_ordinal) FROM gh_event WHERE repository=$1), 0) AS through")
        .bind(repository.as_str()).fetch_one(&mut **transaction).await?;
    Ok(ObservationPosition {
        generation: position
            .generation
            .to_u64()
            .ok_or(StoreError::InvalidObservationReceipt)?,
        through: position
            .through
            .to_u64()
            .ok_or(StoreError::InvalidObservationReceipt)?,
    })
}

pub(crate) async fn retain_stage(
    transaction: &mut Transaction<'_, Postgres>,
    invocation: &ObservationInvocation,
    result: ObservationResult,
) -> Result<(), StoreError> {
    sqlx::query("UPDATE repository_state SET observation_effect_id=$2, observation_effect_input=$3, observation_effect_result=$4 WHERE repository=$1")
        .bind(invocation.repository.as_str()).bind(invocation.effect).bind(&invocation.input).bind(result.encode()).execute(&mut **transaction).await?;
    Ok(())
}
