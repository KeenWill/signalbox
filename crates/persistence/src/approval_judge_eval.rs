//! Durable recording for approval-judge eval runs.
//!
//! The `approval-judge-eval` binary replays synthetic labeled cases that hold
//! no session, turn, or parked request, so its calls can never satisfy the
//! live-request linkage `tool_approval_judge_model_call` enforces. The
//! eval-owned tables written here record the same measurement the stdout
//! scorecard prints — the scorecard stays the primary artifact — without
//! claiming daemon provenance.

use std::{collections::BTreeMap, error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_domain::{
    DelegateApprovalRecommendation, DirectModelSelection, ProviderReportedTokenUsage,
    ResolvedProviderTarget,
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
    /// Exact resolved target every call was sent to. The selection is a
    /// mutable configuration mapping and provider-model spellings are not
    /// unique across targets, so only this identifies the invoked target
    /// after a configuration change.
    pub target: ResolvedProviderTarget,
    /// Provider model the selection resolved to.
    pub provider_model: String,
    /// Frozen non-secret credential reference every call was sent with. One
    /// selection and target can be re-routed to another credential profile
    /// by configuration alone, so rows are indistinguishable without it.
    pub credential_reference: String,
    /// Whether the resolved adapter's reported input total includes the cache
    /// axes; resolved once because a run holds a single frozen binding.
    pub usage_input_includes_cache_tokens: bool,
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

/// Reports whether both eval recording tables exist in the connected database
/// and admit inserts for the connected role, so a caller can refuse to start
/// work whose recording would predictably fail against a database the daemon
/// has not migrated yet or an underprivileged role. Schema application stays
/// with the daemon; this only observes it. The checked privileges are exactly
/// what recording exercises: INSERT on both tables, plus SELECT on the run
/// table because the sealing trigger reads the run's recording transaction
/// while admitting each call row.
pub async fn verify_recording_schema(pool: &PgPool) -> Result<(), ApprovalJudgeEvalRecordingError> {
    let present: bool = sqlx::query_scalar(
        "SELECT to_regclass('approval_judge_eval_run') IS NOT NULL
            AND to_regclass('approval_judge_eval_call') IS NOT NULL",
    )
    .fetch_one(pool)
    .await?;
    if !present {
        return Err(ApprovalJudgeEvalRecordingError::TablesAbsent);
    }
    let privileged: bool = sqlx::query_scalar(
        "SELECT has_table_privilege('approval_judge_eval_run', 'INSERT')
            AND has_table_privilege('approval_judge_eval_run', 'SELECT')
            AND has_table_privilege('approval_judge_eval_call', 'INSERT')",
    )
    .fetch_one(pool)
    .await?;
    if privileged {
        Ok(())
    } else {
        Err(ApprovalJudgeEvalRecordingError::TablesUnwritable)
    }
}

/// Requires the scorecard's own header fields to agree with the typed run
/// columns they duplicate.
///
/// The typed columns exist for indexed queries while the scorecard is the
/// primary artifact; letting them disagree would attribute one run to two
/// different experiment inputs depending on which representation a reader
/// consults, so a divergent or headerless scorecard is rejected before
/// anything commits.
fn require_scorecard_header_agreement(
    run: &ApprovalJudgeEvalRunRecord,
) -> Result<(), ApprovalJudgeEvalRecordingError> {
    let header = |field: &str| run.scorecard.get(field);
    let selection = run.selection.into_uuid().to_string();
    if header("judge_selection").and_then(serde_json::Value::as_str) != Some(selection.as_str()) {
        return Err(header_mismatch("judge_selection"));
    }
    if header("provider_model").and_then(serde_json::Value::as_str)
        != Some(run.provider_model.as_str())
    {
        return Err(header_mismatch("provider_model"));
    }
    if header("corpus_digest").and_then(serde_json::Value::as_str)
        != Some(run.corpus_digest.as_str())
    {
        return Err(header_mismatch("corpus_digest"));
    }
    if header("contract_digest").and_then(serde_json::Value::as_str)
        != Some(run.contract_digest.as_str())
    {
        return Err(header_mismatch("contract_digest"));
    }
    if header("rendered_digest").and_then(serde_json::Value::as_str)
        != Some(run.rendered_digest.as_str())
    {
        return Err(header_mismatch("rendered_digest"));
    }
    if header("repeats").and_then(serde_json::Value::as_u64) != Some(u64::from(run.repeats)) {
        return Err(header_mismatch("repeats"));
    }
    Ok(())
}

const fn header_mismatch(field: &'static str) -> ApprovalJudgeEvalRecordingError {
    ApprovalJudgeEvalRecordingError::ScorecardHeaderMismatch { field }
}

/// Requires the scorecard's per-case verdicts to agree with the call records.
///
/// The scorecard's `cases[].repeats` entries and the normalized call rows
/// state the same verdicts twice; a caller supplying a scorecard whose
/// verdicts differ from `calls` — or a scorecard reporting verdicts with no
/// matching calls at all — would let the two representations report different
/// experiment results, so recording requires them to agree before anything
/// commits. Comparison covers each case's verdict sequence in attempt order:
/// recommendation spelling and rationale, with the sequence lengths equal.
fn require_scorecard_verdict_agreement(
    run: &ApprovalJudgeEvalRunRecord,
    calls: &[ApprovalJudgeEvalCallRecord],
) -> Result<(), ApprovalJudgeEvalRecordingError> {
    let mut recorded: BTreeMap<&str, Vec<&ApprovalJudgeEvalCallRecord>> = BTreeMap::new();
    for call in calls {
        recorded
            .entry(call.case_name.as_str())
            .or_default()
            .push(call);
    }
    for sequence in recorded.values_mut() {
        sequence.sort_by_key(|call| call.repeat_ordinal);
    }
    let Some(cases) = run
        .scorecard
        .get("cases")
        .and_then(serde_json::Value::as_array)
    else {
        return Err(verdict_mismatch("cases"));
    };
    let mut stated: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for case in cases {
        let Some(name) = case.get("name").and_then(serde_json::Value::as_str) else {
            return Err(verdict_mismatch("cases"));
        };
        let Some(repeats) = case.get("repeats").and_then(serde_json::Value::as_array) else {
            return Err(verdict_mismatch(name));
        };
        let mut verdicts = Vec::new();
        for repeat in repeats {
            let recommendation = repeat
                .get("recommendation")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| verdict_mismatch(name))?;
            let rationale = repeat
                .get("rationale")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| verdict_mismatch(name))?;
            verdicts.push((recommendation, rationale));
        }
        // Every configured attempt is accounted for: the case's successful
        // verdicts plus its stated failed calls must cover the run's repeats
        // exactly, so a scorecard cannot quietly drop an attempt the call
        // rows would otherwise be missing.
        let failed_calls = case
            .get("failed_calls")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| verdict_mismatch(name))?;
        let successful = u64::try_from(verdicts.len()).map_err(|_| verdict_mismatch(name))?;
        if successful.saturating_add(failed_calls) != u64::from(run.repeats) {
            return Err(verdict_mismatch(name));
        }
        require_scorecard_case_summary_agreement(case, name, run.repeats, &verdicts)?;
        if stated.insert(name, verdicts).is_some() {
            return Err(verdict_mismatch(name));
        }
    }
    for (name, sequence) in &recorded {
        let stated_verdicts = stated.get(name).ok_or_else(|| verdict_mismatch(name))?;
        let recorded_verdicts: Vec<(&str, &str)> = sequence
            .iter()
            .map(|call| {
                (
                    approval_judge_recommendation_to_str(call.recommendation),
                    call.rationale.as_str(),
                )
            })
            .collect();
        if *stated_verdicts != recorded_verdicts {
            return Err(verdict_mismatch(name));
        }
    }
    for (name, verdicts) in &stated {
        if !verdicts.is_empty() && !recorded.contains_key(name) {
            return Err(verdict_mismatch(name));
        }
    }
    require_scorecard_aggregate_summary_agreement(&run.scorecard, cases)?;
    Ok(())
}

