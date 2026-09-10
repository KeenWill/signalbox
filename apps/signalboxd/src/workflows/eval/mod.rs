//! Ordered approval-judge measurements through journaled effects; docs/spec/eval-system.md.

mod effects;
mod records;
mod seal;

pub use effects::{EvalServices, EvaluationEffects};
pub use records::*;

use signalbox_approval_judge_eval::{ApprovalJudgeCaseVerdict, ApprovalJudgeScorecard, live};
use signalbox_domain::{EffectRequest, InlineFramePayload, ProgramCapability};
use signalbox_workflow_runtime::native::{
    NativeProgram, NativeProgramError, NativeValue, WorkflowContext,
};

pub const EVAL_ENTRY: &str = "approval-judge-eval";
pub const EVAL_REVISION: &str = "1";

pub struct ApprovalJudgeEval;

impl NativeValue for EvalManifest {
    fn decode(bytes: &[u8]) -> Result<Self, NativeProgramError> {
        let manifest: Self = decode(bytes)?;
        manifest.validate()?;
        Ok(manifest)
    }
    fn encode(&self) -> Result<Vec<u8>, NativeProgramError> {
        self.validate()?;
        encode(self)
    }
}

pub struct EvalScorecard(pub serde_json::Value);

impl NativeValue for EvalScorecard {
    fn decode(bytes: &[u8]) -> Result<Self, NativeProgramError> {
        Ok(Self(decode(bytes)?))
    }
    fn encode(&self) -> Result<Vec<u8>, NativeProgramError> {
        encode(&self.0)
    }
}

impl NativeProgram for ApprovalJudgeEval {
    type Input = EvalManifest;
    type Output = EvalScorecard;

    async fn run(
        mut context: WorkflowContext,
        input: EvalManifest,
    ) -> Result<EvalScorecard, NativeProgramError> {
        let corpus: CorpusAnswer =
            invoke(&mut context, ProgramCapability::Corpus, "load", &Empty {}).await?;
        let mut outcomes = Vec::new();
        for trial in 0..input.trial_count()? {
            let outcome = invoke::<_, JudgeAnswer>(
                &mut context,
                ProgramCapability::Judge,
                "evaluate",
                &TrialRequest { trial },
            )
            .await?;
            if input.format == CorpusFormat::Offline
                && !matches!(outcome, JudgeAnswer::Verdict { .. })
            {
                return Err(NativeProgramError::new("offline trial has no verdict"));
            }
            outcomes.push(outcome);
        }
        let scorecard = score(&input, &corpus, &outcomes)?;
        let _: SealAnswer = invoke(
            &mut context,
            ProgramCapability::EvalRecord,
            "seal",
            &SealRequest {
                scorecard: scorecard.clone(),
            },
        )
        .await?;
        Ok(EvalScorecard(scorecard))
    }
}

async fn invoke<I: serde::Serialize, O: serde::de::DeserializeOwned>(
    context: &mut WorkflowContext,
    capability: ProgramCapability,
    method: &str,
    input: &I,
) -> Result<O, NativeProgramError> {
    let answer = context
        .effect(EffectRequest::new(
            capability,
            method.into(),
            InlineFramePayload::new(encode(input)?),
        ))
        .await?;
    decode(answer.as_bytes())
}

fn score(
    input: &EvalManifest,
    corpus: &CorpusAnswer,
    outcomes: &[JudgeAnswer],
) -> Result<serde_json::Value, NativeProgramError> {
    if corpus.cases.len() != input.cases.len() || outcomes.len() != input.trial_count()? as usize {
        return Err(NativeProgramError::new("incomplete evaluation evidence"));
    }
    match input.format {
        CorpusFormat::Offline => {
            let mut verdicts = Vec::new();
            for (case, outcome) in corpus.cases.iter().zip(outcomes) {
                let Case::Offline(case) = case else {
                    return Err(NativeProgramError::new("wrong corpus case format"));
                };
                let JudgeAnswer::Verdict {
                    actual, rationale, ..
                } = outcome
                else {
                    return Err(NativeProgramError::new("offline trial has no verdict"));
                };
                verdicts.push(ApprovalJudgeCaseVerdict {
                    case_id: case.id.clone(),
                    expected: case.expected,
                    actual: *actual,
                    correct: case.expected == *actual,
                    rationale: rationale.clone(),
                    label_provenance: case.label_provenance.clone(),
                });
            }
            serde_json::to_value(ApprovalJudgeScorecard::from_verdicts(verdicts))
                .map_err(codec_error)
        }
        CorpusFormat::Live => {
            let mut scores = std::collections::BTreeMap::new();
            let mut reports = Vec::new();
            for (case, outcomes) in corpus
                .cases
                .iter()
                .zip(outcomes.chunks(input.repeats as usize))
            {
                let Case::Live(case) = case else {
                    return Err(NativeProgramError::new("wrong corpus case format"));
                };
                let mut verdicts = Vec::new();
                let mut failures = Vec::new();
                for outcome in outcomes {
                    match outcome {
                        JudgeAnswer::Verdict {
                            actual,
                            rationale,
                            provider_reported_model,
                            usage,
                            ..
                        } => verdicts.push(live::ScoredVerdict {
                            recommendation: recommendation(*actual),
                            rationale: rationale.clone(),
                            provider_reported_model: provider_reported_model.clone(),
                            usage: usage.domain(),
                        }),
                        JudgeAnswer::Failed { cause, .. } => failures.push(cause.clone()),
                        JudgeAnswer::Ambiguous => failures.push("ambiguous".into()),
                    }
                }
                reports.push(live::score_case(
                    case,
                    input.repeats as usize,
                    &verdicts,
                    failures,
                    input.postures.get(&case.tool).map(String::as_str),
                    &mut scores,
                ));
            }
            let rendered = live::render_scorecard(
                live::ScorecardMetadata {
                    scoring_semantics_version: 3,
                    judge_selection: input.binding.selection.clone(),
                    provider_model: input.binding.provider_model.clone(),
                    corpus_digest: corpus.corpus_digest.clone(),
                    contract_digest: input.binding.contract_digest.clone(),
                    rendered_digest: corpus.rendered_digest.clone(),
                    repeats: input.repeats as usize,
                    speculative_tools: input.speculative_tools.clone(),
                },
                &scores,
                reports,
            )
            .map_err(NativeProgramError::new)?;
            serde_json::from_str(&rendered).map_err(codec_error)
        }
    }
}

fn recommendation(
    value: signalbox_approval_judge_eval::ApprovalDisposition,
) -> signalbox_domain::DelegateApprovalRecommendation {
    use signalbox_approval_judge_eval::ApprovalDisposition;
    use signalbox_domain::DelegateApprovalRecommendation;
    match value {
        ApprovalDisposition::Approve => DelegateApprovalRecommendation::Approve,
        ApprovalDisposition::Deny => DelegateApprovalRecommendation::Deny,
        ApprovalDisposition::EscalateToHuman => DelegateApprovalRecommendation::EscalateToHuman,
    }
}

fn codec_error(error: serde_json::Error) -> NativeProgramError {
    NativeProgramError::new(error.to_string())
}
fn encode(value: &impl serde::Serialize) -> Result<Vec<u8>, NativeProgramError> {
    serde_json::to_vec(value).map_err(codec_error)
}
fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, NativeProgramError> {
    serde_json::from_slice(bytes).map_err(codec_error)
}
