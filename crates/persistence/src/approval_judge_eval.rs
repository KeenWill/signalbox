//! Durable recording for approval-judge eval runs.
//!
//! The `approval-judge-eval` binary replays synthetic labeled cases that hold
//! no session, turn, or parked request, so its calls can never satisfy the
//! live-request linkage `tool_approval_judge_model_call` enforces. The
//! eval-owned tables written here record the same measurement the stdout
//! scorecard prints — the scorecard stays the primary artifact — without
//! claiming daemon provenance.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_domain::{
    DelegateApprovalRecommendation, DirectModelSelection, ProviderReportedTokenUsage,
};
use sqlx::{
    PgPool,
    types::{Json, Uuid},
};

use crate::{commit_failure_is_ambiguous, mapping::approval_judge_recommendation_to_str};

/// Identity of one recorded eval run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApprovalJudgeEvalRunId(Uuid);

impl ApprovalJudgeEvalRunId {
    /// Wraps an externally minted run identity.
    #[must_use]
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    /// Returns the underlying UUID.
    #[must_use]
    pub const fn into_uuid(self) -> Uuid {
        self.0
    }
}

/// One eval run exactly as its scorecard states it.
#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalJudgeEvalRunRecord {
    /// Run identity minted by the invoking binary.
    pub run: ApprovalJudgeEvalRunId,
    /// Direct judge selection frozen for every replayed call.
    pub selection: DirectModelSelection,
    /// Provider model the selection resolved to.
    pub provider_model: String,
    /// Stable digest of the corpus bytes.
    pub corpus_digest: String,
    /// Stable digest of the operation contract beyond the payloads.
    pub contract_digest: String,
    /// Stable digest of every rendered request payload.
    pub rendered_digest: String,
    /// Judge calls attempted per case.
    pub repeats: u32,
    /// The exact scorecard object the run printed.
    pub scorecard: serde_json::Value,
}

/// One successful judge call within a recorded run.
///
/// A failed call yields no verdict and records no row; the scorecard's
/// per-case `failed_calls` count keeps those visible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalJudgeEvalCallRecord {
    /// Corpus case name the call replayed.
    pub case_name: String,
    /// One-based position among the case's attempted repeats.
    pub repeat_ordinal: u32,
    /// Closed recommendation the judge emitted.
    pub recommendation: DelegateApprovalRecommendation,
    /// Exact rationale emitted with the recommendation.
    pub rationale: String,
    /// Provider-reported usage for the call.
    pub usage: ProviderReportedTokenUsage,
}

/// Records one run and its per-call verdicts in one transaction.
///
/// Either the run row and every call row commit together or nothing is
/// recorded, so a stored run always carries its complete verdict evidence.
pub async fn record_eval_run(
    pool: &PgPool,
    run: &ApprovalJudgeEvalRunRecord,
    calls: &[ApprovalJudgeEvalCallRecord],
) -> Result<(), ApprovalJudgeEvalRecordingError> {
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO approval_judge_eval_run
            (eval_run_id, direct_model_selection_id, provider_model,
             corpus_digest, contract_digest, rendered_digest, repeats,
             scorecard)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(run.run.into_uuid())
    .bind(run.selection.into_uuid())
    .bind(run.provider_model.as_str())
    .bind(run.corpus_digest.as_str())
    .bind(run.contract_digest.as_str())
    .bind(run.rendered_digest.as_str())
    .bind(Decimal::from(run.repeats))
    .bind(Json(&run.scorecard))
    .execute(&mut *transaction)
    .await?;
    for call in calls {
        sqlx::query(
            "INSERT INTO approval_judge_eval_call
                (eval_run_id, case_name, repeat_ordinal, recommendation_kind,
                 rationale, input_tokens, output_tokens,
                 cache_creation_input_tokens, cache_read_input_tokens)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(run.run.into_uuid())
        .bind(call.case_name.as_str())
        .bind(Decimal::from(call.repeat_ordinal))
        .bind(approval_judge_recommendation_to_str(call.recommendation))
        .bind(call.rationale.as_str())
        .bind(call.usage.input_tokens().map(Decimal::from))
        .bind(call.usage.output_tokens().map(Decimal::from))
        .bind(call.usage.cache_creation_input_tokens().map(Decimal::from))
        .bind(call.usage.cache_read_input_tokens().map(Decimal::from))
        .execute(&mut *transaction)
        .await?;
    }
    transaction
        .commit()
        .await
        .map_err(ApprovalJudgeEvalRecordingError::commit)
}

/// PostgreSQL failure while recording an eval run.
#[derive(Debug)]
pub enum ApprovalJudgeEvalRecordingError {
    /// PostgreSQL failure with explicit commit ambiguity.
    Database {
        /// Original driver error.
        source: sqlx::Error,
        /// Whether a failed commit acknowledgement leaves outcome unknown.
        commit_ambiguous: bool,
    },
}

impl ApprovalJudgeEvalRecordingError {
    fn commit(error: sqlx::Error) -> Self {
        Self::Database {
            commit_ambiguous: commit_failure_is_ambiguous(&error),
            source: error,
        }
    }
}

impl fmt::Display for ApprovalJudgeEvalRecordingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database {
                commit_ambiguous: true,
                ..
            } => formatter.write_str("eval recording database commit outcome is ambiguous"),
            Self::Database {
                commit_ambiguous: false,
                ..
            } => formatter.write_str("eval recording database operation failed"),
        }
    }
}

impl Error for ApprovalJudgeEvalRecordingError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
        }
    }
}

impl From<sqlx::Error> for ApprovalJudgeEvalRecordingError {
    fn from(source: sqlx::Error) -> Self {
        Self::Database {
            source,
            commit_ambiguous: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ApprovalJudgeEvalRecordingError;

    #[test]
    fn recording_errors_distinguish_commit_ambiguity() {
        let ordinary = ApprovalJudgeEvalRecordingError::Database {
            source: sqlx::Error::PoolClosed,
            commit_ambiguous: false,
        };
        let ambiguous = ApprovalJudgeEvalRecordingError::Database {
            source: sqlx::Error::PoolClosed,
            commit_ambiguous: true,
        };
        assert_eq!(
            ordinary.to_string(),
            "eval recording database operation failed"
        );
        assert_eq!(
            ambiguous.to_string(),
            "eval recording database commit outcome is ambiguous"
        );
    }
}
