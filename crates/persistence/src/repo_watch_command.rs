//! Core-owned exact payload encoding for repository-watch command recovery.

use crate::mapping::{model_settings_from_json, model_settings_to_json};
use serde_json::{Value, json};
use signalbox_domain::{
    CreateSession, DangerousToolAutoApproval, DescendantTerminationScope, DirectModelSelection,
    DurableCommandId, FinishCondition, ModelAlias, ModelSelectionRequest, ModuleDispatch,
    RepoWatchDispatchId, SessionConfigurationDefaults, SessionCreationCause,
    SessionCreationProvenance, SessionId, SessionLifecycleCommand, SessionLifecycleOperation,
    SessionOwnership, SessionPlacement, SessionSystemPrompt, SessionTemplateContentDigest,
    SessionTemplateName, SessionTemplateProvenance, StartGate, StopStickiness, TranscriptAncestry,
};
use uuid::Uuid;

/// The command forms repository watch retains, encoded without module database access.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchCommandRecord {
    /// Held template creation with copied defaults and repository-watch provenance.
    Create(Box<CreateSession>),
    /// A start release or parent-only sticky stop.
    Lifecycle(SessionLifecycleCommand),
}

impl RepoWatchCommandRecord {
    /// Encodes the complete checked payload; unsupported forms are not representable here.
    pub fn encode(&self) -> Option<Vec<u8>> {
        let value = match self {
            Self::Create(command) => {
                let SessionCreationCause::ModuleDispatched {
                    dispatch: ModuleDispatch::RepositoryWatch { dispatch },
                } = command.provenance().cause()
                else {
                    return None;
                };
                if command.provenance().ancestry() != TranscriptAncestry::None
                    || command.placement() != &SessionPlacement::pathless()
                    || command.start_gate() != StartGate::Held
                    || command.ownership() != SessionOwnership::Owned
                    || command.finish_condition() != Some(&FinishCondition::ExternalGate)
                {
                    return None;
                }
                let template = command.template_provenance()?;
                let defaults = command.initial_configuration_defaults();
                let (kind, selection) = match defaults.model() {
                    ModelSelectionRequest::Direct(id) => ("direct", id.into_uuid()),
                    ModelSelectionRequest::Alias(id) => ("alias", id.into_uuid()),
                };
                json!({"kind":"create", "command":command.command_id().into_uuid().to_string(), "dispatch":dispatch.into_uuid().to_string(),
                    "template":template.name().as_str(), "digest":template.content_digest().as_bytes(),
                    "model_kind":kind, "model":selection.to_string(),
                    "auto_approval": matches!(defaults.dangerous_tool_auto_approval(), DangerousToolAutoApproval::ApproveAll),
                    "system_prompt":defaults.system_prompt().map(SessionSystemPrompt::as_str),
                    "settings":model_settings_to_json(defaults.model_settings())})
            }
            Self::Lifecycle(command) => {
                let operation = match command.operation() {
                    SessionLifecycleOperation::ReleaseStart => "release_start",
                    SessionLifecycleOperation::Stop {
                        sticky: StopStickiness::Sticky,
                        descendant_scope: DescendantTerminationScope::ParentAlone,
                    } => "sticky_stop",
                    _ => return None,
                };
                json!({"kind":"lifecycle", "command":command.command_id().into_uuid().to_string(), "session":command.session().into_uuid().to_string(), "operation":operation})
            }
        };
        serde_json::to_vec(&value).ok()
    }