/// Requires one case's derived summaries to agree with its own verdicts.
fn require_scorecard_case_summary_agreement(
    case: &serde_json::Value,
    name: &str,
    configured_repeats: u32,
    verdicts: &[(&str, &str)],
) -> Result<(), ApprovalJudgeEvalRecordingError> {
    let mut counts: BTreeMap<&str, u64> = BTreeMap::new();
    for &(recommendation, _) in verdicts {
        *counts.entry(recommendation).or_default() += 1;
    }
    let majority = counts
        .iter()
        .find(|(_, count)| **count * 2 > u64::from(configured_repeats))
        .map(|(recommendation, _)| *recommendation);
    let measured = !verdicts.is_empty();
    let complete = u64::try_from(verdicts.len()).ok() == Some(u64::from(configured_repeats));
    let stable = (configured_repeats >= 2 && complete).then_some(counts.len() == 1);
    let leading = counts.values().max().copied().unwrap_or(0);
    let tied = measured && counts.values().filter(|count| **count == leading).count() > 1;
    let expected = [
        ("verdict_counts", serde_json::json!(counts)),
        ("majority", serde_json::json!(majority)),
        ("measured", serde_json::json!(measured)),
        ("complete", serde_json::json!(complete)),
        ("stable", serde_json::json!(stable)),
        ("tied", serde_json::json!(tied)),
    ];
    for (field, expected) in expected {
        if case.get(field) != Some(&expected) {
            return Err(ApprovalJudgeEvalRecordingError::ScorecardSummaryMismatch {
                case: String::from(name),
                field,
            });
        }
    }
    Ok(())
}

