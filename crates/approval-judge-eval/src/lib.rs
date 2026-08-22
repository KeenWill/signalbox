//! Offline corpus and scoring support for the current binary approval judge.
//!
//! The harness delegates request rendering and model-output decoding to
//! [`signalboxd::approval_judge_eval`], so evaluations exercise the same
//! prompt and decision assembly as daemon execution without entering the
//! daemon's durable decision path.

use std::{
    collections::HashSet,
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use signalbox_domain::DelegateApprovalRecommendation;
use signalbox_model_provider_runtime::ApprovalJudgeModel;
use signalboxd::approval_judge_eval::{
    ApprovalJudgeEvalBinding, ApprovalJudgeEvalCase, judge_eval_case, judge_system_prompt,
    render_eval_case,
};

mod database;
pub mod manifest;
pub mod store;

pub use database::DatabaseCorpusStore;
pub use store::{
    CorpusKey, CorpusRegistration, CorpusSourceDescriptor, CorpusStore, CorpusStoreCorruption,
    CorpusStoreError, CorpusStoreFuture, DigestParseError, DiskCorpusStore, Sha256Digest,
};

/// The only corpus format this pre-alpha harness currently accepts.
pub const CORPUS_FORMAT_VERSION: u32 = 1;
// Hard safety ceiling bounding manifest, hashing, and durable-index memory and
// storage amplification from attacker-controlled case identities.
const MAX_CASE_ID_BYTES: usize = 128;

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
    /// Tool-request and authority context admitted by the daemon renderer.
    pub request: ApprovalJudgeRequestContext,
    /// Labeled binary-judge disposition.
    pub expected: ApprovalDisposition,
    /// Free-text provenance explaining where the label came from.
    pub label_provenance: String,
}

/// Request fields and frozen authority context used by replay.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalJudgeRequestContext {
    /// Exact tool name.
    pub tool: String,
    /// Provider argument text normalized by the daemon request renderer.
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
    decode_corpus(&bytes).map_err(|error| match error {
        CorpusLoadError::Json(source) => CorpusLoadError::JsonInFile {
            path: path.to_path_buf(),
            source,
        },
        other => other,
    })
}

