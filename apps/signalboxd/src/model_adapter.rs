//! Exact static model-to-adapter runtime composition.

use std::{collections::HashMap, sync::Arc};

use signalbox_model_runtime::{
    CancellationSignal, ModelOperation, ModelRuntime, ObservationSink, PreparationDefect,
    PreparationOutcome, TerminalReport,
};
use signalbox_model_runtime_codex_cli::{
    CodexCliConstructionError, CodexCliPreparedRequest, CodexCliRuntime,
};

use crate::configuration::{HubModelConfiguration, ModelAdapter};

/// One prepared capability tagged by the adapter that created it.
pub enum ConfiguredPreparedRequest<C, A, P> {
    /// Anthropic HTTP request capability.
    Anthropic { runtime: Arc<A>, prepared: P },
    /// Codex CLI process request capability.
    CodexCli {
        runtime: Arc<CodexCliRuntime>,
        prepared: Box<CodexCliPreparedRequest<C>>,
    },
}

/// Runtime router whose exact routes come only from startup configuration.
pub struct ConfiguredModelRuntime<A> {
    anthropic: Option<Arc<A>>,
    codex_cli: Option<Arc<CodexCliRuntime>>,
    routes: HashMap<String, ModelAdapter>,
}

impl<A> ConfiguredModelRuntime<A> {
    /// Constructs every configured adapter without provider interaction.
    pub fn new(
        anthropic: Option<A>,
        configuration: &HubModelConfiguration,
    ) -> Result<Self, CodexCliConstructionError> {
        Ok(Self {
            anthropic: anthropic.map(Arc::new),
            codex_cli: configuration.codex_cli_runtime()?.map(Arc::new),
            routes: configuration.adapter_routes(),
        })
    }
}

impl<A> std::fmt::Debug for ConfiguredModelRuntime<A> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfiguredModelRuntime")
            .field(
                "anthropic",
                &self.anthropic.as_ref().map(|_| "[model adapter]"),
            )
            .field(
                "codex_cli",
                &self.codex_cli.as_ref().map(|_| "[model adapter]"),
            )
            .field("route_count", &self.routes.len())
            .finish()
    }
}

fn map_preparation<C, P, R>(
    outcome: PreparationOutcome<C, P>,
    prepared: impl FnOnce(P) -> R,
) -> PreparationOutcome<C, R> {
    match outcome {
        PreparationOutcome::Prepared(value) => PreparationOutcome::Prepared(prepared(value)),
        PreparationOutcome::Cancelled { correlation } => {
            PreparationOutcome::Cancelled { correlation }
        }
        PreparationOutcome::Failed {
            correlation,
            failure,
        } => PreparationOutcome::Failed {
            correlation,
            failure,
        },
        PreparationOutcome::Defect {
            correlation,
            defect,
        } => PreparationOutcome::Defect {
            correlation,
            defect,
        },
    }
}

