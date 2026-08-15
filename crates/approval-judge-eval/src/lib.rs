//! Offline corpus and scoring support for the current binary approval judge.
//!
//! The harness delegates request rendering and model-output decoding to
//! [`signalboxd::approval_judge_eval`], so evaluations exercise the same
//! prompt and decision assembly as daemon execution without entering the
//! daemon's durable decision path.

use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use signalbox_domain::DelegateApprovalRecommendation;
use signalbox_model_provider_runtime::ApprovalJudgeModel;
use signalboxd::approval_judge_eval::{
    ApprovalJudgeEvalBinding, ApprovalJudgeEvalCase, judge_eval_case,
};

/// The only corpus format this pre-alpha harness currently accepts.
pub const CORPUS_FORMAT_VERSION: u32 = 1;

/// A versioned collection of labeled approval-judge cases.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalJudgeCorpus {
    /// Corpus representation version.
    pub format_version: u32,
    /// Cases in replay order.
    pub cases: Vec<ApprovalJudgeCase>,
}

/// One labeled approval request and the authority context shown to the judge.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalJudgeCase {
    /// Stable logical identity, also used to derive the replay request id.
    pub id: String,
    /// Exact tool-request and authority context rendered for the judge.
    pub request: ApprovalJudgeRequestContext,
    /// Labeled binary-judge disposition.
    pub expected: ApprovalDisposition,
    /// Free-text provenance explaining where the label came from.
    pub label_provenance: String,
}

/// The exact request fields and frozen authority context used by replay.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalJudgeRequestContext {
    /// Exact tool name.
    pub tool: String,
    /// Exact provider argument text, whether canonical JSON or undecodable.
    pub arguments: String,
    /// Commissioned goal shown to the judge, when present.
    pub commissioned_goal: Option<String>,
    /// Session template name shown to the judge, when present.
    pub session_template: Option<String>,
    /// System prompt frozen for the judged turn, when present.
    pub frozen_system_prompt: Option<String>,
}

/// The closed output vocabulary of the current approval judge.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDisposition {
    /// Permit the exact request.
    Approve,
    /// Permanently reject the exact request.
    Deny,
    /// Leave the request parked for the user.
    EscalateToHuman,
}

impl ApprovalDisposition {
    /// Returns the structured-output spelling used by the binary judge.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Deny => "deny",
            Self::EscalateToHuman => "escalate_to_human",
        }
    }
}

impl From<DelegateApprovalRecommendation> for ApprovalDisposition {
    fn from(recommendation: DelegateApprovalRecommendation) -> Self {
        match recommendation {
            DelegateApprovalRecommendation::Approve => Self::Approve,
            DelegateApprovalRecommendation::Deny => Self::Deny,
            DelegateApprovalRecommendation::EscalateToHuman => Self::EscalateToHuman,
        }
    }
}

/// Loads and validates a corpus JSON document from a file.
pub fn load_corpus(path: impl AsRef<Path>) -> Result<ApprovalJudgeCorpus, CorpusLoadError> {
    let path = path.as_ref();
    let bytes = fs::read(path).map_err(|source| CorpusLoadError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    decode_corpus(&bytes)
}

/// Decodes and validates a corpus JSON document from bytes.
pub fn decode_corpus(bytes: &[u8]) -> Result<ApprovalJudgeCorpus, CorpusLoadError> {
    let corpus: ApprovalJudgeCorpus =
        serde_json::from_slice(bytes).map_err(CorpusLoadError::Json)?;
    if corpus.format_version != CORPUS_FORMAT_VERSION {
        return Err(CorpusLoadError::UnsupportedFormatVersion {
            observed: corpus.format_version,
        });
    }
    Ok(corpus)
}

/// A corpus file could not be read or admitted.
#[derive(Debug)]
pub enum CorpusLoadError {
    /// Filesystem access failed.
    Read {
        /// Requested corpus path.
        path: PathBuf,
        /// Underlying filesystem failure.
        source: std::io::Error,
    },
    /// JSON decoding or strict shape validation failed.
    Json(serde_json::Error),
    /// The corpus names a format this harness does not implement.
    UnsupportedFormatVersion {
        /// Version found in the document.
        observed: u32,
    },
}

impl fmt::Display for CorpusLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(
                    formatter,
                    "could not read corpus {}: {source}",
                    path.display()
                )
            }
            Self::Json(source) => write!(formatter, "corpus JSON is invalid: {source}"),
            Self::UnsupportedFormatVersion { observed } => write!(
                formatter,
                "corpus format version {observed} is unsupported; expected {CORPUS_FORMAT_VERSION}"
            ),
        }
    }
}

