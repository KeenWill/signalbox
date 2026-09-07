//! Credentialed compatibility smoke for the Responses adapter.
//! The gated OpenAI smoke workflow runs the ignored test; ordinary tests use
//! no credentials. Completion, including an output ceiling, or a decoded
//! refusal must report model identity and usage. Answer quality is not tested.

use signalbox_model_runtime::{
    CancellationSignal, CompletionFinish, ConversationMessage, CredentialAccess,
    CredentialAccessError, CredentialAccessFailure, CredentialReference, CredentialValue,
    DeliveryMode, ModelOperation, ModelRuntime, ModelSettings, PreparationOutcome, RequestedTarget,
    ResolvedTarget, TerminalEvidence,
};
use signalbox_model_runtime_openai::{OpenAiConfig, OpenAiRuntime};
use std::time::Duration;

// The smoke's configured inexpensive target and bounded exchange.
const MODEL: &str = "gpt-5-nano";
const MAX_OUTPUT_TOKENS: u32 = 512;
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(120);

struct EnvironmentCredential;
impl CredentialAccess for EnvironmentCredential {
    async fn resolve(
        &self,
        reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        match std::env::var("OPENAI_API_KEY") {
            Ok(value) if !value.is_empty() => Ok(CredentialValue::new(value.into_bytes())),
            _ => Err(CredentialAccessError::new(
                reference.clone(),
                CredentialAccessFailure::Unavailable,
            )),
        }
    }
}

#[tokio::test]
#[ignore = "spends one real OpenAI exchange; run only from the gated compatibility smoke"]
async fn the_openai_api_completes_one_exchange() {
    let mut config = OpenAiConfig::new(None);
    config.exchange_timeout = Some(EXCHANGE_TIMEOUT);
    let runtime =
        OpenAiRuntime::new(config, EnvironmentCredential).expect("valid smoke configuration");
    let mut operation = ModelOperation::new(
        "openai-smoke".to_string(),
        CredentialReference::new("openai-smoke"),
        RequestedTarget::new(MODEL),
        ResolvedTarget::new(MODEL),
        vec![ConversationMessage::user_text(
            "Reply with the single word: ready",
        )],
        ModelSettings::new(MAX_OUTPUT_TOKENS),
    );
    operation.delivery = DeliveryMode::Streamed;
    let prepared = match runtime
        .prepare(operation, CancellationSignal::never())
        .await
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        _ => panic!("smoke operation did not prepare"),
    };
    let mut observations = Vec::new();
    let report = runtime
        .execute(prepared, &mut observations, CancellationSignal::never())
        .await;
    assert!(
        decoded_response(&report.evidence),
        "Responses compatibility failed: {:?}",
        report.evidence
    );
}

fn decoded_response(evidence: &TerminalEvidence) -> bool {
    let (exchange, model, usage) = match evidence {
        TerminalEvidence::Completed(completion)
            if matches!(
                completion.finish,
                CompletionFinish::EndTurn | CompletionFinish::MaxOutputTokens
            ) && completion.message_id.is_some() =>
        {
            (
                &completion.exchange,
                &completion.reported_model,
                &completion.usage,
            )
        }
        TerminalEvidence::Refused(refusal) => {
            (&refusal.exchange, &refusal.reported_model, &refusal.usage)
        }
        _ => return false,
    };
    exchange.http_status == Some(200)
        && model.is_some()
        && usage.input_tokens.is_some_and(|count| count > 0)
        && usage.output_tokens.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_model_runtime::{
        CompletionEvidence, ExchangeFacts, NativeErrorFacts, ProviderErrorEvidence,
        ProviderErrorKind, ProviderMessageId, ProviderReportedModel, RefusalEvidence, TokenUsage,
    };

    fn completion() -> CompletionEvidence {
        CompletionEvidence {
            exchange: ExchangeFacts {
                http_status: Some(200),
                ..ExchangeFacts::default()
            },
            message_id: Some(ProviderMessageId::new("resp_smoke")),
            reported_model: Some(ProviderReportedModel::new(MODEL)),
            finish: CompletionFinish::EndTurn,
            content: Vec::new(),
            usage: TokenUsage {
                input_tokens: Some(3),
                output_tokens: Some(0),
                ..TokenUsage::unreported()
            },
        }
    }

    #[test]
    fn an_output_ceiling_with_usage_passes_compatibility() {
        let evidence = TerminalEvidence::Completed(CompletionEvidence {
            finish: CompletionFinish::MaxOutputTokens,
            ..completion()
        });
        assert!(decoded_response(&evidence));
    }
    #[test]
    fn a_typed_refusal_with_usage_passes_compatibility() {
        let fixture = completion();
        let evidence = TerminalEvidence::Refused(signalbox_model_runtime::RefusalEvidence {
            reason: signalbox_model_runtime::RefusalReason::ContentPolicy,
            exchange: fixture.exchange,
            message_id: fixture.message_id,
            reported_model: fixture.reported_model,
            content: Vec::new(),
            usage: fixture.usage,
            retained_input_tokens: None,
            retained_output_tokens: None,
        });
        assert!(decoded_response(&evidence));
    }

    #[test]
    fn missing_usage_fails_compatibility() {
        let evidence = TerminalEvidence::Completed(CompletionEvidence {
            usage: TokenUsage::unreported(),
            ..completion()
        });
        assert!(!decoded_response(&evidence));
    }
    #[test]
    fn refusal_with_reported_zero_output_passes_compatibility() {
        let fixture = completion();
        let evidence = TerminalEvidence::Refused(RefusalEvidence {
            reason: signalbox_model_runtime::RefusalReason::Unspecified,
            exchange: fixture.exchange,
            message_id: fixture.message_id,
            reported_model: fixture.reported_model,
            content: Vec::new(),
            usage: fixture.usage,
            retained_input_tokens: None,
            retained_output_tokens: None,
        });
        assert!(decoded_response(&evidence));
    }

    #[test]
    fn native_error_is_not_mistaken_for_a_decoded_refusal() {
        let fixture = completion();
        let evidence = TerminalEvidence::ProviderError(ProviderErrorEvidence {
            exchange: fixture.exchange,
            reported_model: fixture.reported_model,
            usage: fixture.usage,
            kind: ProviderErrorKind::Unrecognized,
            non_acceptance_proven: false,
            native: NativeErrorFacts {
                message: Some("native error".to_string()),
                ..NativeErrorFacts::default()
            },
        });
        assert!(!decoded_response(&evidence));
    }
}