#[derive(Default)]
struct ScorecardAggregate {
    cases: u64,
    correct_majorities: u64,
    unstable_cases: u64,
    stability_unmeasured_cases: u64,
    partial_cases: u64,
    unmeasured_cases: u64,
    failed_calls: u64,
}

/// Requires the scorecard's aggregate summaries to agree with its cases.
fn require_scorecard_aggregate_summary_agreement(
    scorecard: &serde_json::Value,
    cases: &[serde_json::Value],
) -> Result<(), ApprovalJudgeEvalRecordingError> {
    let mut categories: BTreeMap<&str, ScorecardAggregate> = BTreeMap::new();
    let mut totals = ScorecardAggregate::default();
    let mut expected_escalations = 0_u64;
    let mut observed_escalation_majorities = 0_u64;
    let mut missed_escalations = 0_u64;
    let mut excess_escalations = 0_u64;

    for case in cases {
        let category = case
            .get("category")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| aggregate_mismatch("categories"))?;
        let expected = case
            .get("expected")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| aggregate_mismatch("correct_majorities"))?;
        let majority = match case.get("majority") {
            Some(serde_json::Value::Null) => None,
            Some(value) => Some(
                value
                    .as_str()
                    .ok_or_else(|| aggregate_mismatch("correct_majorities"))?,
            ),
            None => return Err(aggregate_mismatch("correct_majorities")),
        };
        let measured = case
            .get("measured")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| aggregate_mismatch("unmeasured_cases"))?;
        let complete = case
            .get("complete")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| aggregate_mismatch("partial_cases"))?;
        let stable = case
            .get("stable")
            .ok_or_else(|| aggregate_mismatch("stability_unmeasured_cases"))?;
        let unstable = case
            .get("verdict_counts")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|counts| counts.len() > 1);
        let failed_calls = case
            .get("failed_calls")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| aggregate_mismatch("categories"))?;
        let correct = measured && majority == Some(expected);
        let escalation_expected = expected == "escalate_to_human";
        let escalation_majority = majority == Some("escalate_to_human");
        let update = |aggregate: &mut ScorecardAggregate| {
            aggregate.cases += 1;
            aggregate.correct_majorities += u64::from(correct);
            aggregate.unstable_cases += u64::from(unstable);
            aggregate.stability_unmeasured_cases += u64::from(measured && stable.is_null());
            aggregate.partial_cases += u64::from(measured && !complete);
            aggregate.unmeasured_cases += u64::from(!measured);
            aggregate.failed_calls += failed_calls;
        };
        update(&mut totals);
        update(categories.entry(category).or_default());
        expected_escalations += u64::from(escalation_expected);
        observed_escalation_majorities += u64::from(escalation_majority);
        missed_escalations +=
            u64::from(escalation_expected && majority.is_some() && !escalation_majority);
        excess_escalations += u64::from(!escalation_expected && escalation_majority);
    }

    require_aggregate_field(scorecard, "total_cases", totals.cases)?;
    require_aggregate_field(scorecard, "correct_majorities", totals.correct_majorities)?;
    require_aggregate_field(scorecard, "unstable_cases", totals.unstable_cases)?;
    require_aggregate_field(
        scorecard,
        "stability_unmeasured_cases",
        totals.stability_unmeasured_cases,
    )?;
    require_aggregate_field(scorecard, "partial_cases", totals.partial_cases)?;
    require_aggregate_field(scorecard, "unmeasured_cases", totals.unmeasured_cases)?;

    let expected_escalation = serde_json::json!({
        "expected_cases": expected_escalations,
        "observed_majorities": observed_escalation_majorities,
        "missed": missed_escalations,
        "excess": excess_escalations,
    });
    if scorecard.get("escalation_calibration") != Some(&expected_escalation) {
        return Err(aggregate_mismatch("escalation_calibration"));
    }

    let stated_categories = scorecard
        .get("categories")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| aggregate_mismatch("categories"))?;
    if stated_categories.len() != categories.len() {
        return Err(aggregate_mismatch("categories"));
    }
    let mut seen_categories = BTreeMap::new();
    for stated in stated_categories {
        let category = stated
            .get("category")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| aggregate_mismatch("categories"))?;
        let expected = categories
            .get(category)
            .ok_or_else(|| aggregate_mismatch("categories"))?;
        let expected = serde_json::json!({
            "category": category,
            "cases": expected.cases,
            "correct_majorities": expected.correct_majorities,
            "unstable_cases": expected.unstable_cases,
            "stability_unmeasured_cases": expected.stability_unmeasured_cases,
            "partial_cases": expected.partial_cases,
            "unmeasured_cases": expected.unmeasured_cases,
            "failed_calls": expected.failed_calls,
        });
        if stated != &expected || seen_categories.insert(category, ()).is_some() {
            return Err(aggregate_mismatch("categories"));
        }
    }
    Ok(())
}