impl Error for CorpusLoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Json(source) => Some(source),
            Self::UnsupportedFormatVersion { .. } => None,
        }
    }
}

/// Replays and scores every corpus case through the current binary judge path.
pub async fn score_corpus(
    model: &dyn ApprovalJudgeModel,
    binding: &ApprovalJudgeEvalBinding,
    corpus: &ApprovalJudgeCorpus,
) -> Result<ApprovalJudgeScorecard, ScoreError> {
    let mut verdicts = Vec::with_capacity(corpus.cases.len());
    for case in &corpus.cases {
        let eval_case = ApprovalJudgeEvalCase {
            name: case.id.clone(),
            tool: case.request.tool.clone(),
            arguments: case.request.arguments.clone(),
            goal: case.request.commissioned_goal.clone(),
            template: case.request.session_template.clone(),
            system_prompt: case.request.frozen_system_prompt.clone(),
        };
        let result = judge_eval_case(model, binding, &eval_case)
            .await
            .map_err(|source| ScoreError {
                case_id: case.id.clone(),
                source,
            })?;
        let actual = ApprovalDisposition::from(result.recommendation);
        verdicts.push(ApprovalJudgeCaseVerdict {
            case_id: case.id.clone(),
            expected: case.expected,
            actual,
            correct: actual == case.expected,
            rationale: result.rationale,
            label_provenance: case.label_provenance.clone(),
        });
    }
    Ok(ApprovalJudgeScorecard::from_verdicts(verdicts))
}

/// One case's expected label and decoded judge decision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ApprovalJudgeCaseVerdict {
    /// Stable corpus case identity.
    pub case_id: String,
    /// Labeled disposition.
    pub expected: ApprovalDisposition,
    /// Disposition decoded by the current binary judge adapter.
    pub actual: ApprovalDisposition,
    /// Whether expected and actual dispositions match.
    pub correct: bool,
    /// Exact bounded rationale decoded with the disposition.
    pub rationale: String,
    /// Label provenance copied from the corpus for report readers.
    pub label_provenance: String,
}

/// Aggregate metrics and all constituent per-case verdicts.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ApprovalJudgeScorecard {
    /// Exact-match accuracy over all cases.
    pub accuracy: MetricRate,
    /// Precision and recall in stable disposition order.
    pub dispositions: Vec<DispositionMetrics>,
    /// Per-case evidence from which the aggregate is derived.
    pub verdicts: Vec<ApprovalJudgeCaseVerdict>,
}

impl ApprovalJudgeScorecard {
    fn from_verdicts(verdicts: Vec<ApprovalJudgeCaseVerdict>) -> Self {
        let correct = verdicts.iter().filter(|verdict| verdict.correct).count();
        let accuracy = MetricRate::new(correct, verdicts.len());
        let dispositions = vec![
            DispositionMetrics::from_verdicts(ApprovalDisposition::Approve, &verdicts),
            DispositionMetrics::from_verdicts(ApprovalDisposition::Deny, &verdicts),
            DispositionMetrics::from_verdicts(ApprovalDisposition::EscalateToHuman, &verdicts),
        ];
        Self {
            accuracy,
            dispositions,
            verdicts,
        }
    }
}

/// One fraction with counts retained alongside its optional decimal value.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct MetricRate {
    /// Count satisfying the metric.
    pub numerator: usize,
    /// Population against which the count is measured.
    pub denominator: usize,
    /// Decimal ratio, or `None` when the denominator is zero.
    pub value: Option<f64>,
}

impl MetricRate {
    fn new(numerator: usize, denominator: usize) -> Self {
        Self {
            numerator,
            denominator,
            value: (denominator != 0).then_some(numerator as f64 / denominator as f64),
        }
    }
}