impl<C, A> ModelRuntime<C> for ConfiguredModelRuntime<A>
where
    C: Clone + Send + Sync,
    A: ModelRuntime<C> + Send + Sync,
    A::Prepared: Send,
{
    type Prepared = ConfiguredPreparedRequest<C, A, A::Prepared>;

    async fn prepare(
        &self,
        operation: ModelOperation<C>,
        cancellation: CancellationSignal,
    ) -> PreparationOutcome<C, Self::Prepared> {
        match self.routes.get(operation.resolved_target.as_str()) {
            Some(ModelAdapter::Anthropic) => {
                let runtime = match self.anthropic.as_ref() {
                    Some(runtime) => Arc::clone(runtime),
                    None => {
                        return PreparationOutcome::Defect {
                            correlation: operation.correlation,
                            defect: PreparationDefect::RequestConstructionFailed {
                                detail: String::from("configured Anthropic adapter is unavailable"),
                            },
                        };
                    }
                };
                let prepared = runtime.prepare(operation, cancellation).await;
                map_preparation(prepared, |prepared| ConfiguredPreparedRequest::Anthropic {
                    runtime,
                    prepared,
                })
            }
            Some(ModelAdapter::CodexCli) => match self.codex_cli.as_ref() {
                Some(runtime) => {
                    let prepared = runtime.prepare(operation, cancellation).await;
                    let runtime = Arc::clone(runtime);
                    map_preparation(prepared, |prepared| ConfiguredPreparedRequest::CodexCli {
                        runtime,
                        prepared: Box::new(prepared),
                    })
                }
                None => PreparationOutcome::Defect {
                    correlation: operation.correlation,
                    defect: PreparationDefect::RequestConstructionFailed {
                        detail: String::from("configured Codex CLI adapter is unavailable"),
                    },
                },
            },
            None => PreparationOutcome::Defect {
                correlation: operation.correlation,
                defect: PreparationDefect::RequestConstructionFailed {
                    detail: String::from("model has no configured adapter route"),
                },
            },
        }
    }

    async fn execute(
        &self,
        prepared: Self::Prepared,
        sink: &mut (dyn ObservationSink<C> + Send),
        cancellation: CancellationSignal,
    ) -> TerminalReport<C> {
        match prepared {
            ConfiguredPreparedRequest::Anthropic { runtime, prepared } => {
                runtime.execute(prepared, sink, cancellation).await
            }
            ConfiguredPreparedRequest::CodexCli { runtime, prepared } => {
                runtime.execute(*prepared, sink, cancellation).await
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use signalbox_model_runtime::{
        AssistantPart, CancellationSignal, CompletionEvidence, CompletionFinish,
        ConversationMessage, CredentialReference, ExchangeFacts, ModelOperation, ModelRuntime,
        Observation, ObservationSink, PreparationOutcome, ProviderReportedModel, RequestedTarget,
        ResolvedTarget, Script, ScriptedModel, TerminalEvidence, TokenUsage,
    };

    use crate::configuration::HubModelConfiguration;

    use super::ConfiguredModelRuntime;

    #[derive(Default)]
    struct Observations(Vec<Observation<String>>);

    impl ObservationSink<String> for Observations {
        fn observe(&mut self, observation: Observation<String>) {
            self.0.push(observation);
        }
    }

    fn prepared<P>(outcome: PreparationOutcome<String, P>) -> Option<P> {
        match outcome {
            PreparationOutcome::Prepared(prepared) => Some(prepared),
            PreparationOutcome::Cancelled { .. }
            | PreparationOutcome::Failed { .. }
            | PreparationOutcome::Defect { .. } => None,
        }
    }

    fn completion_text(evidence: &TerminalEvidence) -> Option<&str> {
        match evidence {
            TerminalEvidence::Completed(completion) => match completion.content.first() {
                Some(signalbox_model_runtime::AssistantPart::Text(text)) => Some(text.as_str()),
                Some(signalbox_model_runtime::AssistantPart::Thinking { .. })
                | Some(signalbox_model_runtime::AssistantPart::RedactedThinking { .. })
                | Some(signalbox_model_runtime::AssistantPart::ToolCall(_))
                | None => None,
            },
            TerminalEvidence::Refused(_)
            | TerminalEvidence::ProviderError(_)
            | TerminalEvidence::CancellationConfirmed(_)
            | TerminalEvidence::ProvenUnsent(_)
            | TerminalEvidence::BoundaryLoss(_) => None,
        }
    }

    fn scripted_completion(text: &str, model: &str) -> TerminalEvidence {
        TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new(model)),
            finish: CompletionFinish::EndTurn,
            content: vec![AssistantPart::Text(text.to_owned())],
            usage: TokenUsage::unreported(),
        })
    }

    #[tokio::test]
    async fn anthropic_mapping_executes_the_existing_runtime_path_unchanged() {
        let expected_completion = "unchanged Anthropic";
        let configuration = HubModelConfiguration::parse(
            r#"
version = 1

[[credential_profiles]]
name = "anthropic-primary"
billing_kind = "api_metered"

[[adapter_mappings]]
model_family = "anthropic"
adapter = "anthropic"
credential_profile = "anthropic-primary"

[compaction]
prompt = "Summarize."

[[models]]
selection_id = "10000000-0000-4000-8000-000000000001"
target_id = "20000000-0000-4000-8000-000000000001"
model_family = "anthropic"
provider_model = "claude-example"
max_output_tokens = 256
context_window_tokens = 200000
"#,
        )
        .expect("Anthropic-only configuration is valid");
        let anthropic = ScriptedModel::single(Script::delivering(scripted_completion(
            expected_completion,
            "claude-example",
        )));
        let received = anthropic.clone();
        let runtime = ConfiguredModelRuntime::new(Some(anthropic), &configuration)
            .expect("Anthropic-only runtime constructs");
        let operation = ModelOperation::new(
            String::from("anthropic-route"),
            CredentialReference::new("anthropic-primary"),
            RequestedTarget::new("anthropic-selection"),
            ResolvedTarget::new("claude-example"),
            vec![ConversationMessage::user_text("respond")],
            signalbox_model_runtime::ModelSettings::new(256),
        );

        let prepared = prepared(
            runtime
                .prepare(operation, CancellationSignal::never())
                .await,
        )
        .expect("configured Anthropic operation prepares through its existing adapter");
        let mut observations = Observations::default();
        let report = runtime
            .execute(prepared, &mut observations, CancellationSignal::never())
            .await;

        assert_eq!(completion_text(&report.evidence), Some(expected_completion));
        assert_eq!(received.received_operations().len(), 1);
        assert!(observations.0.is_empty());
    }

    #[tokio::test]
    async fn configured_codex_model_runs_through_the_cli_fake_transport() {
        let temporary = tempfile::tempdir().expect("temporary working directory is available");
        let executable = temporary.path().join("fake-codex");
        let expected_completion = "routed through Codex";
        std::fs::write(
            &executable,
            r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"type":"thread.started","thread_id":"019c0000-0000-7000-8000-000000000001"}'
printf '%s\n' '{"type":"turn.started"}'
printf '%s\n' '{"type":"item.completed","item":{"id":"message-1","type":"agent_message","text":"{\"outcome\":\"completed\",\"text\":\"<completion-text>\",\"tool_calls\":[]}"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":8,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":4,"reasoning_output_tokens":0}}'
"#
            .replace("<completion-text>", expected_completion),
        )
        .expect("fake Codex executable is writable");
        let mut permissions = std::fs::metadata(&executable)
            .expect("fake executable metadata is available")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions)
            .expect("fake Codex executable permissions are set");
        let configuration = HubModelConfiguration::parse(&format!(
            r#"
version = 1

[[credential_profiles]]
name = "anthropic-primary"
billing_kind = "api_metered"

[[credential_profiles]]
name = "codex-subscription-primary"
billing_kind = "subscription"

[[adapter_mappings]]
model_family = "anthropic"
adapter = "anthropic"
credential_profile = "anthropic-primary"

[[adapter_mappings]]
model_family = "codex"
adapter = "codex_cli"
credential_profile = "codex-subscription-primary"

[codex_cli]
executable = "{}"
working_directory = "{}"

[compaction]
prompt = "Summarize."

[[models]]
selection_id = "10000000-0000-4000-8000-000000000001"
target_id = "20000000-0000-4000-8000-000000000001"
model_family = "anthropic"
provider_model = "claude-example"
max_output_tokens = 256
context_window_tokens = 200000

[[models]]
selection_id = "10000000-0000-4000-8000-000000000002"
target_id = "20000000-0000-4000-8000-000000000002"
model_family = "codex"
provider_model = "gpt-offline-exact"
max_output_tokens = 256
context_window_tokens = 200000
"#,
            executable.display(),
            temporary.path().display(),
        ))
        .expect("Codex mapping and process paths are valid");
        let runtime = ConfiguredModelRuntime::new(None::<ScriptedModel<String>>, &configuration)
            .expect("configured adapters construct");
        assert!(format!("{runtime:?}").contains("anthropic: None"));
        let operation = ModelOperation::new(
            String::from("codex-route"),
            CredentialReference::new("codex-subscription-primary"),
            RequestedTarget::new("codex-selection"),
            ResolvedTarget::new("gpt-offline-exact"),
            vec![ConversationMessage::user_text("respond")],
            signalbox_model_runtime::ModelSettings::new(256),
        );

        let prepared = prepared(
            runtime
                .prepare(operation, CancellationSignal::never())
                .await,
        )
        .expect("configured Codex operation prepares through the CLI adapter");
        let mut observations = Observations::default();
        let report = runtime
            .execute(prepared, &mut observations, CancellationSignal::never())
            .await;

        assert_eq!(report.correlation, "codex-route");
        assert_eq!(completion_text(&report.evidence), Some(expected_completion));
        assert_eq!(observations.0.len(), 4);
    }
}