/// Lowercase hexadecimal SHA-256 binding a case's rendered request identity.
///
/// The digest input follows the corpus digest conventions: one JSON object
/// with bytewise-sorted keys and no insignificant whitespace; absent optional
/// fields serialize as `null`. It covers the case id, every request field,
/// and the exact judge system prompt, so a recorded response is invalidated
/// by a case rename, a request edit, or a prompt revision alike.
#[must_use]
pub fn request_fingerprint(case: &ApprovalJudgeCase) -> String {
    // The canonical object is written field by field in bytewise key order,
    // with `serde_json::Value`'s infallible display doing the string
    // escaping, so no fallible serializer sits on this path.
    fn field(value: Option<&str>) -> String {
        value.map_or_else(
            || String::from("null"),
            |text| serde_json::Value::from(text).to_string(),
        )
    }
    let request = &case.request;
    let encoded = format!(
        "{{\"arguments\":{},\"case_id\":{},\"commissioned_goal\":{},\"frozen_system_prompt\":{},\"judge_system_prompt\":{},\"session_template\":{},\"tool\":{}}}",
        field(Some(request.arguments.as_str())),
        field(Some(case.id.as_str())),
        field(request.commissioned_goal.as_deref()),
        field(request.frozen_system_prompt.as_deref()),
        field(Some(judge_system_prompt())),
        field(request.session_template.as_deref()),
        field(Some(request.tool.as_str())),
    );
    let digest = Sha256::digest(encoded.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Decodes and validates a corpus JSON document from bytes.
pub fn decode_corpus(bytes: &[u8]) -> Result<ApprovalJudgeCorpus, CorpusLoadError> {
    let corpus: ApprovalJudgeCorpus =
        serde_json::from_slice(bytes).map_err(CorpusLoadError::Json)?;
    validate_corpus(&corpus)?;
    Ok(corpus)
}

pub(crate) fn validate_corpus(corpus: &ApprovalJudgeCorpus) -> Result<(), CorpusLoadError> {
    if corpus.format_version != CORPUS_FORMAT_VERSION {
        return Err(CorpusLoadError::UnsupportedFormatVersion {
            observed: corpus.format_version,
        });
    }
    if corpus.cases.is_empty() {
        return Err(CorpusLoadError::EmptyCorpus);
    }
    let mut case_ids = HashSet::with_capacity(corpus.cases.len());
    for case in &corpus.cases {
        if let Some(field) = nul_case_field(case) {
            return Err(CorpusLoadError::NulCaseString {
                id: case.id.clone(),
                field,
            });
        }
        validate_case_id(&case.id).map_err(|error| match error {
            CaseIdError::Blank => CorpusLoadError::BlankCaseId,
            CaseIdError::Invalid => CorpusLoadError::InvalidCaseId {
                id: case.id.clone(),
            },
        })?;
        if !case_ids.insert(case.id.as_str()) {
            return Err(CorpusLoadError::DuplicateCaseId {
                id: case.id.clone(),
            });
        }
        if case.label_provenance.trim().is_empty() {
            return Err(CorpusLoadError::MissingLabelProvenance {
                id: case.id.clone(),
            });
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaseIdError {
    Blank,
    Invalid,
}

fn validate_case_id(id: &str) -> Result<(), CaseIdError> {
    if id.trim().is_empty() {
        return Err(CaseIdError::Blank);
    }
    if id.len() > MAX_CASE_ID_BYTES || id.chars().any(char::is_control) {
        return Err(CaseIdError::Invalid);
    }
    Ok(())
}

fn nul_case_field(case: &ApprovalJudgeCase) -> Option<&'static str> {
    if case.id.contains('\0') {
        Some("id")
    } else if case.request.tool.contains('\0') {
        Some("tool")
    } else if case.request.arguments.contains('\0') {
        Some("arguments")
    } else if case
        .request
        .commissioned_goal
        .as_deref()
        .is_some_and(|value| value.contains('\0'))
    {
        Some("commissioned_goal")
    } else if case
        .request
        .session_template
        .as_deref()
        .is_some_and(|value| value.contains('\0'))
    {
        Some("session_template")
    } else if case
        .request
        .frozen_system_prompt
        .as_deref()
        .is_some_and(|value| value.contains('\0'))
    {
        Some("frozen_system_prompt")
    } else if case.label_provenance.contains('\0') {
        Some("label_provenance")
    } else {
        None
    }
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
    /// JSON decoding or strict shape validation failed for a named file.
    JsonInFile {
        /// Corpus file that failed to decode.
        path: PathBuf,
        /// Underlying decode failure.
        source: serde_json::Error,
    },
    /// The corpus names a format this harness does not implement.
    UnsupportedFormatVersion {
        /// Version found in the document.
        observed: u32,
    },
    /// The corpus contains no evaluation cases.
    EmptyCorpus,
    /// More than one case uses the same stable logical identity.
    DuplicateCaseId {
        /// Repeated case identity.
        id: String,
    },
    /// A case id is empty or whitespace-only and carries no stable identity.
    BlankCaseId,
    /// A case id exceeds the shared byte ceiling or contains a control character.
    InvalidCaseId {
        /// Rejected case identity.
        id: String,
    },
    /// A case string contains U+0000, which PostgreSQL JSONB cannot preserve.
    NulCaseString {
        /// Case carrying the unsupported string.
        id: String,
        /// Case field containing U+0000.
        field: &'static str,
    },
    /// A case does not identify the source of its expected label.
    MissingLabelProvenance {
        /// Case without meaningful label provenance.
        id: String,
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
            Self::JsonInFile { path, source } => write!(
                formatter,
                "corpus {} is not valid corpus JSON: {source}",
                path.display()
            ),
            Self::UnsupportedFormatVersion { observed } => write!(
                formatter,
                "corpus format version {observed} is unsupported; expected {CORPUS_FORMAT_VERSION}"
            ),
            Self::EmptyCorpus => write!(formatter, "corpus contains no cases"),
            Self::DuplicateCaseId { id } => {
                write!(formatter, "corpus case id {id} appears more than once")
            }
            Self::BlankCaseId => write!(
                formatter,
                "corpus contains a case whose id is empty or whitespace-only"
            ),
            Self::InvalidCaseId { id } => write!(
                formatter,
                "corpus case id {id:?} exceeds 128 bytes or contains control characters"
            ),
            Self::NulCaseString { id, field } => write!(
                formatter,
                "corpus case {id:?} field {field} contains unsupported U+0000"
            ),
            Self::MissingLabelProvenance { id } => {
                write!(formatter, "corpus case {id} has no label provenance")
            }
        }
    }
}

impl Error for CorpusLoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Json(source) => Some(source),
            Self::JsonInFile { source, .. } => Some(source),
            Self::UnsupportedFormatVersion { .. }
            | Self::EmptyCorpus
            | Self::DuplicateCaseId { .. }
            | Self::BlankCaseId
            | Self::InvalidCaseId { .. }
            | Self::NulCaseString { .. }
            | Self::MissingLabelProvenance { .. } => None,
        }
    }
}

