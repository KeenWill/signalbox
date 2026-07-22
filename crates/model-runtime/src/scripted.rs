//! A deterministic scripted model for tests.
//!
//! ADR-0043 requires scripted provider fixtures to declare their exact
//! result rather than simulate one: a [`Script`] states the observations to
//! emit and the terminal evidence to return, and [`ScriptedModel`] replays
//! them through the same [`ModelRuntime`] surface a real adapter implements.
//! Nothing is inferred from timing, and the cancellation signal is ignored —
//! a script that describes cancellation declares cancellation evidence
//! explicitly.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

use crate::evidence::{
    PreparationFailure, ProvenUnsentEvidence, TerminalEvidence, TerminalReport, UnsentCause,
};
use crate::observation::{Observation, ObservationFact, ObservationSink};
use crate::operation::ModelOperation;
use crate::runtime::{CancellationSignal, ModelRuntime};

/// One scripted execution: the observations to emit, in order, and the
/// exact declared terminal evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct Script {
    /// Observation facts emitted in order, each correlated to the executed
    /// operation's identity.
    pub observations: Vec<ObservationFact>,
    /// The declared terminal evidence, returned verbatim.
    pub terminal: TerminalEvidence,
}

impl Script {
    /// A script that emits no observations and returns the declared
    /// terminal evidence.
    pub fn delivering(terminal: TerminalEvidence) -> Self {
        Self {
            observations: Vec::new(),
            terminal,
        }
    }

    /// Appends one observation fact to emit before the terminal evidence.
    #[must_use]
    pub fn observing(mut self, fact: ObservationFact) -> Self {
        self.observations.push(fact);
        self
    }
}

/// A deterministic fake model: replays scripts through the real
/// [`ModelRuntime`] surface.
///
/// Each execution consumes the next script in order and records the
/// operation it received for later assertion; both live under one lock, so
/// concurrent executions record operations in exactly their
/// script-consumption order. An execution beyond the last script reports a
/// proven-unsent preparation failure naming the exhaustion, so a miscounted
/// test fails on evidence rather than panicking.
#[derive(Debug)]
pub struct ScriptedModel<C> {
    state: Mutex<ScriptedState<C>>,
}

#[derive(Debug)]
struct ScriptedState<C> {
    scripts: VecDeque<Script>,
    received: Vec<ModelOperation<C>>,
}

impl<C> ScriptedModel<C> {
    /// A model that follows the given scripts, one per execution, in order.
    pub fn following(scripts: impl IntoIterator<Item = Script>) -> Self {
        Self {
            state: Mutex::new(ScriptedState {
                scripts: scripts.into_iter().collect(),
                received: Vec::new(),
            }),
        }
    }

    /// A model scripted for exactly one execution.
    pub fn single(script: Script) -> Self {
        Self::following([script])
    }

    /// Every operation this model has executed, in execution order.
    pub fn received_operations(&self) -> Vec<ModelOperation<C>>
    where
        C: Clone,
    {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .received
            .clone()
    }
}