fn require_aggregate_field(
    scorecard: &serde_json::Value,
    field: &'static str,
    expected: u64,
) -> Result<(), ApprovalJudgeEvalRecordingError> {
    if scorecard.get(field).and_then(serde_json::Value::as_u64) == Some(expected) {
        Ok(())
    } else {
        Err(aggregate_mismatch(field))
    }
}

const fn aggregate_mismatch(field: &'static str) -> ApprovalJudgeEvalRecordingError {
    ApprovalJudgeEvalRecordingError::ScorecardAggregateMismatch { field }
}

fn verdict_mismatch(case: &str) -> ApprovalJudgeEvalRecordingError {
    ApprovalJudgeEvalRecordingError::ScorecardVerdictMismatch {
        case: String::from(case),
    }
}

/// Records one run and its per-call verdicts in one transaction.
///
/// Either the run row and every call row commit together or nothing is
/// recorded, so a stored run always carries its complete verdict evidence.
/// Every call ordinal must fall inside the run's configured repeats — a call
/// outside that range would be durable evidence for an attempt the run
/// claims was never configured — and the scorecard must agree with the typed
/// representations it duplicates, header fields and per-case verdicts alike;
/// every violation is rejected before anything commits.
pub async fn record_eval_run(
    pool: &PgPool,
    run: &ApprovalJudgeEvalRunRecord,
    calls: &[ApprovalJudgeEvalCallRecord],
) -> Result<(), ApprovalJudgeEvalRecordingError> {
    require_scorecard_header_agreement(run)?;
    require_scorecard_verdict_agreement(run, calls)?;
    for call in calls {
        if call.repeat_ordinal < 1 || call.repeat_ordinal > run.repeats {
            return Err(ApprovalJudgeEvalRecordingError::CallOutsideConfiguredRepeats);
        }
    }
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO approval_judge_eval_run
            (eval_run_id, direct_model_selection_id,
             resolved_provider_model_identity_id, provider_model,
             credential_reference, usage_input_includes_cache_tokens,
             corpus_digest, contract_digest, rendered_digest, repeats,
             scorecard)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(run.run.into_uuid())
    .bind(run.selection.into_uuid())
    .bind(run.target.identity().into_uuid())
    .bind(run.provider_model.as_str())
    .bind(run.credential_reference.as_str())
    .bind(run.usage_input_includes_cache_tokens)
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

/// Rejection or PostgreSQL failure while recording an eval run.
#[derive(Debug)]
pub enum ApprovalJudgeEvalRecordingError {
    /// PostgreSQL failure with explicit commit ambiguity.
    Database {
        /// Original driver error.
        source: sqlx::Error,
        /// Whether a failed commit acknowledgement leaves outcome unknown.
        commit_ambiguous: bool,
    },
    /// The connected database has no eval recording tables.
    TablesAbsent,
    /// The connected role lacks a table privilege eval recording exercises.
    TablesUnwritable,
    /// A call's repeat ordinal falls outside the run's configured repeats.
    CallOutsideConfiguredRepeats,
    /// A scorecard header field disagrees with the typed column it duplicates.
    ScorecardHeaderMismatch {
        /// The scorecard field that diverged or was absent.
        field: &'static str,
    },
    /// The scorecard's per-case verdicts disagree with the call records.
    ScorecardVerdictMismatch {
        /// The case whose verdicts diverged, or the structural key at fault.
        case: String,
    },
    /// A scorecard's per-case derived summary disagrees with its verdicts.
    ScorecardSummaryMismatch {
        /// The case whose summary diverged.
        case: String,
        /// The derived field that diverged or was absent.
        field: &'static str,
    },
    /// A scorecard aggregate disagrees with its per-case data.
    ScorecardAggregateMismatch {
        /// The aggregate field or section that diverged or was absent.
        field: &'static str,
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
            Self::TablesAbsent => formatter.write_str(
                "eval recording tables are absent; the daemon has not applied this migration set",
            ),
            Self::TablesUnwritable => formatter.write_str(
                "eval recording tables refuse the connected role a required table privilege",
            ),
            Self::CallOutsideConfiguredRepeats => formatter
                .write_str("eval call repeat ordinal is outside the run's configured repeats"),
            Self::ScorecardHeaderMismatch { field } => write!(
                formatter,
                "eval scorecard header disagrees with the typed run record: {field}"
            ),
            Self::ScorecardVerdictMismatch { case } => write!(
                formatter,
                "eval scorecard verdicts disagree with the call records: {case}"
            ),
            Self::ScorecardSummaryMismatch { case, field } => write!(
                formatter,
                "eval scorecard case summary disagrees with its verdicts: {case}.{field}"
            ),
            Self::ScorecardAggregateMismatch { field } => write!(
                formatter,
                "eval scorecard aggregate disagrees with its cases: {field}"
            ),
        }
    }
}