/// Replays and scores every corpus case through the current binary judge path.
pub async fn score_corpus(
    model: &dyn ApprovalJudgeModel,
    binding: &ApprovalJudgeEvalBinding,
    corpus: &ApprovalJudgeCorpus,
) -> Result<ApprovalJudgeScorecard, ScoreError> {
    if corpus.format_version != CORPUS_FORMAT_VERSION {
        return Err(ScoreError::UnsupportedFormatVersion {
            observed: corpus.format_version,
        });
    }
    if corpus.cases.is_empty() {
        return Err(ScoreError::EmptyCorpus);
    }
    let mut case_ids = HashSet::with_capacity(corpus.cases.len());
    for case in &corpus.cases {
        if let Some(field) = nul_case_field(case) {
            return Err(ScoreError::NulCaseString {
                id: case.id.clone(),
                field,
            });
        }
        match validate_case_id(&case.id) {
            Ok(()) => {}
            Err(CaseIdError::Blank) => return Err(ScoreError::BlankCaseId),
            Err(CaseIdError::Invalid) => {
                return Err(ScoreError::InvalidCaseId {
                    id: case.id.clone(),
                });
            }
        }
        if !case_ids.insert(case.id.as_str()) {
            return Err(ScoreError::DuplicateCaseId {
                id: case.id.clone(),
            });
        }
        if case.label_provenance.trim().is_empty() {
            return Err(ScoreError::MissingLabelProvenance {
                id: case.id.clone(),
            });
        }
    }

    let eval_cases = corpus.cases.iter().map(eval_case).collect::<Vec<_>>();
    for (case, eval_case) in corpus.cases.iter().zip(&eval_cases) {
        render_eval_case(eval_case).map_err(|source| ScoreError::Case {
            case_id: case.id.clone(),
            source: Box::new(source),
        })?;
    }

    let mut verdicts = Vec::with_capacity(corpus.cases.len());
    for (case, eval_case) in corpus.cases.iter().zip(&eval_cases) {
        let result = judge_eval_case(model, binding, eval_case)
            .await
            .map_err(|source| ScoreError::Case {
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

fn eval_case(case: &ApprovalJudgeCase) -> ApprovalJudgeEvalCase {
    ApprovalJudgeEvalCase {
        name: case.id.clone(),
        tool: case.request.tool.clone(),
        arguments: case.request.arguments.clone(),
        goal: case.request.commissioned_goal.clone(),
        template: case.request.session_template.clone(),
        system_prompt: case.request.frozen_system_prompt.clone(),
        // This corpus carries no repository-watch fence: `request_fingerprint`
        // binds a fixed set of request fields, and adding one would change
        // every recorded response's identity. Its cases therefore state their
        // grant in goal and prompt text, and a case whose verdict turns on a
        // fence belongs in the replay corpus that can express one.
        dispatch: None,
    }
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

/// A corpus could not be scored through the judge adapter.
#[derive(Debug)]
pub enum ScoreError {
    /// The corpus names a format this scorer does not implement.
    UnsupportedFormatVersion {
        /// Version found in the corpus.
        observed: u32,
    },
    /// The corpus contains no evaluation cases.
    EmptyCorpus,
    /// More than one case uses the same stable logical identity.
    DuplicateCaseId {
        /// Repeated case identity.
        id: String,
    },
    /// A case id is empty or whitespace-only and carries no stable identity.
    BlankCaseId,
    /// A case id exceeds the shared byte ceiling or contains a control character.
    InvalidCaseId {
        /// Rejected case identity.
        id: String,
    },
    /// A case string contains U+0000, which no admitted store can preserve.
    NulCaseString {
        /// Case carrying the unsupported string.
        id: String,
        /// Case field containing U+0000.
        field: &'static str,
    },
    /// A case does not identify the source of its expected label.
    MissingLabelProvenance {
        /// Case without meaningful label provenance.
        id: String,
    },
    /// One case failed admission or replay.
    Case {
        /// Logical identity of the failed case.
        case_id: String,
        /// Underlying admission or replay failure.
        source: Box<dyn Error + Send + Sync>,
    },
}

impl ScoreError {
    /// Returns the logical identity of the failed case, when applicable.
    #[must_use]
    pub fn case_id(&self) -> Option<&str> {
        match self {
            Self::UnsupportedFormatVersion { .. } | Self::EmptyCorpus | Self::BlankCaseId => None,
            Self::DuplicateCaseId { id }
            | Self::InvalidCaseId { id }
            | Self::NulCaseString { id, .. }
            | Self::MissingLabelProvenance { id } => Some(id),
            Self::Case { case_id, .. } => Some(case_id),
        }
    }
}

impl fmt::Display for ScoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFormatVersion { observed } => write!(
                formatter,
                "corpus format version {observed} is unsupported; expected {CORPUS_FORMAT_VERSION}"
            ),
            Self::EmptyCorpus => write!(formatter, "corpus contains no cases"),
            Self::DuplicateCaseId { id } => {
                write!(formatter, "corpus case id {id} appears more than once")
            }
            Self::BlankCaseId => write!(
                formatter,
                "corpus contains a case whose id is empty or whitespace-only"
            ),
            Self::InvalidCaseId { id } => write!(
                formatter,
                "corpus case id {id:?} exceeds 128 bytes or contains control characters"
            ),
            Self::NulCaseString { id, field } => write!(
                formatter,
                "corpus case {id:?} field {field} contains unsupported U+0000"
            ),
            Self::MissingLabelProvenance { id } => {
                write!(formatter, "corpus case {id} has no label provenance")
            }
            Self::Case { case_id, source } => write!(
                formatter,
                "approval-judge replay failed for case {case_id}: {source}"
            ),
        }
    }
}

impl Error for ScoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::UnsupportedFormatVersion { .. }
            | Self::EmptyCorpus
            | Self::DuplicateCaseId { .. }
            | Self::BlankCaseId
            | Self::InvalidCaseId { .. }
            | Self::NulCaseString { .. }
            | Self::MissingLabelProvenance { .. } => None,
            Self::Case { source, .. } => Some(source.as_ref()),
        }
    }
}

