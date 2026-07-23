//! A deterministic scripted model for tests.
//!
//! docs/spec/runtime-substrate.md requires scripted provider fixtures to
//! declare their exact result rather than simulate one: a [`Script`] states
//! the observations to emit and the terminal evidence to return, and
//! [`ScriptedModel`] prepares and consumes an opaque script capability
//! through the same [`ModelRuntime`] surface a real adapter implements.
//! Nothing is inferred from timing, and the cancellation signal is ignored —
//! a script that describes cancellation declares cancellation evidence
//! explicitly.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

use crate::evidence::{TerminalEvidence, TerminalReport};
use crate::observation::{Observation, ObservationFact, ObservationSink};
use crate::operation::ModelOperation;
use crate::preparation::{PreparationDefect, PreparationOutcome};
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
/// Each preparation consumes the next script in order and records the
/// operation it received for later assertion; both live under one lock, so
/// concurrent preparations record operations in exactly their
/// script-consumption order. Preparation beyond the last script reports an
/// adapter defect, so script exhaustion can never be mistaken for provider
/// evidence.
#[derive(Debug)]
pub struct ScriptedModel<C> {
    state: Mutex<ScriptedState<C>>,
}

#[derive(Debug)]
struct ScriptedState<C> {
    scripts: VecDeque<Script>,
    received: Vec<ModelOperation<C>>,
}

/// An opaque one-shot scripted execution capability.
///
/// Its fields are private and the type deliberately implements neither
/// `Clone` nor diagnostic formatting, matching the provider capability shape
/// required by docs/spec/runtime-substrate.md.
pub struct ScriptedPrepared<C> {
    correlation: C,
    script: Script,
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
    type Prepared = ScriptedPrepared<C>;

    // All work happens inside the future: a created-but-never-polled
    // preparation consumes no script and records no operation.
    async fn prepare(
        &self,
        operation: ModelOperation<C>,
        _cancellation: CancellationSignal,
    ) -> PreparationOutcome<C, Self::Prepared> {
        let correlation = operation.correlation.clone();
        let script = {
            // One lock for dequeue and receipt: recorded order is
            // script-consumption order even under concurrent preparations.
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let script = state.scripts.pop_front();
            state.received.push(operation);
            script
        };
        match script {
            Some(script) => PreparationOutcome::Prepared(ScriptedPrepared {
                correlation,
                script,
            }),
            None => PreparationOutcome::Defect {
                correlation,
                defect: PreparationDefect::RequestConstructionFailed {
                    detail: "scripted model has no remaining script for this preparation"
                        .to_string(),
                },
            },
        }
    }

    async fn execute(
        &self,
        prepared: Self::Prepared,
        sink: &mut (dyn ObservationSink<C> + Send),
        _cancellation: CancellationSignal,
    ) -> TerminalReport<C> {
        let ScriptedPrepared {
            correlation,
            script,
        } = prepared;
        for fact in script.observations {
            sink.observe(Observation {
                correlation: correlation.clone(),
                fact,
            });
        }
        TerminalReport {
            correlation,
            evidence: script.terminal,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    use super::{Script, ScriptedModel};
    use crate::credential::CredentialReference;
    use crate::evidence::{CompletionEvidence, CompletionFinish, ExchangeFacts, TerminalEvidence};
    use crate::message::AssistantPart;
    use crate::observation::{Observation, ObservationFact};
    use crate::operation::ModelOperation;
    use crate::preparation::{PreparationDefect, PreparationOutcome};
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
    fn an_unpolled_preparation_consumes_nothing() {
        let model = ScriptedModel::single(Script::delivering(completed_terminal()));
        let mut observations: Vec<Observation<String>> = Vec::new();

        drop(model.prepare(operation("call-0"), CancellationSignal::never()));

        assert_eq!(model.received_operations(), vec![]);
        assert_eq!(observations, vec![]);
        let prepared = prepare_now(&model, operation("call-1"));
        let report =
            run_now(model.execute(prepared, &mut observations, CancellationSignal::never()));
        assert!(matches!(report.evidence, TerminalEvidence::Completed(_)));
    }

    #[test]
    fn dropping_a_prepared_capability_emits_and_executes_nothing() {
        let model = ScriptedModel::single(Script::delivering(completed_terminal()).observing(
            ObservationFact::TextDelta {
                index: 0,
                text: "not emitted".to_string(),
            },
        ));
        let observations: Vec<Observation<String>> = Vec::new();

        drop(prepare_now(&model, operation("call-2")));

        assert!(observations.is_empty());
        assert_eq!(model.received_operations(), vec![operation("call-2")]);
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

    fn prepare_now(
        model: &ScriptedModel<String>,
        operation: ModelOperation<String>,
    ) -> super::ScriptedPrepared<String> {
        match run_now(model.prepare(operation, CancellationSignal::never())) {
            PreparationOutcome::Prepared(prepared) => prepared,
            PreparationOutcome::Cancelled { .. } => panic!("scripted preparation cancelled"),
            PreparationOutcome::Failed { failure, .. } => {
                panic!("scripted preparation failed: {failure:?}")
            }
            PreparationOutcome::Defect { defect, .. } => {
                panic!("scripted preparation was defective: {defect:?}")
            }
        }
    }

    #[test]
    fn every_observation_carries_the_caller_correlation_verbatim() {
        let script =
            Script::delivering(completed_terminal()).observing(ObservationFact::TextDelta {
                index: 0,
                text: "scripted".to_string(),
            });
        let model = ScriptedModel::single(script.clone());
        let mut observations: Vec<Observation<String>> = Vec::new();

        let prepared = prepare_now(&model, operation("call-7"));
        run_now(model.execute(prepared, &mut observations, CancellationSignal::never()));

        assert_eq!(
            observations,
            vec![Observation {
                correlation: "call-7".to_string(),
                fact: script.observations[0].clone()
            },]
        );
    }

    #[test]
    fn declared_terminal_evidence_passes_through_verbatim() {
        let script = Script::delivering(completed_terminal());
        let model = ScriptedModel::single(script.clone());
        let mut observations: Vec<Observation<String>> = Vec::new();

        let prepared = prepare_now(&model, operation("call-3"));
        let report =
            run_now(model.execute(prepared, &mut observations, CancellationSignal::never()));

        assert_eq!(report.correlation, "call-3".to_string());
        assert_eq!(report.evidence, script.terminal);
    }

    #[test]
    fn the_executed_operation_is_recorded_for_assertion() {
        let model = ScriptedModel::single(Script::delivering(completed_terminal()));
        let mut observations: Vec<Observation<String>> = Vec::new();
        let sent = operation("call-11");

        let prepared = prepare_now(&model, sent.clone());
        run_now(model.execute(prepared, &mut observations, CancellationSignal::never()));

        assert_eq!(model.received_operations(), vec![sent]);
    }

    #[test]
    fn script_exhaustion_is_a_preparation_defect() {
        let model = ScriptedModel::following([]);

        match run_now(model.prepare(operation("call-9"), CancellationSignal::never())) {
            PreparationOutcome::Defect {
                correlation,
                defect: PreparationDefect::RequestConstructionFailed { detail },
            } => {
                assert_eq!(correlation, "call-9");
                assert_eq!(
                    detail,
                    "scripted model has no remaining script for this preparation"
                );
            }
            _ => panic!("script exhaustion must be an adapter defect"),
        }
    }
}
