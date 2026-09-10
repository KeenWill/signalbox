//! Daemon-owned evaluation input composition.

use super::effects::{CatalogBlobs, CorpusBlobs, EvalFailure, stable_digest};
use super::*;
use crate::approval_judge_eval::judge_system_prompt;
use crate::{HubModelConfiguration, ModelAdapter};
use signalbox_model_provider_runtime::approval_judge_output_contract_text;
use std::sync::Arc;

pub(crate) async fn prepare(
    pool: sqlx::PgPool,
    stores: Arc<crate::BlobStoreRegistry>,
    configuration: &HubModelConfiguration,
    input: signalbox_process_protocol::EvaluationInput,
) -> Result<EvalManifest, EvalFailure> {
    let invalid = |message: String| {
        EvalFailure::Rejected(signalbox_workflow_runtime::LiveDeliveryFailure::new(
            message,
        ))
    };
    let blobs = CatalogBlobs {
        repository: signalbox_persistence::blob::BlobCatalogRepository::new(pool),
        stores,
    };
    let recorded_responses = match input.recorded_responses {
        Some(digest) => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Responses {
                responses: Vec<RecordedResponse>,
            }
            let bytes = blobs.read(digest.into_digest()).await?;
            if signalbox_domain::BlobDigest::digest(&bytes) != digest.into_digest() {
                return Err(invalid("recorded-response digest mismatch".into()));
            }
            let responses: Responses =
                serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
            Some(responses.responses)
        }
        None => None,
    };
    let binding = if recorded_responses.is_some() {
        recorded_binding()
    } else {
        configured_binding(configuration).map_err(invalid)?
    };
    let mut manifest = EvalManifest {
        corpus: input.corpus.to_string(),
        format: match input.format {
            signalbox_process_protocol::EvaluationCorpusFormat::Offline => CorpusFormat::Offline,
            signalbox_process_protocol::EvaluationCorpusFormat::Live => CorpusFormat::Live,
        },
        cases: input.cases,
        repeats: input.repeats,
        binding,
        postures: configuration
            .tool_approval_postures()
            .map(|(name, posture)| {
                let value = match posture {
                    signalbox_domain::ToolApprovalPosture::Auto => "auto",
                    signalbox_domain::ToolApprovalPosture::Delegated => "delegated",
                    signalbox_domain::ToolApprovalPosture::Human => "human",
                };
                (name.as_str().into(), value.into())
            })
            .collect(),
        speculative_tools: Vec::new(),
        recorded_responses,
    };
    manifest
        .validate()
        .map_err(|error| invalid(error.to_string()))?;
    let digest = manifest
        .corpus
        .parse()
        .map_err(|error: signalbox_domain::BlobDigestParseError| invalid(error.to_string()))?;
    let bytes = blobs.read(digest).await?;
    let corpus = super::effects::decode_selected(&manifest, &bytes)?;
    let composition = if configuration.daemon_tools().is_some() {
        crate::DaemonToolComposition::WithMappedFamilies
    } else {
        crate::DaemonToolComposition::Base
    };
    manifest.speculative_tools = corpus
        .cases
        .iter()
        .filter_map(|case| match case {
            Case::Live(case) => Some(case.tool.as_str()),
            Case::Offline(_) => None,
        })
        .filter(|tool| {
            manifest.postures.get(*tool).map(String::as_str) != Some("delegated")
                || !signalbox_domain::ToolName::try_new((*tool).into()).is_ok_and(|name| {
                    crate::DaemonToolCatalog::validate_approval_postures_for_composition(
                        [(name, signalbox_domain::ToolApprovalPosture::Auto)],
                        composition,
                    )
                    .is_ok()
                })
        })
        .map(String::from)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(manifest)
}

pub fn configured_binding(configuration: &HubModelConfiguration) -> Result<JudgeBinding, String> {
    let selection = configuration
        .configured_approval_judge_selection()
        .ok_or("configuration has no approval judge selection")?;
    let route = configuration
        .resolve_direct_model(selection)
        .ok_or("approval judge route missing")?;
    let credential_profile = configuration
        .credential_profile(route.credential_profile())
        .ok_or("credential profile missing")?;
    let delivery = credential_profile.delivery();
    let credential_delivery = delivery.key();
    let credential_file = delivery.path();
    let credential_env_key = delivery.env_key();
    let catalog = configuration.runtime_model_catalog();
    let definition = catalog
        .resolve(route.target())
        .ok_or("runtime model definition missing")?;
    let definition_max_output_tokens = definition.max_output_tokens();
    let definition_context_window_tokens = definition.context_window_tokens();
    let operation_contract = format!(
        "{}\u{0}{}\u{0}adapter={:?}\u{0}max_output_tokens={}\u{0}context_window_tokens={}\u{0}credential_profile={}\u{0}credential_delivery={}\u{0}credential_file={}\u{0}credential_env_key={}",
        judge_system_prompt(),
        approval_judge_output_contract_text(),
        route.adapter(),
        definition_max_output_tokens,
        definition_context_window_tokens,
        route.credential_profile(),
        credential_delivery,
        credential_file
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        credential_env_key.unwrap_or_default(),
    );
    let operation_contract = match route.adapter() {
        ModelAdapter::ClaudeCli => {
            let runtime = configuration
                .claude_cli()
                .ok_or_else(|| String::from("Claude CLI route has no runtime configuration"))?;
            format!(
                "{operation_contract}\u{0}cli_executable={}\u{0}cli_mcp_bridge_executable={}\u{0}cli_working_directory={}",
                runtime.executable().display(),
                runtime.mcp_bridge_executable().display(),
                runtime.working_directory().display(),
            )
        }
        ModelAdapter::CodexCli => {
            let runtime = configuration
                .codex_cli()
                .ok_or_else(|| String::from("Codex CLI route has no runtime configuration"))?;
            format!(
                "{operation_contract}\u{0}cli_executable={}\u{0}cli_working_directory={}",
                runtime.executable().display(),
                runtime.working_directory().display(),
            )
        }
        ModelAdapter::Anthropic | ModelAdapter::OpenAi => operation_contract,
    };
    let contract_digest = stable_digest(operation_contract.as_bytes());
    Ok(JudgeBinding {
        selection: selection.into_uuid().to_string(),
        target: route.target().identity().into_uuid().to_string(),
        credential_reference: route.credential_profile().into(),
        provider_model: definition.provider_model().into(),
        contract_digest,
        cache_accounting: if configuration
            .cache_inclusive_input_targets()
            .contains(&route.target())
        {
            "input_includes_cache"
        } else {
            "input_excludes_cache"
        }
        .into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_binding_uses_the_configured_judge_route() {
        let example = crate::configuration::checked_in_example_configuration().unwrap();
        let source = example
            .source()
            .replace("# [approval_judge]", "[approval_judge]")
            .replace(
                "# selection_id = \"c70bd4e1-3699-458c-ae03-310a77763807\"",
                "selection_id = \"c70bd4e1-3699-458c-ae03-310a77763807\"",
            );
        let configuration = HubModelConfiguration::parse(&source).unwrap();
        let binding = configured_binding(&configuration).unwrap();
        // The selected example model differs from the first catalog entry.
        assert_eq!(binding.selection, "c70bd4e1-3699-458c-ae03-310a77763807");
        assert_eq!(binding.target, "c74d2c41-8c70-4537-ab5d-7bbeac25562a");
        assert_eq!(binding.provider_model, "claude-fable-5");
        assert_eq!(binding.credential_reference, "anthropic-primary");
        assert_ne!(binding, recorded_binding());
        assert!(configured_binding(&example).is_err());
    }
}