#[cfg(test)]
mod tests {
    use expect_test::expect;
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
        MetricRate, ScoreError, decode_corpus, request_fingerprint, score_corpus,
    };

    const SEED_CORPUS: &[u8] = include_bytes!("../corpora/seed-v1.json");
    const SEED_RESPONSES: &[u8] = include_bytes!("../corpora/seed-responses-v1.json");
    // Arbitrary admitted-fixture constructor parameters: replay reads neither,
    // they only need to form a request-safe model definition.
    const FIXTURE_MAX_OUTPUT_TOKENS: u32 = 256;
    const FIXTURE_CONTEXT_WINDOW_TOKENS: u32 = 4_096;
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
    }

    #[test]
    fn duplicate_corpus_case_ids_fail_closed() {
        let mut corpus =
            decode_corpus(SEED_CORPUS).expect("the checked-in seed corpus is admitted");
        let duplicate_id = corpus.cases[0].id.clone();
        corpus.cases[1].id = duplicate_id.clone();
        let encoded = serde_json::to_vec(&corpus).expect("the duplicate fixture serializes");

        let error = decode_corpus(&encoded).expect_err("a duplicate case id is rejected");

        expect![["corpus case id synthetic-read-source-file appears more than once"]]
            .assert_eq(&error.to_string());
    }

    #[tokio::test]
    async fn scoring_rejects_directly_constructed_blank_case_id() {
        let mut corpus =
            decode_corpus(SEED_CORPUS).expect("the checked-in seed corpus is admitted");
        corpus.cases[0].id = "   ".to_string();
        let (model, binding) = fixture_model([]);

        let error = score_corpus(&model, &binding, &corpus)
            .await
            .expect_err("the scoring boundary rejects a blank case id");

        assert!(matches!(error, ScoreError::BlankCaseId));
    }

    fn assert_seed_fingerprint(
        corpus: &ApprovalJudgeCorpus,
        responses: &serde_json::Value,
        index: usize,
    ) {
        let case = &corpus.cases[index];
        let recorded = responses["responses"][index]["request_fingerprint"]
            .as_str()
            .expect("the seed response names a fingerprint");
        assert_eq!(
            recorded,
            request_fingerprint(case),
            "fingerprint mismatch for case {}",
            case.id
        );
    }

    #[test]
    fn seed_response_fingerprints_match_the_seed_corpus() {
        let corpus = decode_corpus(SEED_CORPUS).expect("the checked-in seed corpus is admitted");
        let responses: serde_json::Value =
            serde_json::from_slice(SEED_RESPONSES).expect("the seed responses parse");

        assert_seed_fingerprint(&corpus, &responses, 0);
        assert_seed_fingerprint(&corpus, &responses, 1);
        assert_seed_fingerprint(&corpus, &responses, 2);
    }

    #[test]
    fn blank_corpus_case_ids_fail_closed() {
        let mut corpus =
            decode_corpus(SEED_CORPUS).expect("the checked-in seed corpus is admitted");
        corpus.cases[0].id = "   ".to_string();
        let encoded = serde_json::to_vec(&corpus).expect("the blank-id fixture serializes");

        let error = decode_corpus(&encoded).expect_err("a blank case id is rejected");

        expect![["corpus contains a case whose id is empty or whitespace-only"]]
            .assert_eq(&error.to_string());
    }

    #[test]
    fn overlong_corpus_case_ids_fail_shared_admission() {
        let mut corpus =
            decode_corpus(SEED_CORPUS).expect("the checked-in seed corpus is admitted");
        corpus.cases[0].id = "x".repeat(129);
        let encoded = serde_json::to_vec(&corpus).expect("the overlong fixture serializes");

        let error = decode_corpus(&encoded).expect_err("an overlong case id is rejected");

        assert!(matches!(
            error,
            super::CorpusLoadError::InvalidCaseId { .. }
        ));
    }

    #[test]
    fn nul_bearing_case_strings_fail_shared_corpus_admission() {
        let mut corpus =
            decode_corpus(SEED_CORPUS).expect("the checked-in seed corpus is admitted");
        corpus.cases[0].label_provenance = String::from("fixture provenance\0suffix");
        let encoded = serde_json::to_vec(&corpus).expect("the NUL-bearing fixture serializes");

        let error = decode_corpus(&encoded)
            .expect_err("a case string that JSONB cannot preserve is rejected");

        assert!(error.to_string().contains("label_provenance"));
        assert!(error.to_string().contains("U+0000"));
    }

    #[tokio::test]
    async fn scorer_reports_case_verdicts() {
        let corpus = decode_corpus(SEED_CORPUS).expect("the checked-in seed corpus is admitted");
        let response_fixture = [
            (ApprovalDisposition::Approve, APPROVE_RATIONALE),
            (ApprovalDisposition::EscalateToHuman, ESCALATE_RATIONALE),
            (ApprovalDisposition::Deny, DENY_RATIONALE),
        ];
        let expected_verdicts = response_fixture.map(|(disposition, _)| disposition);
        let scripts = response_fixture
            .map(|(disposition, rationale)| scripted_decision(disposition, rationale));
        let (model, binding) = fixture_model(scripts);

        let scorecard = score_corpus(&model, &binding, &corpus)
            .await
            .expect("the scripted judge scores every seed case");

        assert_eq!(scorecard.verdicts.len(), corpus.cases.len());
        assert_eq!(scorecard.verdicts[0].actual, expected_verdicts[0]);
        assert_eq!(scorecard.verdicts[1].actual, expected_verdicts[1]);
        assert_eq!(scorecard.verdicts[2].actual, expected_verdicts[2]);
    }

    #[tokio::test]
    async fn scorer_reports_aggregate_accuracy() {
        let corpus = decode_corpus(SEED_CORPUS).expect("the checked-in seed corpus is admitted");
        let (model, binding) = fixture_model([
            scripted_decision(ApprovalDisposition::Approve, APPROVE_RATIONALE),
            scripted_decision(ApprovalDisposition::EscalateToHuman, ESCALATE_RATIONALE),
            scripted_decision(ApprovalDisposition::Deny, DENY_RATIONALE),
        ]);

        let scorecard = score_corpus(&model, &binding, &corpus)
            .await
            .expect("the scripted judge scores every seed case");

        assert_eq!(scorecard.accuracy.numerator, 1);
        assert_eq!(scorecard.accuracy.denominator, 3);
    }

    #[tokio::test]
    async fn scorer_reports_per_disposition_precision_recall() {
        let corpus = decode_corpus(SEED_CORPUS).expect("the checked-in seed corpus is admitted");
        let (model, binding) = fixture_model([
            scripted_decision(ApprovalDisposition::Approve, APPROVE_RATIONALE),
            scripted_decision(ApprovalDisposition::EscalateToHuman, ESCALATE_RATIONALE),
            scripted_decision(ApprovalDisposition::Deny, DENY_RATIONALE),
        ]);

        let scorecard = score_corpus(&model, &binding, &corpus)
            .await
            .expect("the scripted judge scores every seed case");

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
            FIXTURE_MAX_OUTPUT_TOKENS,
            FIXTURE_CONTEXT_WINDOW_TOKENS,
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

        expect![["corpus format version 2 is unsupported; expected 1"]]
            .assert_eq(&error.to_string());
    }

    #[test]
    fn empty_corpus_fails_closed() {
        let corpus = ApprovalJudgeCorpus {
            format_version: CORPUS_FORMAT_VERSION,
            cases: Vec::new(),
        };
        let encoded = serde_json::to_vec(&corpus).expect("the empty fixture serializes");

        let error = decode_corpus(&encoded).expect_err("an empty corpus is rejected");

        expect![["corpus contains no cases"]].assert_eq(&error.to_string());
    }

    #[tokio::test]
    async fn scoring_rejects_directly_constructed_unsupported_corpus() {
        let corpus = ApprovalJudgeCorpus {
            format_version: CORPUS_FORMAT_VERSION + 1,
            cases: Vec::new(),
        };
        let (model, binding) = fixture_model([]);

        let error = score_corpus(&model, &binding, &corpus)
            .await
            .expect_err("the scoring boundary rejects an unsupported version");

        expect![["corpus format version 2 is unsupported; expected 1"]]
            .assert_eq(&error.to_string());
    }

    #[tokio::test]
    async fn scoring_rejects_directly_constructed_empty_corpus() {
        let corpus = ApprovalJudgeCorpus {
            format_version: CORPUS_FORMAT_VERSION,
            cases: Vec::new(),
        };
        let (model, binding) = fixture_model([]);

        let error = score_corpus(&model, &binding, &corpus)
            .await
            .expect_err("the scoring boundary rejects an empty corpus");

        expect![["corpus contains no cases"]].assert_eq(&error.to_string());
    }

    #[tokio::test]
    async fn scoring_rejects_directly_constructed_duplicate_case_ids() {
        let mut corpus =
            decode_corpus(SEED_CORPUS).expect("the checked-in seed corpus is admitted");
        let duplicate_id = corpus.cases[0].id.clone();
        corpus.cases[1].id = duplicate_id.clone();
        let (model, binding) = fixture_model([]);

        let error = score_corpus(&model, &binding, &corpus)
            .await
            .expect_err("the scoring boundary rejects duplicate case ids");

        expect![["corpus case id synthetic-read-source-file appears more than once"]]
            .assert_eq(&error.to_string());
    }

    #[test]
    fn corpus_case_without_label_provenance_fails_closed() {
        let mut corpus =
            decode_corpus(SEED_CORPUS).expect("the checked-in seed corpus is admitted");
        corpus.cases[0].label_provenance = String::from(" \t ");
        let encoded = serde_json::to_vec(&corpus).expect("the provenance fixture serializes");

        let error = decode_corpus(&encoded).expect_err("blank label provenance is rejected");

        expect![["corpus case synthetic-read-source-file has no label provenance"]]
            .assert_eq(&error.to_string());
    }

    #[tokio::test]
    async fn scoring_rejects_directly_constructed_case_without_label_provenance() {
        let mut corpus =
            decode_corpus(SEED_CORPUS).expect("the checked-in seed corpus is admitted");
        let missing_provenance_case_id = corpus.cases[0].id.clone();
        corpus.cases[0].label_provenance.clear();
        let (model, binding) = fixture_model([]);

        let error = score_corpus(&model, &binding, &corpus)
            .await
            .expect_err("the scoring boundary rejects missing label provenance");

        assert!(matches!(error, ScoreError::MissingLabelProvenance { .. }));
        assert_eq!(error.case_id(), Some(missing_provenance_case_id.as_str()));
    }

    #[tokio::test]
    async fn scorer_preflights_every_case_before_model_execution() {
        let mut corpus =
            decode_corpus(SEED_CORPUS).expect("the checked-in seed corpus is admitted");
        let invalid_case_id = corpus.cases[1].id.clone();
        corpus.cases[1].request.tool = String::new();
        let (model, binding) = fixture_model([]);

        let error = score_corpus(&model, &binding, &corpus)
            .await
            .expect_err("the later inadmissible case fails before the first model call");

        assert_eq!(error.case_id(), Some(invalid_case_id.as_str()));
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