/// One disposition's one-vs-rest classification metrics.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct DispositionMetrics {
    /// Disposition treated as the positive class.
    pub disposition: ApprovalDisposition,
    /// Cases labeled and predicted as this disposition.
    pub true_positives: usize,
    /// Cases predicted as this disposition under another label.
    pub false_positives: usize,
    /// Cases carrying this label but predicted otherwise.
    pub false_negatives: usize,
    /// `true_positives / (true_positives + false_positives)`.
    pub precision: MetricRate,
    /// `true_positives / (true_positives + false_negatives)`.
    pub recall: MetricRate,
}

impl DispositionMetrics {
    fn from_verdicts(
        disposition: ApprovalDisposition,
        verdicts: &[ApprovalJudgeCaseVerdict],
    ) -> Self {
        let true_positives = verdicts
            .iter()
            .filter(|verdict| verdict.expected == disposition && verdict.actual == disposition)
            .count();
        let false_positives = verdicts
            .iter()
            .filter(|verdict| verdict.expected != disposition && verdict.actual == disposition)
            .count();
        let false_negatives = verdicts
            .iter()
            .filter(|verdict| verdict.expected == disposition && verdict.actual != disposition)
            .count();
        Self {
            disposition,
            true_positives,
            false_positives,
            false_negatives,
            precision: MetricRate::new(true_positives, true_positives + false_positives),
            recall: MetricRate::new(true_positives, true_positives + false_negatives),
        }
    }
}

/// A case could not be replayed through the judge adapter.
#[derive(Debug)]
pub struct ScoreError {
    case_id: String,
    source: Box<dyn Error + Send + Sync>,
}

impl ScoreError {
    /// Returns the logical identity of the failed case.
    #[must_use]
    pub fn case_id(&self) -> &str {
        &self.case_id
    }
}

impl fmt::Display for ScoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "approval-judge replay failed for case {}: {}",
            self.case_id, self.source
        )
    }
}

