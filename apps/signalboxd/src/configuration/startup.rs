//! Shared validation of startup-only configuration before database admission.

use super::*;

pub(super) struct ParsedStartup {
    pub(super) numeric_bounds: NumericBoundsConfiguration,
    pub(super) global_model_settings: ModelSettingsOverlay,
    pub(super) model_settings_profiles: HashMap<Arc<str>, ModelSettingsOverlay>,
    pub(super) compaction_prompt: Arc<str>,
    pub(super) file_media: bool,
    pub(super) blob_storage: Option<BlobStorageConfiguration>,
    pub(super) web_fetch_egress_policy: WebFetchEgressPolicy,
    pub(super) daemon_tools: Option<DaemonToolConfiguration>,
    pub(super) credential_profiles: HashMap<Arc<str>, CredentialProfile>,
    pub(super) credential_pools: HashMap<Arc<str>, CredentialPool>,
    pub(super) tool_approval_postures: BTreeMap<ToolName, ToolApprovalPosture>,
    pub(super) approval_wait_timeout: Option<std::time::Duration>,
    pub(super) approval_judge_selection: Option<DirectModelSelection>,
    pub(super) convergence: Option<signalbox_convergence::ConvergencePolicy>,
    pub(super) workspace_instructions: WorkspaceInstructionConfiguration,
    pub(super) tool_proposal_limits: signalbox_application::ToolProposalLimits,
    pub(super) mappings: HashMap<Arc<str>, AdapterMapping>,
    pub(super) session_credential_pin: SessionCredentialPin,
    pub(super) fallback_credential_profile: Arc<str>,
    pub(super) codex_cli: Option<CodexCliConfiguration>,
    pub(super) codex_cli_credential_profile: Option<Arc<str>>,
    pub(super) claude_cli: Option<ClaudeCliConfiguration>,
    pub(super) claude_cli_credential_profile: Option<Arc<str>>,
}

