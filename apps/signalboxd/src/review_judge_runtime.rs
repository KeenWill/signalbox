//! Narrow tool catalog for the agentic review-judgment template.

use signalbox_application::{
    ToolCatalog, ToolCatalogValidationFailure, ToolDefinition, ToolPreauthorization,
};
use signalbox_domain::{NormalizedToolArguments, ToolName};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JudgeMode {
    Ordinary,
    Synopsis,
    FullContext,
    FullContextWithoutTools,
}

impl JudgeMode {
    fn from_template(name: &str) -> Self {
        match name {
            "review-judgment-agentic" => Self::Synopsis,
            "review-judgment-agentic-full" => Self::FullContext,
            "review-judgment-agentic-full-no-tools" => Self::FullContextWithoutTools,
            _ => Self::Ordinary,
        }
    }

    pub(crate) fn tool_limit(self) -> Option<u64> {
        match self {
            Self::Ordinary => None,
            Self::Synopsis => Some(8),
            Self::FullContext => Some(16),
            Self::FullContextWithoutTools => Some(0),
        }
    }

    fn remaining(self, used: u64) -> Option<u64> {
        self.tool_limit().map(|limit| limit.saturating_sub(used))
    }
}

pub(crate) async fn judge_mode(
    repository: &signalbox_persistence::model_execution::PostgresModelCallRepository,
    session: signalbox_domain::SessionId,
) -> Result<JudgeMode, signalbox_persistence::model_execution::ModelCallRepositoryError> {
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
        .map_or(JudgeMode::Ordinary, |template| {
            JudgeMode::from_template(template.name().as_str())
        }))
}

pub(crate) async fn tool_allowance(
    repository: &signalbox_persistence::model_execution::PostgresModelCallRepository,
    mode: JudgeMode,
    session: signalbox_domain::SessionId,
    turn: signalbox_domain::TurnId,
) -> Result<Option<u64>, signalbox_persistence::model_execution::ModelCallRepositoryError> {
    if mode.tool_limit().is_none() {
        return Ok(None);
    }
    Ok(mode.remaining(repository.turn_tool_request_count(session, turn).await?))
}

#[derive(Clone)]
pub(crate) struct JudgeCatalog<Catalog> {
    pub catalog: Catalog,
    pub mode: JudgeMode,
}

impl<Catalog> JudgeCatalog<Catalog> {
    fn permits(&self, name: &ToolName) -> bool {
        let review_text = matches!(
            name.as_str(),
            crate::blob_tools::FINDING_TEXT_NAME | crate::blob_tools::REVIEW_THREAD_TEXT_NAME
        );
        let extended = matches!(name.as_str(), "review_thread_list" | "read_diff");
        match self.mode {
            JudgeMode::Ordinary => !review_text && !extended,
            JudgeMode::Synopsis => review_text || name.as_str() == "read_file",
            JudgeMode::FullContext => review_text || extended || name.as_str() == "read_file",
            JudgeMode::FullContextWithoutTools => false,
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
            mode: JudgeMode::Synopsis,
        };
        assert_eq!(judge.definitions().len(), 3);
        let ordinary = JudgeCatalog {
            catalog: judge.catalog.clone(),
            mode: JudgeMode::Ordinary,
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

    #[test]
    fn full_context_ablation_removes_both_tool_advertisement_and_admission() {
        let catalog = CompiledToolCatalog::try_new([CompiledTool::new(
            ToolDefinition::new(
                ToolName::try_new(String::from("read_file")).unwrap(),
                String::from("Read source"),
                ToolInputSchema::try_new(String::from(r#"{"type":"object"}"#)).unwrap(),
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            |_arguments: &NormalizedToolArguments| Ok(()),
        )])
        .unwrap();
        let judge = JudgeCatalog {
            catalog,
            mode: JudgeMode::from_template("review-judgment-agentic-full-no-tools"),
        };
        assert_eq!(judge.mode.tool_limit(), Some(0));
        assert!(judge.definitions().is_empty());
        let arguments =
            NormalizedToolArguments::try_from_provider_text(String::from("{}")).unwrap();
        assert!(
            judge
                .validate_arguments(
                    &ToolName::try_new(String::from("read_file")).unwrap(),
                    &arguments
                )
                .is_err()
        );
    }

    #[test]
    fn full_context_judgments_can_fetch_beyond_the_synopsis_allowance() {
        let mode = JudgeMode::from_template("review-judgment-agentic-full");
        assert_eq!(mode.remaining(8), Some(8));
        assert_eq!(mode.remaining(16), Some(0));
        assert_eq!(mode.remaining(17), Some(0));
    }
}
