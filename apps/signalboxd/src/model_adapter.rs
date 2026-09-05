//! Exact static model-to-adapter runtime composition.

use std::{collections::HashMap, sync::Arc, time::Duration};

use signalbox_model_runtime::{
    CancellationSignal, InputTokenCountOutcome, MessagePart, ModelInputTokenCounter,
    ModelOperation, ModelRuntime, ObservationSink, PreparationDefect, PreparationOutcome,
    TerminalReport,
};
use signalbox_model_runtime_claude_cli::{
    ClaudeCliConstructionError, ClaudeCliPreparedRequest, ClaudeCliRuntime,
};
use signalbox_model_runtime_codex_cli::{
    CodexCliConstructionError, CodexCliPreparedRequest, CodexCliRuntime,
};

use crate::configuration::{HubModelConfiguration, ModelAdapter};

/// One prepared capability tagged by the adapter that created it.
///
/// Each direct HTTP adapter contributes its own runtime and prepared-capability
/// parameters, so a test composition can script either one independently of the
/// other exactly as it already could for Anthropic alone.
pub enum ConfiguredPreparedRequest<C, A, P, O, Q> {
    /// Anthropic HTTP request capability.
    Anthropic { runtime: Arc<A>, prepared: P },
    /// OpenAI HTTP request capability.
    OpenAi { runtime: Arc<O>, prepared: Q },
    /// Claude Code CLI process request capability.
    ClaudeCli {
        runtime: Arc<ClaudeCliRuntime>,
        prepared: Box<ClaudeCliPreparedRequest<C>>,
    },
    /// Codex CLI process request capability.
    CodexCli {
        runtime: Arc<CodexCliRuntime>,
        prepared: Box<CodexCliPreparedRequest<C>>,
    },
}

impl<C, A, O> ModelInputTokenCounter<C> for ConfiguredModelRuntime<A, O>
where
    C: Clone + Send + Sync,
    A: ModelInputTokenCounter<C> + Send + Sync,
    O: Send + Sync,
{
    async fn count_input_tokens(
        &self,
        mut operation: ModelOperation<C>,
        cancellation: CancellationSignal,
    ) -> InputTokenCountOutcome<C> {
        let provider_model = operation.resolved_target.as_str();
        if self.routes.get(provider_model) != Some(&ModelAdapter::Anthropic) {
            return InputTokenCountOutcome::Unavailable {
                correlation: operation.correlation,
            };
        }
        omit_unreplayable_provider_compaction(&mut operation, ModelAdapter::Anthropic);
        let Some(runtime) = self.anthropic.as_ref() else {
            return InputTokenCountOutcome::Failed {
                correlation: operation.correlation,
            };
        };
        runtime.count_input_tokens(operation, cancellation).await
    }
}

/// Why one configured CLI adapter could not be constructed at startup.
///
/// Each variant keeps its own adapter's typed construction evidence so the
/// composition root can name the exact failing adapter without inspecting an
/// adapter-owned string.
#[derive(Debug)]
pub enum ConfiguredAdapterConstructionError {
    /// The configured Claude Code CLI adapter could not be constructed.
    ClaudeCli(ClaudeCliConstructionError),
    /// The configured Codex CLI adapter could not be constructed.
    CodexCli(CodexCliConstructionError),
}

impl ConfiguredAdapterConstructionError {
    /// Returns the closed operator cause token for this construction failure.
    pub const fn cause_code(&self) -> &'static str {
        match self {
            Self::ClaudeCli(_) => "claude_cli_construction_failed",
            Self::CodexCli(_) => "codex_cli_construction_failed",
        }
    }
}

impl std::fmt::Display for ConfiguredAdapterConstructionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ClaudeCli(_) => "configured Claude CLI adapter could not be constructed",
            Self::CodexCli(_) => "configured Codex CLI adapter could not be constructed",
        })
    }
}

impl std::error::Error for ConfiguredAdapterConstructionError {}

/// Runtime router whose exact routes come only from startup configuration.
pub struct ConfiguredModelRuntime<A, O> {
    anthropic: Option<Arc<A>>,
    openai: Option<Arc<O>>,
    claude_cli: Option<Arc<ClaudeCliRuntime>>,
    codex_cli: Option<Arc<CodexCliRuntime>>,
    routes: HashMap<String, ModelAdapter>,
}