pub(super) fn parse_startup(
    content: &str,
    document: &DocumentMut,
) -> Result<ParsedStartup, HubModelConfigurationError> {
    reject_unknown_fields(
        document.as_table(),
        &[
            "version",
            "numeric_bounds",
            "credential_profiles",
            "credential_pools",
            "adapter_mappings",
            "claude_cli",
            "codex_cli",
            "model_settings",
            "model_settings_profiles",
            "models",
            "serving_targets",
            "aliases",
            "compaction",
            "web_fetch",
            "tool_mappings",
            "daemon_tools",
            "git_identity",
            "tool_approval_postures",
            "tool_settings",
            "approval_judge",
            "convergence",
            "repository_watch",
            "blob_storage",
            "file_media",
            "workspace_instructions",
            "tool_proposals",
        ],
    )?;
    if document.get("version").and_then(|item| item.as_integer()) != Some(1) {
        return Err(HubModelConfigurationError::UnsupportedVersion);
    }
    let numeric_bounds = NumericBoundsConfiguration::parse(document.get("numeric_bounds"))?;
    let global_model_settings = parse_model_settings_overlay(document.get("model_settings"))?;
    let model_settings_profiles =
        parse_model_settings_profiles(document.get("model_settings_profiles"))?;
    let compaction = document
        .get("compaction")
        .and_then(|item| item.as_table())
        .ok_or(HubModelConfigurationError::MissingCompaction)?;
    reject_unknown_fields(compaction, &["prompt"])?;
    let compaction_prompt = required_string(compaction, "prompt")?;
    if compaction_prompt.is_empty()
        || compaction_prompt.contains('\0')
        || compaction_prompt.len() > MAX_COMPACTION_PROMPT_UTF8_BYTES
    {
        return Err(HubModelConfigurationError::InvalidCompactionPrompt);
    }
    let compaction_prompt: Arc<str> = Arc::from(compaction_prompt);
    let blob_storage = BlobStorageConfiguration::parse(document.get("blob_storage"))
        .map_err(|_| HubModelConfigurationError::InvalidBlobStorageConfiguration)?;
    let file_media = document
        .get("file_media")
        .map(|value| {
            value
                .as_bool()
                .ok_or(HubModelConfigurationError::InvalidDocument)
        })
        .transpose()?
        .unwrap_or(false);
    if file_media && blob_storage.is_none() {
        return Err(HubModelConfigurationError::InvalidBlobStorageConfiguration);
    }
    let web_fetch_egress_policy = document
        .get("web_fetch")
        .map(|item| {
            let table = item
                .as_table()
                .ok_or(HubModelConfigurationError::InvalidWebFetchPolicy)?;
            reject_unknown_fields(table, &["allowed_origins"])
                .map_err(|_| HubModelConfigurationError::InvalidWebFetchPolicy)?;
            let origins = table
                .get("allowed_origins")
                .and_then(|item| item.as_array())
                .ok_or(HubModelConfigurationError::InvalidWebFetchPolicy)?;
            let origins = origins
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or(HubModelConfigurationError::InvalidWebFetchPolicy)
                })
                .collect::<Result<Vec<_>, _>>()?;
            WebFetchEgressPolicy::try_from_allowed_origins(origins)
                .map_err(|_| HubModelConfigurationError::InvalidWebFetchPolicy)
        })
        .transpose()?
        .unwrap_or_default();
    let git_identity = parse_git_identity(document.get("git_identity"))?;
    let exec_supervisor_executable = parse_daemon_tool_settings(document.get("daemon_tools"))?;
    let daemon_tools = parse_tool_mappings(
        document.get("tool_mappings"),
        git_identity,
        exec_supervisor_executable,
    )?;
    let credential_profiles = parse_credential_profiles(document.get("credential_profiles"))?;
    let credential_pools =
        parse_credential_pools(document.get("credential_pools"), &credential_profiles)?;
    let approval_wait_timeout =
        tool_settings::parse_approval_wait_timeout(document.get("tool_settings"))?;
    let tool_approval_postures =
        parse_tool_approval_postures(document.get("tool_approval_postures"))?;
    let tool_composition = match daemon_tools {
        Some(_) => crate::DaemonToolComposition::WithMappedFamilies,
        None => crate::DaemonToolComposition::Base,
    };
    crate::DaemonToolCatalog::validate_approval_postures_for_composition(
        tool_approval_postures
            .iter()
            .map(|(name, posture)| (name.clone(), *posture)),
        tool_composition,
    )
    .map_err(|_| HubModelConfigurationError::InvalidToolApprovalPostures)?;
    let approval_judge_selection = parse_approval_judge(document.get("approval_judge"))?;
    #[derive(serde::Deserialize)]
    struct ConvergenceSection {
        convergence: Option<signalbox_convergence::ConvergencePolicy>,
    }
    let convergence = toml::from_str::<ConvergenceSection>(content)
        .map_err(|_| HubModelConfigurationError::InvalidDocument)?
        .convergence;
    if let Some(policy) = &convergence {
        policy
            .validate()
            .map_err(|_| HubModelConfigurationError::InvalidDocument)?;
    }
    let tool_proposal_limits = parse_tool_proposal_limits(document.get("tool_proposals"))?;
    let workspace_instructions =
        parse_workspace_instruction_configuration(document.get("workspace_instructions"))?;
    let mapping_tables = document
        .get("adapter_mappings")
        .and_then(|item| item.as_array_of_tables())
        .ok_or(HubModelConfigurationError::MissingAdapterMappings)?;
    if mapping_tables.is_empty() {
        return Err(HubModelConfigurationError::MissingAdapterMappings);
    }
    let mut mappings = HashMap::<Arc<str>, AdapterMapping>::new();
    let mut session_credentials = Vec::with_capacity(mapping_tables.len());
    let mut codex_cli_credential_profile = None;
    let mut claude_cli_credential_profile = None;
    for mapping in mapping_tables {
        reject_unknown_fields(mapping, &["model_family", "adapter", "credential_pool"])?;
        let family = validated_name(required_string(mapping, "model_family")?)?;
        let adapter = ModelAdapter::parse(required_string(mapping, "adapter")?)?;
        let credential_pool = validated_name(required_string(mapping, "credential_pool")?)?;
        let Some(pool) = credential_pools.get(&credential_pool) else {
            return Err(HubModelConfigurationError::UnknownCredentialPool {
                model_family: family,
                credential_pool,
            });
        };
        if pool.adapter() != adapter {
            return Err(HubModelConfigurationError::ConflictingPoolAdapters { credential_pool });
        }
        let credential_profile = pool
            .preferred_member()
            .map(|member| Arc::<str>::from(member.profile()))
            .ok_or_else(|| HubModelConfigurationError::EmptyCredentialPool {
                credential_pool: Arc::clone(&credential_pool),
            })?;
        let adapter_profile = match adapter {
            ModelAdapter::CodexCli => &mut codex_cli_credential_profile,
            ModelAdapter::ClaudeCli => &mut claude_cli_credential_profile,
            ModelAdapter::Anthropic | ModelAdapter::OpenAi => {
                // Direct HTTP runtimes resolve the operation's pinned
                // profile from the complete file-access catalog.
                let entry = AdapterMapping {
                    adapter,
                    credential_pool,
                    credential_profile: Arc::clone(&credential_profile),
                };
                if mappings.contains_key(&family) {
                    return Err(HubModelConfigurationError::DuplicateModelFamily {
                        model_family: family,
                    });
                }
                mappings.insert(Arc::clone(&family), entry);
                session_credentials.push(SessionModelCredential::new(family, credential_profile));
                continue;
            }
        };
        // CLI runtimes receive their complete adapter-scoped delivery
        // catalogs. The retained value is only the default for an ambient
        // operation that pins no catalog member.
        adapter_profile.get_or_insert_with(|| Arc::clone(&credential_profile));
        let entry = AdapterMapping {
            adapter,
            credential_pool,
            credential_profile: Arc::clone(&credential_profile),
        };
        if mappings.contains_key(&family) {
            return Err(HubModelConfigurationError::DuplicateModelFamily {
                model_family: family,
            });
        }
        mappings.insert(Arc::clone(&family), entry);
        session_credentials.push(SessionModelCredential::new(family, credential_profile));
    }
    let fallback_credential_profile = session_credentials
        .first()
        .map(|credential| Arc::from(credential.credential_reference()))
        .ok_or(HubModelConfigurationError::InvalidField)?;
    let session_credential_pin = SessionCredentialPin::try_new(session_credentials)
        .map_err(|_| HubModelConfigurationError::InvalidField)?;

    let codex_cli = document
        .get("codex_cli")
        .map(|item| {
            let table = item
                .as_table()
                .ok_or(HubModelConfigurationError::InvalidCodexCliConfiguration)?;
            reject_unknown_fields(
                table,
                &[
                    "executable",
                    "working_directory",
                    "model_context_window_overrides",
                ],
            )?;
            let executable = PathBuf::from(required_string(table, "executable")?);
            let working_directory = PathBuf::from(required_string(table, "working_directory")?);
            let model_context_window_overrides =
                parse_positive_u32_inline_map(table.get("model_context_window_overrides"))?;
            if !executable.is_absolute()
                || !executable.is_file()
                || !working_directory.is_absolute()
                || !working_directory.is_dir()
            {
                return Err(HubModelConfigurationError::InvalidCodexCliConfiguration);
            }
            Ok(CodexCliConfiguration {
                executable,
                working_directory,
                model_context_window_overrides,
            })
        })
        .transpose()?;
    if mappings
        .values()
        .any(|mapping| mapping.adapter == ModelAdapter::CodexCli)
        && codex_cli.is_none()
    {
        return Err(HubModelConfigurationError::MissingCodexCliConfiguration);
    }
    if let Some(configuration) = codex_cli.as_ref() {
        let mut runtime_configuration = CodexCliConfig::new(
            configuration.executable.clone(),
            configuration.working_directory.clone(),
            CredentialReference::new(
                codex_cli_credential_profile
                    .as_deref()
                    .unwrap_or(CODEX_CLI_CREDENTIAL_REFERENCE),
            ),
            None,
        );
        runtime_configuration.model_context_window_overrides =
            configuration.model_context_window_overrides.clone();
        CodexCliRuntime::new(runtime_configuration)
            .map_err(|_| HubModelConfigurationError::InvalidCodexCliConfiguration)?;
    }

    let claude_cli = document
        .get("claude_cli")
        .map(|item| {
            let table = item
                .as_table()
                .ok_or(HubModelConfigurationError::InvalidClaudeCliConfiguration)?;
            reject_unknown_fields(
                table,
                &["executable", "mcp_bridge_executable", "working_directory"],
            )?;
            let executable = PathBuf::from(required_string(table, "executable")?);
            let mcp_bridge_executable = resolved_mcp_bridge_reference(
                required_string(table, "mcp_bridge_executable")?,
                std::env::var_os("PATH").as_deref(),
            )?;
            let working_directory = PathBuf::from(required_string(table, "working_directory")?);
            if !executable.is_absolute()
                || !executable.is_file()
                || !mcp_bridge_executable.is_absolute()
                || !mcp_bridge_executable.is_file()
                || !working_directory.is_absolute()
                || !working_directory.is_dir()
            {
                return Err(HubModelConfigurationError::InvalidClaudeCliConfiguration);
            }
            Ok(ClaudeCliConfiguration {
                executable,
                mcp_bridge_executable,
                working_directory,
            })
        })
        .transpose()?;
    if mappings
        .values()
        .any(|mapping| mapping.adapter == ModelAdapter::ClaudeCli)
        && claude_cli.is_none()
    {
        return Err(HubModelConfigurationError::MissingClaudeCliConfiguration);
    }
    if let Some(configuration) = claude_cli.as_ref() {
        ClaudeCliRuntime::new(ClaudeCliConfig::new(
            configuration.executable.clone(),
            configuration.mcp_bridge_executable.clone(),
            configuration.working_directory.clone(),
            CredentialReference::new(
                claude_cli_credential_profile
                    .as_deref()
                    .unwrap_or(CLAUDE_CLI_CREDENTIAL_REFERENCE),
            ),
            None,
            None,
        ))
        .map_err(|_| HubModelConfigurationError::InvalidClaudeCliConfiguration)?;
    }

    Ok(ParsedStartup {
        numeric_bounds,
        global_model_settings,
        model_settings_profiles,
        compaction_prompt,
        blob_storage,
        file_media,
        web_fetch_egress_policy,
        daemon_tools,
        credential_profiles,
        credential_pools,
        tool_approval_postures,
        approval_wait_timeout,
        approval_judge_selection,
        convergence,
        workspace_instructions,
        tool_proposal_limits,
        mappings,
        session_credential_pin,
        fallback_credential_profile,
        codex_cli,
        codex_cli_credential_profile,
        claude_cli,
        claude_cli_credential_profile,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_validation_rejects_mapped_tool_postures_without_mappings() {
        let models = crate::configuration::checked_in_example_configuration().expect("models");
        let mut document = models.source().parse::<DocumentMut>().expect("document");
        let mut postures = toml_edit::Table::new();
        postures.insert(
            signalbox_tools_exec::SANDBOXED_EXEC_NAME,
            toml_edit::value("delegated"),
        );
        document.insert("tool_approval_postures", toml_edit::Item::Table(postures));
        assert!(HubModelConfiguration::startup_numeric_bounds(&document.to_string()).is_ok());
        document.remove("tool_mappings");
        assert_eq!(
            HubModelConfiguration::startup_numeric_bounds(&document.to_string()).err(),
            Some(HubModelConfigurationError::InvalidToolApprovalPostures)
        );
    }

    #[test]
    fn startup_validation_rejects_unknown_document_fields() {
        let models = crate::configuration::checked_in_example_configuration().expect("models");
        let mut document = models.source().parse::<DocumentMut>().expect("document");
        document.insert("unknown_startup_field", toml_edit::value(true));
        assert!(HubModelConfiguration::startup_numeric_bounds(&document.to_string()).is_err());
    }

    #[test]
    fn startup_validation_checks_sections_beyond_numeric_bounds() {
        let models = crate::configuration::checked_in_example_configuration().expect("models");
        for section in [
            "version",
            "compaction",
            "credential_pools",
            "adapter_mappings",
            "model_settings",
            "claude_cli",
        ] {
            let mut document = models.source().parse::<DocumentMut>().expect("document");
            document.insert(section, toml_edit::value(0));
            assert!(
                HubModelConfiguration::startup_numeric_bounds(&document.to_string()).is_err(),
                "invalid {section}"
            );
        }
    }

    #[test]
    fn startup_validation_defers_reloadable_sections_to_retained_snapshot_selection() {
        let models = crate::configuration::checked_in_example_configuration().expect("models");
        let mut document = models.source().parse::<DocumentMut>().expect("document");
        for section in ["models", "serving_targets", "aliases", "repository_watch"] {
            document.insert(section, toml_edit::value(0));
        }
        assert!(HubModelConfiguration::startup_numeric_bounds(&document.to_string()).is_ok());
        assert!(HubModelConfiguration::parse(&document.to_string()).is_err());
    }
}
