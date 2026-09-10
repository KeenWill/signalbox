//! Derive a complete recording from the calling run's immutable input and journal.

use super::{effects::*, *};
use signalbox_domain::{
    DeliveryKind, JournalFrame, RequestKind,
    evaluation::{EvaluationOutcome, EvaluationSnapshot, EvaluationTrial},
};
use signalbox_persistence::evaluation::EvaluationError;
use signalbox_workflow_runtime::effects::EffectInvocation;

impl EvalServices {
    pub(super) async fn seal(
        &self,
        invocation: EffectInvocation<'_>,
    ) -> Result<SealAnswer, EvalFailure> {
        let request: SealRequest =
            decode(invocation.request.payload().as_bytes()).map_err(failure)?;
        let manifest = self.manifest(invocation.run).await?;
        // Re-read by digest, independently of the program's corpus answer and scorecard.
        let corpus = self.corpus(&manifest).await?;
        let journal = self
            .journal
            .load(invocation.run)
            .await
            .map_err(infrastructure_failure)?
            .ok_or_else(|| failure("evaluation journal missing"))?;
        let seal_position = journal
            .entries()
            .iter()
            .find_map(|entry| match entry.frame() {
                JournalFrame::Request(frame)
                    if frame.ordinal() == invocation.ordinal
                        && frame.kind() == &RequestKind::Effect(invocation.request.clone()) =>
                {
                    Some(entry.position())
                }
                _ => None,
            })
            .ok_or_else(|| failure("seal request is not retained by the calling run"))?;
        let mut trials = Vec::new();
        let mut outcomes = Vec::new();
        for entry in journal
            .entries()
            .iter()
            .take_while(|entry| entry.position() < seal_position)
        {
            let JournalFrame::Request(frame) = entry.frame() else {
                continue;
            };
            let RequestKind::Effect(effect) = frame.kind() else {
                continue;
            };
            if effect.capability() != ProgramCapability::Judge || effect.method() != "evaluate" {
                continue;
            }
            let trial: TrialRequest = decode(effect.payload().as_bytes()).map_err(failure)?;
            if trial.trial as usize != trials.len()
                || trial.trial >= manifest.trial_count().map_err(failure)?
            {
                return Err(failure("seal trial membership differs from the manifest"));
            }
            let (position, answer) = journal
                .entries()
                .iter()
                .take_while(|entry| entry.position() < seal_position)
                .find_map(|entry| match entry.frame() {
                    JournalFrame::Delivery(delivery) => match delivery.kind() {
                        DeliveryKind::Answer { resolves, payload }
                            if *resolves == frame.ordinal() =>
                        {
                            Some((entry.position(), payload))
                        }
                        _ => None,
                    },
                    _ => None,
                })
                .ok_or_else(|| failure("seal trial has no resolved outcome"))?;
            let answer: JudgeAnswer = decode(answer.as_bytes()).map_err(failure)?;
            let outcome = match &answer {
                JudgeAnswer::Verdict { .. } => {
                    EvaluationOutcome::Verdict(serde_json::to_value(&answer).map_err(failure)?)
                }
                JudgeAnswer::Failed { .. } => {
                    EvaluationOutcome::Failed(serde_json::to_value(&answer).map_err(failure)?)
                }
                JudgeAnswer::Ambiguous => EvaluationOutcome::Ambiguous,
            };
            let case_index = (trial.trial / manifest.repeats) as usize;
            trials.push(EvaluationTrial {
                ordinal: trial.trial,
                case_position: manifest.cases[case_index],
                repeat: trial.trial % manifest.repeats,
                case: serde_json::to_value(&corpus.cases[case_index]).map_err(failure)?,
                evidence_position: position,
                outcome,
            });
            outcomes.push(answer);
        }
        let scorecard = score(&manifest, &corpus, &outcomes).map_err(failure)?;
        if scorecard != request.scorecard {
            return Err(failure("seal scorecard differs from retained evidence"));
        }
        let registration = self
            .registrations
            .for_run(invocation.run)
            .await
            .map_err(infrastructure_failure)?
            .ok_or_else(|| failure("evaluation registration missing"))?;
        let input = self
            .registrations
            .input_for_run(invocation.run)
            .await
            .map_err(infrastructure_failure)?
            .ok_or_else(|| failure("evaluation input missing"))?;
        let snapshot = EvaluationSnapshot {
            run: invocation.run,
            registration: registration.id,
            input: input.as_bytes().to_vec(),
            metadata: serde_json::json!({
                "binding": manifest.binding,
                "corpus_digest": corpus.corpus_digest,
                "rendered_digest": corpus.rendered_digest,
            }),
            scorecard_kind: match manifest.format {
                CorpusFormat::Offline => "offline",
                CorpusFormat::Live => "live",
            }
            .into(),
            scorecard,
            trials,
        };
        let receipt = self
            .recordings
            .seal(&snapshot)
            .await
            .map_err(|error| match error {
                EvaluationError::Conflict | EvaluationError::InvalidSnapshot => failure(error),
                EvaluationError::Corrupt | EvaluationError::Database(_) => {
                    infrastructure_failure(error)
                }
            })?;
        Ok(SealAnswer {
            run: receipt.run.into_uuid().to_string(),
        })
    }
}