impl<A, O> Clone for ConfiguredModelRuntime<A, O> {
    fn clone(&self) -> Self {
        Self {
            anthropic: self.anthropic.clone(),
            openai: self.openai.clone(),
            claude_cli: self.claude_cli.clone(),
            codex_cli: self.codex_cli.clone(),
            routes: self.routes.clone(),
        }
    }
}

impl<A, O> ConfiguredModelRuntime<A, O> {
    /// Constructs every configured adapter without provider interaction.
    pub fn new(
        anthropic: Option<A>,
        openai: Option<O>,
        configuration: &HubModelConfiguration,
        model_exchange_timeout: Option<Duration>,
        post_kill_reap_bound: Option<Duration>,
        native_message_limit: Option<usize>,
    ) -> Result<Self, ConfiguredAdapterConstructionError> {
        Ok(Self {
            anthropic: anthropic.map(Arc::new),
            openai: openai.map(Arc::new),
            claude_cli: configuration
                .claude_cli_runtime(
                    model_exchange_timeout,
                    post_kill_reap_bound,
                    native_message_limit,
                )
                .map_err(ConfiguredAdapterConstructionError::ClaudeCli)?
                .map(Arc::new),
            codex_cli: configuration
                .codex_cli_runtime(model_exchange_timeout, post_kill_reap_bound)
                .map_err(ConfiguredAdapterConstructionError::CodexCli)?
                .map(Arc::new),
            routes: configuration.adapter_routes(),
        })
    }
}

