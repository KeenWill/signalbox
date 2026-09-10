use super::*;

#[derive(Debug)]
pub(super) struct ContinuationEntryMeasurement {
    pub(super) adapter: ModelAdapter,
    pub(super) models: RuntimeModelCatalog,
    pub(super) replays_provider_compaction: bool,
}

impl signalbox_persistence::model_execution::ToolContinuationEntryMeasurement
    for ContinuationEntryMeasurement
{
    fn additional_entry_bytes(
        &self,
        operation: &signalbox_application::PreparedModelOperation,
    ) -> Option<u64> {
        let messages = operation
            .messages()
            .iter()
            .filter(|message| {
                !matches!(
                    message,
                    signalbox_application::ModelConversationMessage::ContextSummary { .. }
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        let entries = signalbox_model_provider_runtime::rendered_entry_bytes(
            &messages,
            operation.reasoning_provenance(),
            &self.models,
            |message| match self.adapter {
                ModelAdapter::Anthropic => {
                    signalbox_model_runtime_anthropic::serialized_message_bytes(
                        message,
                        self.replays_provider_compaction,
                    )
                }
                ModelAdapter::OpenAi => {
                    signalbox_model_runtime_openai::serialized_message_bytes(message)
                }
                ModelAdapter::CodexCli => {
                    signalbox_model_runtime_codex_cli::serialized_message_bytes(message)
                }
                ModelAdapter::ClaudeCli => {
                    signalbox_model_runtime_claude_cli::serialized_message_bytes(message)
                }
            },
        )?;
        Some(
            entries
                .values()
                .fold(0_u64, |total, bytes| total.saturating_add(*bytes)),
        )
    }
}

impl HubModelConfiguration {
    pub(crate) fn compaction_request_bytes(
        &self,
        selection: DirectModelSelection,
        source: &str,
    ) -> Option<u64> {
        let route = self.resolve_direct_model(selection)?;
        let definition = self.runtime_models.resolve(route.target)?;
        let request = signalbox_model_provider_runtime::ContextCompactionModelRequest {
            call: signalbox_domain::ModelCallId::from_uuid(uuid::Uuid::nil()),
            session: signalbox_domain::SessionId::from_uuid(uuid::Uuid::nil()),
            selection,
            target: route.target,
            credential_reference: "compaction-measurement".to_owned(),
            system_prompt: self.compaction_prompt.to_string(),
            rendered_range: source.to_owned(),
        };
        let operation = request.into_operation(definition);
        let bytes = match route.adapter() {
            ModelAdapter::Anthropic => {
                signalbox_model_runtime_anthropic::serialized_request_bytes(&operation)
            }
            ModelAdapter::OpenAi => {
                signalbox_model_runtime_openai::serialized_request_bytes(&operation)
            }
            ModelAdapter::CodexCli => {
                signalbox_model_runtime_codex_cli::serialized_request_bytes(&operation)
            }
            ModelAdapter::ClaudeCli => {
                signalbox_model_runtime_claude_cli::serialized_request_bytes(&operation)
            }
        }?;
        u64::try_from(bytes).ok()
    }
}
