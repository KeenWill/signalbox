//! Narrow tool catalog for the agentic review-judgment template.

use signalbox_application::{
    ToolCatalog, ToolCatalogValidationFailure, ToolDefinition, ToolPreauthorization,
};
use signalbox_domain::{NormalizedToolArguments, ToolName};

pub(crate) const TEMPLATE_NAME: &str = "review-judgment-agentic";
/// Each judgment may admit at most eight tool requests across its turn.
pub(crate) const TOOL_CALL_LIMIT: u64 = 8;
/// Allows a final response after the read allowance is spent.
pub(crate) const TOOL_ROUND_LIMIT: usize = TOOL_CALL_LIMIT as usize + 1;

pub(crate) async fn is_agentic_judge(
    repository: &signalbox_persistence::model_execution::PostgresModelCallRepository,
    session: signalbox_domain::SessionId,
) -> Result<bool, signalbox_persistence::model_execution::ModelCallRepositoryError> {
    use signalbox_persistence::{
        model_execution::{ModelCallCorruption, ModelCallRepositoryError},
        session::SessionRepositoryError,
    };
    let loaded = repository
        .session_repository()
        .load_session(session)
        .await
        .map_err(|error| match error {
            SessionRepositoryError::Database(error) => ModelCallRepositoryError::from(error),
            SessionRepositoryError::Corruption(error) => {
                ModelCallCorruption::CurrentSession(error).into()
            }
        })?;
    Ok(loaded
        .as_ref()
        .and_then(|session| session.template_provenance())
        .is_some_and(|template| template.name().as_str() == TEMPLATE_NAME))
}

pub(crate) async fn tool_allowance(
    repository: &signalbox_persistence::model_execution::PostgresModelCallRepository,
    restricted: bool,
    session: signalbox_domain::SessionId,
    turn: signalbox_domain::TurnId,
) -> Result<Option<u64>, signalbox_persistence::model_execution::ModelCallRepositoryError> {
    if !restricted {
        return Ok(None);
    }
    Ok(Some(TOOL_CALL_LIMIT.saturating_sub(
        repository.turn_tool_request_count(session, turn).await?,
    )))
}

#[derive(Clone)]
pub(crate) struct JudgeCatalog<Catalog> {
    pub catalog: Catalog,
    pub restricted: bool,
}

impl<Catalog> JudgeCatalog<Catalog> {
    fn permits(&self, name: &ToolName) -> bool {
        let review_text = matches!(
            name.as_str(),
            crate::blob_tools::FINDING_TEXT_NAME | crate::blob_tools::REVIEW_THREAD_TEXT_NAME
        );
        if self.restricted {
            review_text || name.as_str() == "read_file"
        } else {
            !review_text
        }
    }
}

impl<Catalog: ToolCatalog> ToolCatalog for JudgeCatalog<Catalog> {
    fn definitions(&self) -> Box<[ToolDefinition]> {
        self.catalog
            .definitions()
            .into_vec()
            .into_iter()
            .filter(|definition| self.permits(definition.name()))
            .collect()
    }

    fn definition(&self, name: &ToolName) -> Option<ToolDefinition> {
        self.permits(name)
            .then(|| self.catalog.definition(name))
            .flatten()
    }

    fn validate_arguments(
        &self,
        name: &ToolName,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolCatalogValidationFailure> {
        if !self.permits(name) {
            return Err(ToolCatalogValidationFailure::UnknownTool);
        }
        self.catalog.validate_arguments(name, arguments)
    }

    fn preauthorization(
        &self,
        name: &ToolName,
        arguments: &NormalizedToolArguments,
    ) -> Result<ToolPreauthorization, ToolCatalogValidationFailure> {
        if !self.permits(name) {
            return Err(ToolCatalogValidationFailure::UnknownTool);
        }
        self.catalog.preauthorization(name, arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_application::{CompiledTool, CompiledToolCatalog, ToolInputSchema};
    use signalbox_domain::{ToolEffectClass, ToolPermissionDefault};

    #[test]
    fn a_judge_cannot_advertise_or_dispatch_tools_outside_its_read_catalog() {
        let names = [
            "read_file",
            "finding_text",
            "review_thread_text",
            "write_file",
            "exec_command",
            "blob_read",
        ];
        let catalog = CompiledToolCatalog::try_new(names.map(|name| {
            CompiledTool::new(
                ToolDefinition::new(
                    ToolName::try_new(name.to_owned()).unwrap(),
                    name.to_owned(),
                    ToolInputSchema::try_new(String::from(r#"{"type":"object"}"#)).unwrap(),
                    ToolPermissionDefault::Auto,
                    ToolEffectClass::EffectFree,
                ),
                |_arguments: &NormalizedToolArguments| Ok(()),
            )
        }))
        .unwrap();
        let judge = JudgeCatalog {
            catalog,
            restricted: true,
        };
        assert_eq!(judge.definitions().len(), 3);
        let ordinary = JudgeCatalog {
            catalog: judge.catalog.clone(),
            restricted: false,
        };
        assert_eq!(ordinary.definitions().len(), 4);
        assert!(
            ordinary
                .definition(&ToolName::try_new(String::from("finding_text")).unwrap())
                .is_none()
        );
        assert!(
            ordinary
                .definition(&ToolName::try_new(String::from("write_file")).unwrap())
                .is_some()
        );
        for (index, name) in names.into_iter().enumerate() {
            let name = ToolName::try_new(name.to_owned()).unwrap();
            let arguments =
                NormalizedToolArguments::try_from_provider_text(String::from("{}")).unwrap();
            assert_eq!(judge.definition(&name).is_some(), index < 3);
            assert_eq!(
                judge.validate_arguments(&name, &arguments).is_ok(),
                index < 3
            );
            assert_eq!(judge.preauthorization(&name, &arguments).is_ok(), index < 3);
        }
    }
}