impl Error for ScoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use signalbox_domain::{
        DirectModelSelection, ModelCallId, ProviderModelIdentity, ResolvedProviderTarget,
    };
    use signalbox_model_provider_runtime::{
        RuntimeApprovalJudgeModel, RuntimeModelCatalog, RuntimeModelDefinition,
    };
    use signalbox_model_runtime::{
        AssistantPart, CompletionEvidence, CompletionFinish, ExchangeFacts, ProviderReportedModel,
        Script, ScriptedModel, TerminalEvidence, TokenUsage, ToolCallId, ToolCallProposal,
        ToolName,
    };
    use signalboxd::approval_judge_eval::ApprovalJudgeEvalBinding;
    use uuid::Uuid;

    use super::{
        ApprovalDisposition, ApprovalJudgeCorpus, CORPUS_FORMAT_VERSION, DispositionMetrics,
        MetricRate, decode_corpus, score_corpus,
    };

    const SEED_CORPUS: &[u8] = include_bytes!("../corpora/seed-v1.json");
    const PROVIDER_MODEL: &str = "offline-fixture-judge";
    const APPROVE_RATIONALE: &str = "The exact read is plainly within the grant.";
    const DENY_RATIONALE: &str = "The request crosses the named branch boundary.";
    const ESCALATE_RATIONALE: &str = "The goal is absent, so the request stays parked.";

    #[test]
    fn corpus_format_serde_round_trip_preserves_the_seed_cases() {
        let corpus = decode_corpus(SEED_CORPUS).expect("the checked-in seed corpus is admitted");
        let encoded = serde_json::to_vec(&corpus).expect("the admitted corpus serializes");
        let decoded = decode_corpus(&encoded).expect("the serialized corpus remains admitted");

        assert_eq!(decoded, corpus);
        assert_eq!(decoded.format_version, CORPUS_FORMAT_VERSION);
        assert_eq!(decoded.cases.len(), corpus.cases.len());
        assert_eq!(decoded.cases[0].expected, ApprovalDisposition::Approve);
        assert_eq!(decoded.cases[1].expected, ApprovalDisposition::Deny);
        assert_eq!(
            decoded.cases[2].expected,
            ApprovalDisposition::EscalateToHuman
        );
    }

    #[tokio::test]
    async fn scorer_reports_case_verdicts_and_per_disposition_precision_recall() {
        let corpus = decode_corpus(SEED_CORPUS).expect("the checked-in seed corpus is admitted");
        let (model, binding) = fixture_model([
            scripted_decision(ApprovalDisposition::Approve, APPROVE_RATIONALE),
            scripted_decision(ApprovalDisposition::EscalateToHuman, ESCALATE_RATIONALE),
            scripted_decision(ApprovalDisposition::Deny, DENY_RATIONALE),
        ]);

        let scorecard = score_corpus(&model, &binding, &corpus)
            .await
            .expect("the scripted judge scores every seed case");

        assert_eq!(scorecard.accuracy, MetricRate::new(1, corpus.cases.len()));
        assert_eq!(scorecard.verdicts.len(), corpus.cases.len());
        assert_eq!(scorecard.verdicts[0].actual, ApprovalDisposition::Approve);
        assert_eq!(
            scorecard.verdicts[1].actual,
            ApprovalDisposition::EscalateToHuman
        );
        assert_eq!(scorecard.verdicts[2].actual, ApprovalDisposition::Deny);
        assert_eq!(
            scorecard.dispositions[0],
            DispositionMetrics {
                disposition: ApprovalDisposition::Approve,
                true_positives: 1,
                false_positives: 0,
                false_negatives: 0,
                precision: MetricRate::new(1, 1),
                recall: MetricRate::new(1, 1),
            }
        );
        assert_eq!(
            scorecard.dispositions[1],
            DispositionMetrics {
                disposition: ApprovalDisposition::Deny,
                true_positives: 0,
                false_positives: 1,
                false_negatives: 1,
                precision: MetricRate::new(0, 1),
                recall: MetricRate::new(0, 1),
            }
        );
        assert_eq!(
            scorecard.dispositions[2],
            DispositionMetrics {
                disposition: ApprovalDisposition::EscalateToHuman,
                true_positives: 0,
                false_positives: 1,
                false_negatives: 1,
                precision: MetricRate::new(0, 1),
                recall: MetricRate::new(0, 1),
            }
        );
    }

    fn fixture_model(
        scripts: impl IntoIterator<Item = Script>,
    ) -> (
        RuntimeApprovalJudgeModel<ScriptedModel<ModelCallId>>,
        ApprovalJudgeEvalBinding,
    ) {
        let target =
            ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(20)));
        let catalog = RuntimeModelCatalog::try_from_definitions([RuntimeModelDefinition::try_new(
            target,
            String::from(PROVIDER_MODEL),
            256,
            4_096,
        )
        .expect("the fixture model definition is request-safe")])
        .expect("the fixture catalog names one target once");
        (
            RuntimeApprovalJudgeModel::new(ScriptedModel::following(scripts), catalog),
            ApprovalJudgeEvalBinding {
                selection: DirectModelSelection::from_uuid(Uuid::from_u128(21)),
                target,
                credential_reference: String::from("offline-fixture-credential"),
            },
        )
    }

    fn scripted_decision(disposition: ApprovalDisposition, rationale: &str) -> Script {
        let arguments_json = serde_json::json!({
            "recommendation": disposition.as_str(),
            "rationale": rationale,
        })
        .to_string();
        Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new(PROVIDER_MODEL)),
            finish: CompletionFinish::ToolUse,
            content: vec![AssistantPart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("offline_fixture_decision"),
                name: ToolName::new("tool_approval_decision"),
                arguments_json,
            })],
            usage: TokenUsage::unreported(),
        }))
    }

    #[test]
    fn unsupported_corpus_version_fails_closed() {
        let corpus = ApprovalJudgeCorpus {
            format_version: CORPUS_FORMAT_VERSION + 1,
            cases: Vec::new(),
        };
        let encoded = serde_json::to_vec(&corpus).expect("the unsupported fixture serializes");

        let error = decode_corpus(&encoded).expect_err("an unknown corpus version is rejected");

        assert!(error.to_string().contains("unsupported"));
    }

    #[test]
    fn zero_denominator_metric_has_no_decimal_value() {
        assert_eq!(
            MetricRate::new(0, 0),
            MetricRate {
                numerator: 0,
                denominator: 0,
                value: None,
            }
        );
    }
}