impl Error for ApprovalJudgeEvalRecordingError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::TablesAbsent
            | Self::TablesUnwritable
            | Self::CallOutsideConfiguredRepeats
            | Self::ScorecardHeaderMismatch { .. }
            | Self::ScorecardVerdictMismatch { .. }
            | Self::ScorecardSummaryMismatch { .. }
            | Self::ScorecardAggregateMismatch { .. } => None,
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
    use expect_test::expect;
    use serde_json::json;

    use super::{
        ApprovalJudgeEvalRecordingError, require_scorecard_aggregate_summary_agreement,
        require_scorecard_case_summary_agreement,
    };

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
        expect!["eval recording database operation failed"].assert_eq(&ordinary.to_string());
        expect!["eval recording database commit outcome is ambiguous"]
            .assert_eq(&ambiguous.to_string());
    }

    #[test]
    fn recording_rejections_render_their_causes() {
        expect!["eval recording tables are absent; the daemon has not applied this migration set"]
            .assert_eq(&ApprovalJudgeEvalRecordingError::TablesAbsent.to_string());
        expect!["eval recording tables refuse the connected role a required table privilege"]
            .assert_eq(&ApprovalJudgeEvalRecordingError::TablesUnwritable.to_string());
        expect!["eval call repeat ordinal is outside the run's configured repeats"]
            .assert_eq(&ApprovalJudgeEvalRecordingError::CallOutsideConfiguredRepeats.to_string());
        expect!["eval scorecard header disagrees with the typed run record: repeats"].assert_eq(
            &ApprovalJudgeEvalRecordingError::ScorecardHeaderMismatch { field: "repeats" }
                .to_string(),
        );
        expect!["eval scorecard verdicts disagree with the call records: fixture-case"].assert_eq(
            &ApprovalJudgeEvalRecordingError::ScorecardVerdictMismatch {
                case: String::from("fixture-case"),
            }
            .to_string(),
        );
        expect!["eval scorecard case summary disagrees with its verdicts: fixture-case.majority"]
            .assert_eq(
                &ApprovalJudgeEvalRecordingError::ScorecardSummaryMismatch {
                    case: String::from("fixture-case"),
                    field: "majority",
                }
                .to_string(),
            );
    }

    #[test]
    fn scorecard_case_summaries_must_match_their_verdicts() {
        let verdicts = [("approve", "first"), ("approve", "second")];
        let valid = json!({
            "verdict_counts": {"approve": 2},
            "majority": "approve",
            "measured": true,
            "complete": true,
            "stable": true,
            "tied": false,
        });
        require_scorecard_case_summary_agreement(&valid, "fixture-case", 2, &verdicts)
            .expect("derived summaries agree");

        let mut contradictory = valid.clone();
        contradictory["verdict_counts"] = json!(null);
        assert_summary_mismatch(&contradictory, &verdicts, "verdict_counts");

        let mut contradictory = valid.clone();
        contradictory["majority"] = json!(null);
        assert_summary_mismatch(&contradictory, &verdicts, "majority");

        let mut contradictory = valid.clone();
        contradictory["measured"] = json!(null);
        assert_summary_mismatch(&contradictory, &verdicts, "measured");

        let mut contradictory = valid.clone();
        contradictory["complete"] = json!(null);
        assert_summary_mismatch(&contradictory, &verdicts, "complete");

        let mut contradictory = valid.clone();
        contradictory["stable"] = json!(null);
        assert_summary_mismatch(&contradictory, &verdicts, "stable");

        let mut contradictory = valid.clone();
        contradictory["tied"] = json!(null);
        assert_summary_mismatch(&contradictory, &verdicts, "tied");
    }

    #[test]
    fn scorecard_aggregate_summaries_must_match_their_cases() {
        let cases = vec![json!({
            "category": "git_push",
            "expected": "approve",
            "verdict_counts": {"approve": 2},
            "majority": "approve",
            "measured": true,
            "complete": false,
            "stable": null,
            "failed_calls": 1,
        })];
        let valid = json!({
            "total_cases": 1,
            "correct_majorities": 1,
            "unstable_cases": 0,
            "stability_unmeasured_cases": 1,
            "partial_cases": 1,
            "unmeasured_cases": 0,
            "escalation_calibration": {
                "expected_cases": 0,
                "observed_majorities": 0,
                "missed": 0,
                "excess": 0,
            },
            "categories": [{
                "category": "git_push",
                "cases": 1,
                "correct_majorities": 1,
                "unstable_cases": 0,
                "stability_unmeasured_cases": 1,
                "partial_cases": 1,
                "unmeasured_cases": 0,
                "failed_calls": 1,
            }],
        });
        require_scorecard_aggregate_summary_agreement(&valid, &cases)
            .expect("aggregate summaries agree");

        let mut contradictory = valid;
        contradictory["total_cases"] = json!(2);
        assert!(matches!(
            require_scorecard_aggregate_summary_agreement(&contradictory, &cases),
            Err(ApprovalJudgeEvalRecordingError::ScorecardAggregateMismatch {
                field: "total_cases"
            })
        ));
    }

    #[track_caller]
    fn assert_summary_mismatch(
        case: &serde_json::Value,
        verdicts: &[(&str, &str)],
        expected_field: &'static str,
    ) {
        assert!(matches!(
            require_scorecard_case_summary_agreement(case, "fixture-case", 2, verdicts),
            Err(ApprovalJudgeEvalRecordingError::ScorecardSummaryMismatch {
                field,
                ..
            }) if field == expected_field
        ));
    }
}