impl<A, O> std::fmt::Debug for ConfiguredModelRuntime<A, O> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfiguredModelRuntime")
            .field(
                "anthropic",
                &self.anthropic.as_ref().map(|_| "[model adapter]"),
            )
            .field("openai", &self.openai.as_ref().map(|_| "[model adapter]"))
            .field(
                "claude_cli",
                &self.claude_cli.as_ref().map(|_| "[model adapter]"),
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

fn omit_unreplayable_provider_compaction<C>(
    operation: &mut ModelOperation<C>,
    adapter: ModelAdapter,
) {
    let can_replay = adapter == ModelAdapter::Anthropic
        && signalbox_model_runtime_anthropic::server_compaction_supported(
            operation.resolved_target.as_str(),
        );
    if can_replay {
        return;
    }
    operation.messages.retain_mut(|message| {
        let carried_compaction = message
            .parts
            .iter()
            .any(|part| matches!(part, MessagePart::ProviderCompaction { .. }));
        if carried_compaction {
            message
                .parts
                .retain(|part| !matches!(part, MessagePart::ProviderCompaction { .. }));
        }
        !carried_compaction || !message.parts.is_empty()
    });
}

impl<C, A, O> ModelRuntime<C> for ConfiguredModelRuntime<A, O>
where
    C: Clone + Send + Sync,
    A: ModelRuntime<C> + Send + Sync,
    A::Prepared: Send,
    O: ModelRuntime<C> + Send + Sync,
    O::Prepared: Send,
{
    type Prepared = ConfiguredPreparedRequest<C, A, A::Prepared, O, O::Prepared>;

    async fn prepare(
        &self,
        mut operation: ModelOperation<C>,
        cancellation: CancellationSignal,
    ) -> PreparationOutcome<C, Self::Prepared> {
        let adapter = self.routes.get(operation.resolved_target.as_str()).copied();
        if let Some(adapter) = adapter {
            omit_unreplayable_provider_compaction(&mut operation, adapter);
        }
        match adapter {
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
            Some(ModelAdapter::OpenAi) => {
                let runtime = match self.openai.as_ref() {
                    Some(runtime) => Arc::clone(runtime),
                    None => {
                        return PreparationOutcome::Defect {
                            correlation: operation.correlation,
                            defect: PreparationDefect::RequestConstructionFailed {
                                detail: String::from("configured OpenAI adapter is unavailable"),
                            },
                        };
                    }
                };
                let prepared = runtime.prepare(operation, cancellation).await;
                map_preparation(prepared, |prepared| ConfiguredPreparedRequest::OpenAi {
                    runtime,
                    prepared,
                })
            }
            Some(ModelAdapter::ClaudeCli) => match self.claude_cli.as_ref() {
                Some(runtime) => {
                    let prepared = runtime.prepare(operation, cancellation).await;
                    let runtime = Arc::clone(runtime);
                    map_preparation(prepared, |prepared| ConfiguredPreparedRequest::ClaudeCli {
                        runtime,
                        prepared: Box::new(prepared),
                    })
                }
                None => PreparationOutcome::Defect {
                    correlation: operation.correlation,
                    defect: PreparationDefect::RequestConstructionFailed {
                        detail: String::from("configured Claude CLI adapter is unavailable"),
                    },
                },
            },
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
            ConfiguredPreparedRequest::OpenAi { runtime, prepared } => {
                runtime.execute(prepared, sink, cancellation).await
            }
            ConfiguredPreparedRequest::ClaudeCli { runtime, prepared } => {
                runtime.execute(*prepared, sink, cancellation).await
            }
            ConfiguredPreparedRequest::CodexCli { runtime, prepared } => {
                runtime.execute(*prepared, sink, cancellation).await
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        collections::HashMap,
        os::unix::fs::PermissionsExt,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use signalbox_model_runtime::{
        AnthropicServiceTier, AssistantPart, CancellationSignal, CodexCliServiceTier,
        CompletionEvidence, CompletionFinish, ConversationMessage, CredentialReference,
        ExchangeFacts, FastMode, InputTokenCountOutcome, MessagePart, ModelInputTokenCounter,
        ModelOperation, ModelRuntime, ModelSettings, Observation, ObservationSink,
        OpenAiServiceTier, PreparationDefect, PreparationOutcome, ProviderReportedModel,
        ReasoningLevel, RequestedTarget, ResolvedTarget, Script, ScriptedModel, ServiceTier,
        TerminalEvidence, TokenUsage,
    };
    use signalbox_model_runtime_claude_cli::SUPPORTED_CLAUDE_CLI_VERSION;

    use crate::configuration::{HubModelConfiguration, ModelAdapter};

    use super::{ConfiguredModelRuntime, omit_unreplayable_provider_compaction};

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
                | Some(signalbox_model_runtime::AssistantPart::ProviderCompaction { .. })
                | Some(signalbox_model_runtime::AssistantPart::ToolCall(_))
                | Some(signalbox_model_runtime::AssistantPart::SuppressedToolCall(_))
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

    fn operation_with_provider_compaction(provider_model: &str) -> ModelOperation<String> {
        ModelOperation::new(
            String::from("cross-provider"),
            CredentialReference::new("credential"),
            RequestedTarget::new("selection"),
            ResolvedTarget::new(provider_model),
            vec![
                ConversationMessage {
                    role: signalbox_model_runtime::ConversationRole::Assistant,
                    parts: vec![
                        MessagePart::Text(String::from("preserved output")),
                        MessagePart::ProviderCompaction {
                            block_json: String::from(
                                r#"{"type":"compaction","content":"summary"}"#,
                            ),
                        },
                    ],
                },
                ConversationMessage {
                    role: signalbox_model_runtime::ConversationRole::Assistant,
                    parts: vec![MessagePart::ProviderCompaction {
                        block_json: String::from(r#"{"type":"compaction","content":"summary"}"#),
                    }],
                },
            ],
            ModelSettings::new(256),
        )
    }

    #[test]
    fn cross_provider_projection_omits_only_opaque_compaction_parts() {
        let mut operation = operation_with_provider_compaction("gpt-exact");

        omit_unreplayable_provider_compaction(&mut operation, ModelAdapter::OpenAi);

        assert_eq!(operation.messages.len(), 1);
        assert_eq!(
            operation.messages[0].parts,
            vec![MessagePart::Text(String::from("preserved output"))]
        );
    }

    #[test]
    fn unsupported_anthropic_projection_omits_opaque_compaction_parts() {
        let mut operation = operation_with_provider_compaction("claude-haiku-4-5");

        omit_unreplayable_provider_compaction(&mut operation, ModelAdapter::Anthropic);

        assert_eq!(operation.messages.len(), 1);
        assert_eq!(
            operation.messages[0].parts,
            vec![MessagePart::Text(String::from("preserved output"))]
        );
    }

    #[test]
    fn supported_anthropic_projection_retains_opaque_compaction_parts() {
        let mut operation = operation_with_provider_compaction("claude-fable-5-1");
        let expected = operation.messages.clone();

        omit_unreplayable_provider_compaction(&mut operation, ModelAdapter::Anthropic);

        assert_eq!(operation.messages, expected);
    }

    #[test]
    fn alternate_fast_target_projects_as_declared_runtime_lineage() {
        let standard_model = "claude-standard-fixture";
        let fast_model = "claude-fast-fixture";
        let configuration = HubModelConfiguration::parse_test_fixture(&format!(
            r#"
version = 1

[[credential_profiles]]
name = "anthropic-primary"
adapter = "anthropic"
billing_kind = "api_metered"
delivery = "file"
file = "/run/secrets/anthropic-primary"

[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{{ profile = "anthropic-primary", priority = 1 }}]

[[adapter_mappings]]
model_family = "anthropic"
adapter = "anthropic"
credential_pool = "anthropic-main"

[compaction]
prompt = "Summarize."

[[models]]
selection_id = "10000000-0000-4000-8000-000000000001"
target_id = "20000000-0000-4000-8000-000000000001"
model_family = "anthropic"
provider_model = "{standard_model}"
max_output_tokens = 256
context_window_tokens = 200000
fast_mode = "alternate_target"
fast_target_id = "20000000-0000-4000-8000-000000000002"
reasoning_levels = ["high"]
service_tiers = ["standard_only"]

[[serving_targets]]
target_id = "20000000-0000-4000-8000-000000000002"
model_family = "anthropic"
provider_model = "{fast_model}"
max_output_tokens = 256
context_window_tokens = 200000
"#,
        ))
        .expect("the declared alternate target is valid");
        assert_eq!(configuration.model_capability_catalog().iter().count(), 1);
        let catalog = configuration.runtime_model_capability_catalog();
        let selected = ResolvedTarget::new(standard_model);
        let expected = ResolvedTarget::new(fast_model);
        let mut settings = ModelSettings::new(256);
        settings.reasoning_level = Some(ReasoningLevel::High);
        settings.fast_mode = FastMode::Enabled;
        settings.service_tier = Some(ServiceTier::Anthropic(AnthropicServiceTier::StandardOnly));
        let capabilities = catalog
            .validate(&selected, &settings)
            .expect("the selected target declares fast mode");

        assert_eq!(
            capabilities.effective_target(&selected, settings.fast_mode),
            Ok((&expected, FastMode::Disabled))
        );
    }

    #[tokio::test]
    async fn anthropic_mapping_executes_the_existing_runtime_path_unchanged() {
        let expected_completion = "unchanged Anthropic";
        let configuration = HubModelConfiguration::parse_test_fixture(
            r#"
version = 1

[[credential_profiles]]
name = "anthropic-primary"
adapter = "anthropic"
billing_kind = "api_metered"
delivery = "file"
file = "/run/secrets/anthropic-primary"

[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]


[[adapter_mappings]]
model_family = "anthropic"
adapter = "anthropic"
credential_pool = "anthropic-main"

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
        let runtime = ConfiguredModelRuntime::new(
            Some(anthropic),
            None::<ScriptedModel<String>>,
            &configuration,
            None,
            None,
            None,
        )
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

    const OPENAI_CONFIGURATION: &str = r#"
version = 1

[[credential_profiles]]
name = "anthropic-primary"
adapter = "anthropic"
billing_kind = "api_metered"
delivery = "file"
file = "/run/secrets/anthropic-primary"

[[credential_profiles]]
name = "openai-primary"
adapter = "openai"
billing_kind = "api_metered"
delivery = "file"
file = "/run/secrets/openai-primary"

[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]

[[credential_pools]]
name = "openai-main"
tie_break = "first_listed"
on_pool_exhausted = "fail"
members = [{ profile = "openai-primary", priority = 1 }]

[[adapter_mappings]]
model_family = "anthropic"
adapter = "anthropic"
credential_pool = "anthropic-main"

[[adapter_mappings]]
model_family = "openai"
adapter = "openai"
credential_pool = "openai-main"

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
selection_id = "10000000-0000-4000-8000-000000000004"
target_id = "20000000-0000-4000-8000-000000000004"
model_family = "openai"
provider_model = "gpt-example"
max_output_tokens = 256
context_window_tokens = 200000
reasoning_levels = ["medium"]
fast_mode = "request_control"
service_tiers = ["priority"]
"#;

    fn openai_operation() -> ModelOperation<String> {
        let mut settings = ModelSettings::new(256);
        settings.reasoning_level = Some(ReasoningLevel::Medium);
        settings.service_tier = Some(ServiceTier::OpenAi(OpenAiServiceTier::Priority));
        ModelOperation::new(
            String::from("openai-route"),
            CredentialReference::new("openai-primary"),
            RequestedTarget::new("openai-selection"),
            ResolvedTarget::new("gpt-example"),
            vec![ConversationMessage::user_text("respond")],
            settings,
        )
    }

    #[derive(Clone)]
    struct RecordingInputCounter {
        invocations: Arc<AtomicUsize>,
    }

    impl ModelInputTokenCounter<String> for RecordingInputCounter {
        async fn count_input_tokens(
            &self,
            operation: ModelOperation<String>,
            _cancellation: CancellationSignal,
        ) -> InputTokenCountOutcome<String> {
            self.invocations.fetch_add(1, Ordering::SeqCst);
            InputTokenCountOutcome::Counted {
                correlation: operation.correlation,
                input_tokens: 17,
            }
        }
    }

    #[tokio::test]
    async fn provider_input_estimate_routes_only_anthropic_targets() {
        let invocations = Arc::new(AtomicUsize::new(0));
        let runtime = ConfiguredModelRuntime {
            anthropic: Some(Arc::new(RecordingInputCounter {
                invocations: Arc::clone(&invocations),
            })),
            openai: None::<Arc<()>>,
            claude_cli: None,
            codex_cli: None,
            routes: HashMap::from([
                (String::from("claude-example"), ModelAdapter::Anthropic),
                (String::from("gpt-example"), ModelAdapter::OpenAi),
            ]),
        };
        let mut anthropic = openai_operation();
        anthropic.correlation = String::from("anthropic-count");
        anthropic.resolved_target = ResolvedTarget::new("claude-example");
        let anthropic_outcome = runtime
            .count_input_tokens(anthropic, CancellationSignal::never())
            .await;
        let openai_outcome = runtime
            .count_input_tokens(openai_operation(), CancellationSignal::never())
            .await;

        assert_eq!(
            anthropic_outcome,
            InputTokenCountOutcome::Counted {
                correlation: String::from("anthropic-count"),
                input_tokens: 17,
            }
        );
        assert_eq!(
            openai_outcome,
            InputTokenCountOutcome::Unavailable {
                correlation: String::from("openai-route"),
            }
        );
        assert_eq!(invocations.load(Ordering::SeqCst), 1);
    }

    /// The OpenAI slot mirrors the Anthropic one: a configured route reaches
    /// its own injected runtime and nothing else.
    #[tokio::test]
    async fn openai_mapping_executes_through_its_own_configured_runtime() {
        let expected_completion = "routed through OpenAI";
        let configuration = HubModelConfiguration::parse_test_fixture(OPENAI_CONFIGURATION)
            .expect("the OpenAI mapping and model are valid");
        let openai = ScriptedModel::single(Script::delivering(scripted_completion(
            expected_completion,
            "gpt-example",
        )));
        let received = openai.clone();
        let anthropic = ScriptedModel::<String>::single(Script::delivering(scripted_completion(
            "never routed",
            "claude-example",
        )));
        let unrouted = anthropic.clone();
        let runtime = ConfiguredModelRuntime::new(
            Some(anthropic),
            Some(openai),
            &configuration,
            None,
            None,
            None,
        )
        .expect("configured adapters construct");

        let prepared = prepared(
            runtime
                .prepare(openai_operation(), CancellationSignal::never())
                .await,
        )
        .expect("configured OpenAI operation prepares through its own adapter");
        let mut observations = Observations::default();
        let report = runtime
            .execute(prepared, &mut observations, CancellationSignal::never())
            .await;

        assert_eq!(report.correlation, "openai-route");
        assert_eq!(completion_text(&report.evidence), Some(expected_completion));
        assert_eq!(received.received_operations().len(), 1);
        assert!(unrouted.received_operations().is_empty());
    }

    /// A configured OpenAI route with no constructed adapter is a composition
    /// defect, never a silent fallback onto another provider.
    #[tokio::test]
    async fn openai_route_without_its_adapter_is_a_composition_defect() {
        let configuration = HubModelConfiguration::parse_test_fixture(OPENAI_CONFIGURATION)
            .expect("the OpenAI mapping and model are valid");
        let runtime = ConfiguredModelRuntime::new(
            None::<ScriptedModel<String>>,
            None::<ScriptedModel<String>>,
            &configuration,
            None,
            None,
            None,
        )
        .expect("configured adapters construct");

        let outcome = runtime
            .prepare(openai_operation(), CancellationSignal::never())
            .await;

        assert!(matches!(
            outcome,
            PreparationOutcome::Defect {
                defect: PreparationDefect::RequestConstructionFailed { ref detail },
                ..
            } if detail == "configured OpenAI adapter is unavailable"
        ));
    }

    #[tokio::test]
    async fn configured_claude_model_runs_through_the_cli_fake_transport() {
        let temporary = tempfile::tempdir().expect("temporary working directory is available");
        let executable = temporary.path().join("fake-claude");
        let bridge = temporary.path().join("fake-claude-mcp-bridge");
        let credential_file = temporary.path().join("claude-api-primary");
        let expected_completion = "routed through Claude";
        let provider_model = "claude-cli-offline-exact";
        let file_credential = "synthetic-claude-file-value";
        std::fs::write(&credential_file, file_credential)
            .expect("the Claude credential fixture is writable");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
test -z "${ANTHROPIC_API_KEY+x}" || exit 41
grep -q '"apiKeyHelper"' "${CLAUDE_CONFIG_DIR}/settings.json" || exit 42
test "$(cat "${CLAUDE_CONFIG_DIR}/credential")" = '<file-credential>' || exit 43
cat >/dev/null
printf '%s\n' '{"type":"system","subtype":"init","session_id":"019c0000-0000-7000-8000-000000000002","tools":[],"mcp_servers":[{"name":"signalbox_tools","status":"connected"}],"model":"<provider-model>","slash_commands":[],"skills":[],"plugins":[],"claude_code_version":"<claude-cli-version>"}'
printf '%s\n' '{"type":"assistant","parent_tool_use_id":null,"message":{"model":"<provider-model>","id":"message-1","role":"assistant","content":[{"type":"text","text":"<completion-text>"}],"usage":{"input_tokens":8,"output_tokens":4}}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"session_id":"019c0000-0000-7000-8000-000000000002","stop_reason":"end_turn","terminal_reason":"completed","result":"<completion-text>","errors":[],"usage":{"input_tokens":8,"output_tokens":4,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}'
"#
            .replace("<completion-text>", expected_completion)
            .replace("<file-credential>", file_credential)
            .replace("<provider-model>", provider_model)
            .replace("<claude-cli-version>", SUPPORTED_CLAUDE_CLI_VERSION),
        )
        .expect("fake Claude executable is writable");
        let mut permissions = std::fs::metadata(&executable)
            .expect("fake executable metadata is available")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions)
            .expect("fake Claude executable permissions are set");
        std::fs::write(&bridge, "#!/bin/sh\nexit 0\n").expect("fake MCP bridge is writable");
        let configuration = HubModelConfiguration::parse_test_fixture(&format!(
            r#"
version = 1

[[credential_profiles]]
name = "anthropic-primary"
adapter = "anthropic"
billing_kind = "api_metered"
delivery = "file"
file = "/run/secrets/anthropic-primary"

[[credential_profiles]]
name = "claude-api-primary"
adapter = "claude_cli"
billing_kind = "api_metered"
delivery = "file"
file = "{}"
env_key = "ANTHROPIC_API_KEY"

[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{{ profile = "anthropic-primary", priority = 1 }}]

[[credential_pools]]
name = "claude-main"
tie_break = "first_listed"
on_pool_exhausted = "fail"
members = [{{ profile = "claude-api-primary", priority = 1 }}]

[[adapter_mappings]]
model_family = "anthropic"
adapter = "anthropic"
credential_pool = "anthropic-main"

[[adapter_mappings]]
model_family = "claude_code"
adapter = "claude_cli"
credential_pool = "claude-main"

[claude_cli]
executable = "{}"
mcp_bridge_executable = "{}"
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
selection_id = "10000000-0000-4000-8000-000000000003"
target_id = "20000000-0000-4000-8000-000000000003"
model_family = "claude_code"
provider_model = "{provider_model}"
max_output_tokens = 256
context_window_tokens = 200000
reasoning_levels = ["high"]
fast_mode = "request_control"
"#,
            credential_file.display(),
            executable.display(),
            bridge.display(),
            temporary.path().display(),
        ))
        .expect("Claude mapping and process paths are valid");
        let runtime = ConfiguredModelRuntime::new(
            None::<ScriptedModel<String>>,
            None::<ScriptedModel<String>>,
            &configuration,
            None,
            None,
            None,
        )
        .expect("configured adapters construct");
        assert!(format!("{runtime:?}").contains("claude_cli: Some"));
        let mut settings = ModelSettings::new(256);
        settings.reasoning_level = Some(ReasoningLevel::High);
        settings.fast_mode = FastMode::Enabled;
        let operation = ModelOperation::new(
            String::from("claude-route"),
            CredentialReference::new("claude-api-primary"),
            RequestedTarget::new("claude-cli-selection"),
            ResolvedTarget::new(provider_model),
            vec![ConversationMessage::user_text("respond")],
            settings,
        );

        let prepared = prepared(
            runtime
                .prepare(operation, CancellationSignal::never())
                .await,
        )
        .expect("configured Claude operation prepares through the CLI adapter");
        let mut observations = Observations::default();
        let report = runtime
            .execute(prepared, &mut observations, CancellationSignal::never())
            .await;

        assert_eq!(report.correlation, "claude-route");
        assert_eq!(completion_text(&report.evidence), Some(expected_completion));
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
        let configuration = HubModelConfiguration::parse_test_fixture(&format!(
            r#"
version = 1

[[credential_profiles]]
name = "anthropic-primary"
adapter = "anthropic"
billing_kind = "api_metered"
delivery = "file"
file = "/run/secrets/anthropic-primary"

[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{{ profile = "anthropic-primary", priority = 1 }}]


[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient"

[[credential_pools]]
name = "codex-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{{ profile = "codex-subscription-primary", priority = 1 }}]


[[adapter_mappings]]
model_family = "anthropic"
adapter = "anthropic"
credential_pool = "anthropic-main"

[[adapter_mappings]]
model_family = "codex"
adapter = "codex_cli"
credential_pool = "codex-main"

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
reasoning_levels = ["low"]
fast_mode = "request_control"
service_tiers = ["priority"]
"#,
            executable.display(),
            temporary.path().display(),
        ))
        .expect("Codex mapping and process paths are valid");
        let runtime = ConfiguredModelRuntime::new(
            None::<ScriptedModel<String>>,
            None::<ScriptedModel<String>>,
            &configuration,
            None,
            None,
            None,
        )
        .expect("configured adapters construct");
        assert!(format!("{runtime:?}").contains("anthropic: None"));
        let mut settings = ModelSettings::new(256);
        settings.reasoning_level = Some(ReasoningLevel::Low);
        settings.fast_mode = FastMode::Enabled;
        settings.service_tier = Some(ServiceTier::CodexCli(CodexCliServiceTier::Priority));
        let operation = ModelOperation::new(
            String::from("codex-route"),
            CredentialReference::new("codex-subscription-primary"),
            RequestedTarget::new("codex-selection"),
            ResolvedTarget::new("gpt-offline-exact"),
            vec![ConversationMessage::user_text("respond")],
            settings,
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