    /// Reconstitutes solely from retained bytes, without resolving current configuration.
    pub fn decode(payload: &[u8]) -> Option<Self> {
        let v: Value = serde_json::from_slice(payload).ok()?;
        let command = DurableCommandId::from_uuid(id(&v["command"])?);
        match v["kind"].as_str()? {
            "create" => {
                let selection = id(&v["model"])?;
                let model = match v["model_kind"].as_str()? {
                    "direct" => {
                        ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(selection))
                    }
                    "alias" => ModelSelectionRequest::Alias(ModelAlias::from_uuid(selection)),
                    _ => return None,
                };
                let prompt = if v["system_prompt"].is_null() {
                    None
                } else {
                    Some(
                        SessionSystemPrompt::try_new(v["system_prompt"].as_str()?.to_owned())
                            .ok()?,
                    )
                };
                let defaults = SessionConfigurationDefaults::complete_with_model_settings(
                    model,
                    if v["auto_approval"].as_bool()? {
                        DangerousToolAutoApproval::ApproveAll
                    } else {
                        DangerousToolAutoApproval::Disabled
                    },
                    prompt,
                    model_settings_from_json(v["settings"].clone()).ok()?,
                )?;
                let digest: [u8; 32] = serde_json::from_value(v["digest"].clone()).ok()?;
                Some(Self::Create(Box::new(
                    CreateSession::new_from_template(
                        command,
                        SessionCreationProvenance::new(
                            SessionCreationCause::ModuleDispatched {
                                dispatch: ModuleDispatch::RepositoryWatch {
                                    dispatch: RepoWatchDispatchId::from_uuid(id(&v["dispatch"])?),
                                },
                            },
                            TranscriptAncestry::None,
                        ),
                        SessionTemplateProvenance::new(
                            SessionTemplateName::try_new(v["template"].as_str()?.to_owned())
                                .ok()?,
                            SessionTemplateContentDigest::from_bytes(digest),
                        ),
                        defaults,
                    )
                    .with_lifecycle(
                        StartGate::Held,
                        SessionOwnership::Owned,
                        Some(FinishCondition::ExternalGate),
                    ),
                )))
            }
            "lifecycle" => Some(Self::Lifecycle(SessionLifecycleCommand::new(
                command,
                SessionId::from_uuid(id(&v["session"])?),
                match v["operation"].as_str()? {
                    "release_start" => SessionLifecycleOperation::ReleaseStart,
                    "sticky_stop" => SessionLifecycleOperation::Stop {
                        sticky: StopStickiness::Sticky,
                        descendant_scope: DescendantTerminationScope::ParentAlone,
                    },
                    _ => return None,
                },
            ))),
            _ => None,
        }
    }
}

fn id(value: &Value) -> Option<Uuid> {
    Uuid::parse_str(value.as_str()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_domain::{
        FastMode, FastModeOverlay, ModelSettingsOverlay, ModelSettingsPrecedence, ReasoningLevel,
        SettingOverlay, ValidatedModelSettings,
    };

    #[test]
    fn recovery_preserves_template_defaults_and_lifecycle_payloads() {
        // Distinct identities and digest bytes are arbitrary, stable fixture data.
        let selection = DirectModelSelection::from_uuid(Uuid::from_u128(101));
        let precedence = ModelSettingsPrecedence::new(
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::new(
                SettingOverlay::Value(ReasoningLevel::High),
                FastModeOverlay::Value(FastMode::Enabled),
                SettingOverlay::ProviderDefault,
            ),
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
        );
        let resolved = precedence.resolve();
        let settings = ValidatedModelSettings::reconstitute(
            precedence,
            resolved.effective(),
            resolved.reasoning_source(),
            resolved.fast_mode_source(),
            resolved.service_tier_source(),
            Some(selection),
        )
        .expect("matching stored evidence");
        for model in [
            ModelSelectionRequest::Direct(selection),
            ModelSelectionRequest::Alias(ModelAlias::from_uuid(Uuid::from_u128(102))),
        ] {
            let defaults = SessionConfigurationDefaults::complete_with_model_settings(
                model,
                DangerousToolAutoApproval::ApproveAll,
                Some(
                    SessionSystemPrompt::try_new(String::from("Resolved prompt copy"))
                        .expect("prompt"),
                ),
                settings,
            )
            .expect("defaults");
            let command = RepoWatchCommandRecord::Create(Box::new(
                CreateSession::new_from_template(
                    DurableCommandId::from_uuid(Uuid::from_u128(103)),
                    SessionCreationProvenance::module_dispatched(ModuleDispatch::RepositoryWatch {
                        dispatch: RepoWatchDispatchId::from_uuid(Uuid::from_u128(104)),
                    }),
                    SessionTemplateProvenance::new(
                        SessionTemplateName::try_new(String::from("watch")).expect("name"),
                        SessionTemplateContentDigest::from_bytes([7; 32]),
                    ),
                    defaults,
                )
                .with_lifecycle(
                    StartGate::Held,
                    SessionOwnership::Owned,
                    Some(FinishCondition::ExternalGate),
                ),
            ));
            assert_eq!(
                RepoWatchCommandRecord::decode(&command.encode().expect("encode")),
                Some(command)
            );
        }
        for operation in [
            SessionLifecycleOperation::ReleaseStart,
            SessionLifecycleOperation::Stop {
                sticky: StopStickiness::Sticky,
                descendant_scope: DescendantTerminationScope::ParentAlone,
            },
        ] {
            let command = RepoWatchCommandRecord::Lifecycle(SessionLifecycleCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(105)),
                SessionId::from_uuid(Uuid::from_u128(106)),
                operation,
            ));
            assert_eq!(
                RepoWatchCommandRecord::decode(&command.encode().expect("encode")),
                Some(command)
            );
        }
    }
}