impl<C: Clone + Send + Sync> ModelRuntime<C> for ScriptedModel<C> {
    // All work happens inside the future: a created-but-never-polled
    // execution consumes no script, records no operation, and emits no
    // observation, matching how a real adapter behaves when its future is
    // dropped before first poll.
    async fn execute(
        &self,
        operation: ModelOperation<C>,
        sink: &mut (dyn ObservationSink<C> + Send),
        _cancellation: CancellationSignal,
    ) -> TerminalReport<C> {
        {
            let correlation = operation.correlation.clone();
            let script = {
                // One lock for dequeue and receipt: recorded order is
                // script-consumption order even under concurrent executions.
                let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                let script = state.scripts.pop_front();
                state.received.push(operation);
                script
            };
            let evidence = match script {
                Some(script) => {
                    for fact in script.observations {
                        sink.observe(Observation {
                            correlation: correlation.clone(),
                            fact,
                        });
                    }
                    script.terminal
                }
                None => TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                    cause: UnsentCause::PreparationFailed(
                        PreparationFailure::UnsupportedOperation {
                            detail: "scripted model has no remaining script for this execution"
                                .to_string(),
                        },
                    ),
                }),
            };
            TerminalReport {
                correlation,
                evidence,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    use super::{Script, ScriptedModel};
    use crate::credential::CredentialReference;
    use crate::evidence::{
        CompletionEvidence, CompletionFinish, ExchangeFacts, PreparationFailure,
        ProvenUnsentEvidence, TerminalEvidence, UnsentCause,
    };
    use crate::message::AssistantPart;
    use crate::observation::{Observation, ObservationFact};
    use crate::operation::ModelOperation;
    use crate::runtime::{CancellationSignal, ModelRuntime};
    use crate::settings::ModelSettings;
    use crate::target::{ProviderReportedModel, RequestedTarget, ResolvedTarget};
    use crate::usage::TokenUsage;

    /// Resolves a scripted execution, which is ready on its first poll.
    fn run_now<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("scripted execution never pends"),
        }
    }

    #[test]
    fn an_unpolled_execution_consumes_nothing() {
        let model = ScriptedModel::single(Script::delivering(completed_terminal()));
        let mut observations: Vec<Observation<String>> = Vec::new();

        drop(model.execute(
            operation("call-0"),
            &mut observations,
            CancellationSignal::never(),
        ));

        assert_eq!(model.received_operations(), vec![]);
        assert_eq!(observations, vec![]);
        let report = run_now(model.execute(
            operation("call-1"),
            &mut observations,
            CancellationSignal::never(),
        ));
        assert!(matches!(report.evidence, TerminalEvidence::Completed(_)));
    }

    /// An operation whose correlation seed is the one knob; other facts are
    /// canonical ("model-x" targets, one user message, 64-token ceiling).
    fn operation(correlation: &str) -> ModelOperation<String> {
        ModelOperation::new(
            correlation.to_string(),
            CredentialReference::new("scripted"),
            RequestedTarget::new("model-x"),
            ResolvedTarget::new("model-x-exact"),
            vec![crate::message::ConversationMessage::user_text("hello")],
            ModelSettings::new(64),
        )
    }

    fn completed_terminal() -> TerminalEvidence {
        TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new("model-x-exact")),
            finish: CompletionFinish::EndTurn,
            content: vec![AssistantPart::Text("scripted".to_string())],
            usage: TokenUsage::unreported(),
        })
    }

    #[test]
    fn every_observation_carries_the_caller_correlation_verbatim() {
        let script = Script::delivering(completed_terminal())
            .observing(ObservationFact::RequestPrepared)
            .observing(ObservationFact::TextDelta {
                index: 0,
                text: "scripted".to_string(),
            });
        let model = ScriptedModel::single(script.clone());
        let mut observations: Vec<Observation<String>> = Vec::new();

        run_now(model.execute(
            operation("call-7"),
            &mut observations,
            CancellationSignal::never(),
        ));

        assert_eq!(
            observations,
            vec![
                Observation {
                    correlation: "call-7".to_string(),
                    fact: script.observations[0].clone()
                },
                Observation {
                    correlation: "call-7".to_string(),
                    fact: script.observations[1].clone()
                },
            ]
        );
    }

    #[test]
    fn declared_terminal_evidence_passes_through_verbatim() {
        let script = Script::delivering(completed_terminal());
        let model = ScriptedModel::single(script.clone());
        let mut observations: Vec<Observation<String>> = Vec::new();

        let report = run_now(model.execute(
            operation("call-3"),
            &mut observations,
            CancellationSignal::never(),
        ));

        assert_eq!(report.correlation, "call-3".to_string());
        assert_eq!(report.evidence, script.terminal);
    }

    #[test]
    fn the_executed_operation_is_recorded_for_assertion() {
        let model = ScriptedModel::single(Script::delivering(completed_terminal()));
        let mut observations: Vec<Observation<String>> = Vec::new();
        let sent = operation("call-11");

        run_now(model.execute(sent.clone(), &mut observations, CancellationSignal::never()));

        assert_eq!(model.received_operations(), vec![sent]);
    }

    #[test]
    fn execution_beyond_the_last_script_reports_proven_unsent_exhaustion() {
        let model = ScriptedModel::following([]);
        let mut observations: Vec<Observation<String>> = Vec::new();

        let report = run_now(model.execute(
            operation("call-9"),
            &mut observations,
            CancellationSignal::never(),
        ));

        assert_eq!(
            report.evidence,
            TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                cause: UnsentCause::PreparationFailed(PreparationFailure::UnsupportedOperation {
                    detail: "scripted model has no remaining script for this execution".to_string(),
                }),
            })
        );
        assert_eq!(observations, vec![]);
    }
}
