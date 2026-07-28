//! Deployment-owned static session-template catalog.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use signalbox_domain::{
    DangerousToolAutoApproval, DirectModelSelection, ModelAlias, ModelSelectionRequest,
    SessionConfigurationDefaults, SessionSystemPrompt, SessionTemplateContentDigest,
    SessionTemplateName, SessionTemplateProvenance, SessionTemplateVersion,
};
use toml_edit::{DocumentMut, Table};
use uuid::Uuid;

use crate::HubModelConfiguration;

/// One immutable template resolved completely at daemon startup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSessionTemplate {
    version: SessionTemplateVersion,
    provenance: SessionTemplateProvenance,
    defaults: SessionConfigurationDefaults,
}

impl ResolvedSessionTemplate {
    /// Returns the owner-assigned bundle version.
    pub const fn version(&self) -> SessionTemplateVersion {
        self.version
    }

    /// Borrows the copied-content provenance.
    pub const fn provenance(&self) -> &SessionTemplateProvenance {
        &self.provenance
    }

    /// Borrows the complete resolved defaults copy.
    pub const fn defaults(&self) -> &SessionConfigurationDefaults {
        &self.defaults
    }
}

/// Validated process-lifetime session templates ordered by name.
#[derive(Clone, Debug, Default)]
pub struct SessionTemplateConfiguration {
    templates: BTreeMap<SessionTemplateName, ResolvedSessionTemplate>,
}

impl SessionTemplateConfiguration {
    /// Reads and resolves a complete version-one catalog and every prompt file.
    pub fn read(
        path: &Path,
        home: Option<&Path>,
        models: &HubModelConfiguration,
    ) -> Result<Self, SessionTemplateConfigurationError> {
        let content =
            fs::read_to_string(path).map_err(|_| SessionTemplateConfigurationError::ReadCatalog)?;
        Self::parse_at(&content, path, home, models)
    }

    fn parse_at(
        content: &str,
        path: &Path,
        home: Option<&Path>,
        models: &HubModelConfiguration,
    ) -> Result<Self, SessionTemplateConfigurationError> {
        let document = DocumentMut::from_str(content)
            .map_err(|_| SessionTemplateConfigurationError::InvalidDocument)?;
        reject_unknown_fields(document.as_table(), &["version", "templates"])?;
        if document.get("version").and_then(|item| item.as_integer()) != Some(1) {
            return Err(SessionTemplateConfigurationError::UnsupportedVersion);
        }
        let tables = document
            .get("templates")
            .map(|item| {
                item.as_array_of_tables()
                    .ok_or(SessionTemplateConfigurationError::InvalidTemplates)
            })
            .transpose()?;
        let mut templates = BTreeMap::new();
        if let Some(tables) = tables {
            for table in tables {
                let template = parse_template(table, path, home, models)?;
                let name = template.provenance().name().clone();
                if templates.insert(name, template).is_some() {
                    return Err(SessionTemplateConfigurationError::DuplicateName);
                }
            }
        }
        Ok(Self { templates })
    }

    /// Resolves one validated name from this immutable process snapshot.
    pub fn resolve(&self, name: &SessionTemplateName) -> Option<&ResolvedSessionTemplate> {
        self.templates.get(name)
    }

    /// Iterates immutable summaries in strict name order.
    pub fn summaries(
        &self,
    ) -> impl ExactSizeIterator<Item = (&SessionTemplateName, SessionTemplateVersion)> {
        self.templates
            .iter()
            .map(|(name, template)| (name, template.version()))
    }
}

fn parse_template(
    table: &Table,
    catalog_path: &Path,
    home: Option<&Path>,
    models: &HubModelConfiguration,
) -> Result<ResolvedSessionTemplate, SessionTemplateConfigurationError> {
    reject_unknown_fields(
        table,
        &[
            "name",
            "version",
            "model",
            "alias",
            "system_prompt",
            "system_prompt_file",
            "dangerous_tool_auto_approval",
        ],
    )?;
    let name = SessionTemplateName::try_new(required_string(table, "name")?.to_owned())
        .map_err(|_| SessionTemplateConfigurationError::InvalidName)?;
    let version = table
        .get("version")
        .and_then(|item| item.as_integer())
        .and_then(|value| u64::try_from(value).ok())
        .and_then(SessionTemplateVersion::try_from_u64)
        .ok_or(SessionTemplateConfigurationError::InvalidVersion)?;

    let direct = optional_uuid(table, "model")?;
    let alias = optional_uuid(table, "alias")?;
    let model = match (direct, alias) {
        (Some(_), Some(_)) => {
            return Err(SessionTemplateConfigurationError::ConflictingModelSelection);
        }
        (None, None) => return Err(SessionTemplateConfigurationError::MissingModelSelection),
        (Some(value), None) => {
            let selection = DirectModelSelection::from_uuid(value);
            if !models.contains_selection(selection) {
                return Err(SessionTemplateConfigurationError::UnknownModelSelection);
            }
            ModelSelectionRequest::Direct(selection)
        }
        (None, Some(value)) => {
            let alias = ModelAlias::from_uuid(value);
            if models.resolve_alias(alias).is_none() {
                return Err(SessionTemplateConfigurationError::UnknownModelSelection);
            }
            ModelSelectionRequest::Alias(alias)
        }
    };

    let inline = optional_string(table, "system_prompt")?;
    let file = optional_string(table, "system_prompt_file")?;
    let prompt = match (inline, file) {
        (Some(_), Some(_)) => return Err(SessionTemplateConfigurationError::ConflictingPrompt),
        (None, None) => return Err(SessionTemplateConfigurationError::MissingPrompt),
        (Some(value), None) => value.to_owned(),
        (None, Some(reference)) => {
            let prompt_path = resolve_prompt_path(reference, catalog_path, home)?;
            fs::read_to_string(prompt_path)
                .map_err(|_| SessionTemplateConfigurationError::ReadPrompt)?
        }
    };
    let prompt = SessionSystemPrompt::try_new(prompt)
        .map_err(|_| SessionTemplateConfigurationError::InvalidPrompt)?;
    let dangerous_tool_auto_approval = table
        .get("dangerous_tool_auto_approval")
        .and_then(|item| item.as_bool())
        .ok_or(SessionTemplateConfigurationError::InvalidApproval)?;
    let dangerous_tool_auto_approval = if dangerous_tool_auto_approval {
        DangerousToolAutoApproval::ApproveAll
    } else {
        DangerousToolAutoApproval::Disabled
    };
    let defaults =
        SessionConfigurationDefaults::complete(model, dangerous_tool_auto_approval, Some(prompt));
    let digest = SessionTemplateContentDigest::derive(version, &defaults)
        .ok_or(SessionTemplateConfigurationError::MissingPrompt)?;
    Ok(ResolvedSessionTemplate {
        version,
        provenance: SessionTemplateProvenance::new(name, digest),
        defaults,
    })
}

fn resolve_prompt_path(
    reference: &str,
    catalog_path: &Path,
    home: Option<&Path>,
) -> Result<PathBuf, SessionTemplateConfigurationError> {
    if reference.is_empty()
        || reference.contains('\\')
        || reference
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(SessionTemplateConfigurationError::InvalidPromptPath);
    }
    if let Some(suffix) = reference.strip_prefix("$HOME/") {
        if suffix.is_empty() || suffix.contains('$') {
            return Err(SessionTemplateConfigurationError::InvalidPromptPath);
        }
        return home
            .map(|home| home.join(suffix))
            .ok_or(SessionTemplateConfigurationError::MissingHome);
    }
    if reference.starts_with('/') || reference.contains('$') {
        return Err(SessionTemplateConfigurationError::InvalidPromptPath);
    }
    Ok(catalog_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(reference))
}

fn reject_unknown_fields(
    table: &Table,
    allowed: &[&str],
) -> Result<(), SessionTemplateConfigurationError> {
    if table.iter().any(|(key, _)| !allowed.contains(&key)) {
        Err(SessionTemplateConfigurationError::UnknownField)
    } else {
        Ok(())
    }
}

fn required_string<'a>(
    table: &'a Table,
    key: &str,
) -> Result<&'a str, SessionTemplateConfigurationError> {
    table
        .get(key)
        .and_then(|item| item.as_str())
        .ok_or(SessionTemplateConfigurationError::InvalidField)
}

fn optional_string<'a>(
    table: &'a Table,
    key: &str,
) -> Result<Option<&'a str>, SessionTemplateConfigurationError> {
    table
        .get(key)
        .map(|item| {
            item.as_str()
                .ok_or(SessionTemplateConfigurationError::InvalidField)
        })
        .transpose()
}

fn optional_uuid(
    table: &Table,
    key: &str,
) -> Result<Option<Uuid>, SessionTemplateConfigurationError> {
    optional_string(table, key)?
        .map(|value| {
            Uuid::parse_str(value).map_err(|_| SessionTemplateConfigurationError::InvalidIdentity)
        })
        .transpose()
}

/// Sanitized static session-template configuration failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionTemplateConfigurationError {
    ReadCatalog,
    InvalidDocument,
    UnsupportedVersion,
    InvalidTemplates,
    UnknownField,
    InvalidField,
    InvalidName,
    DuplicateName,
    InvalidVersion,
    InvalidIdentity,
    MissingModelSelection,
    ConflictingModelSelection,
    UnknownModelSelection,
    MissingPrompt,
    ConflictingPrompt,
    InvalidPromptPath,
    MissingHome,
    ReadPrompt,
    InvalidPrompt,
    InvalidApproval,
}

impl fmt::Display for SessionTemplateConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ReadCatalog => "session-template configuration file could not be read",
            Self::InvalidDocument => "session-template configuration is not valid TOML",
            Self::UnsupportedVersion => "session-template configuration version is unsupported",
            Self::InvalidTemplates => "session templates are not an array of tables",
            Self::UnknownField => "session-template configuration contains an unknown field",
            Self::InvalidField => "session-template configuration has a missing or mistyped field",
            Self::InvalidName => "session-template configuration contains an invalid name",
            Self::DuplicateName => "session-template configuration repeats a name",
            Self::InvalidVersion => "session-template configuration contains an invalid version",
            Self::InvalidIdentity => "session-template configuration contains an invalid identity",
            Self::MissingModelSelection => "session template has no model selection",
            Self::ConflictingModelSelection => "session template has multiple model selections",
            Self::UnknownModelSelection => "session template names an unknown model selection",
            Self::MissingPrompt => "session template has no system prompt",
            Self::ConflictingPrompt => "session template has multiple system prompts",
            Self::InvalidPromptPath => "session template contains an invalid prompt path",
            Self::MissingHome => "session template prompt requires an unavailable home directory",
            Self::ReadPrompt => "session template prompt file could not be read as UTF-8",
            Self::InvalidPrompt => "session template contains an invalid system prompt",
            Self::InvalidApproval => "session template has a missing or mistyped approval posture",
        })
    }
}

impl Error for SessionTemplateConfigurationError {}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use signalbox_domain::{DangerousToolAutoApproval, ModelSelectionRequest, SessionTemplateName};

    use super::{SessionTemplateConfiguration, SessionTemplateConfigurationError};
    use crate::HubModelConfiguration;

    const SELECTION_ID: &str = "10000000-0000-4000-8000-000000000001";
    const TARGET_ID: &str = "20000000-0000-4000-8000-000000000002";
    const ALIAS_ID: &str = "30000000-0000-4000-8000-000000000003";
    const TEMPLATE_NAME: &str = "reviewer";
    const TEMPLATE_VERSION: u64 = 7;
    const INLINE_PROMPT: &str = "Review the change and report concrete findings.";

    fn models() -> HubModelConfiguration {
        HubModelConfiguration::parse(&format!(
            r#"
version = 1

[[models]]
selection_id = "{SELECTION_ID}"
target_id = "{TARGET_ID}"
provider = "anthropic"
provider_model = "synthetic-model"
max_output_tokens = 1024

[[aliases]]
alias_id = "{ALIAS_ID}"
selection_id = "{SELECTION_ID}"
"#,
        ))
        .expect("synthetic model fixture is valid")
    }

    fn inline_catalog(extra: &str) -> String {
        format!(
            r#"
version = 1

[[templates]]
name = "{TEMPLATE_NAME}"
version = {TEMPLATE_VERSION}
alias = "{ALIAS_ID}"
system_prompt = "{INLINE_PROMPT}"
dangerous_tool_auto_approval = true
{extra}
"#,
        )
    }

    #[test]
    fn inv047_inline_template_resolves_a_complete_digest_bound_bundle() {
        let configuration = SessionTemplateConfiguration::parse_at(
            &inline_catalog(""),
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        )
        .expect("valid inline template catalog loads");
        let name = SessionTemplateName::try_new(TEMPLATE_NAME.to_owned())
            .expect("fixture template name is admitted");
        let template = configuration
            .resolve(&name)
            .expect("configured template resolves");

        assert_eq!(template.version().as_u64(), TEMPLATE_VERSION);
        assert_eq!(template.provenance().name(), &name);
        assert_eq!(
            template.defaults().model(),
            ModelSelectionRequest::Alias(signalbox_domain::ModelAlias::from_uuid(
                uuid::Uuid::parse_str(ALIAS_ID).expect("fixture alias is a UUID"),
            ))
        );
        assert_eq!(
            template.defaults().dangerous_tool_auto_approval(),
            DangerousToolAutoApproval::ApproveAll
        );
        assert_eq!(
            template
                .defaults()
                .system_prompt()
                .expect("template prompt is required")
                .as_str(),
            INLINE_PROMPT
        );
        assert_ne!(template.provenance().content_digest().as_bytes(), &[0; 32]);
    }

    #[test]
    fn inv047_home_prompt_reference_copies_exact_file_content() {
        let temporary = tempfile::tempdir().expect("temporary deployment root");
        let prompt_directory = temporary.path().join("prompts");
        fs::create_dir(&prompt_directory).expect("prompt directory is created");
        let prompt_path = prompt_directory.join("reviewer.txt");
        fs::write(&prompt_path, INLINE_PROMPT).expect("synthetic prompt is written");
        let catalog = format!(
            r#"
version = 1

[[templates]]
name = "{TEMPLATE_NAME}"
version = {TEMPLATE_VERSION}
model = "{SELECTION_ID}"
system_prompt_file = "$HOME/prompts/reviewer.txt"
dangerous_tool_auto_approval = false
"#,
        );
        let configuration = SessionTemplateConfiguration::parse_at(
            &catalog,
            Path::new("deployment/session-templates.toml"),
            Some(temporary.path()),
            &models(),
        )
        .expect("valid home-relative prompt loads");
        let name = SessionTemplateName::try_new(TEMPLATE_NAME.to_owned())
            .expect("fixture template name is admitted");
        let template = configuration
            .resolve(&name)
            .expect("configured template resolves");

        assert_eq!(
            template
                .defaults()
                .system_prompt()
                .expect("template prompt is required")
                .as_str(),
            INLINE_PROMPT
        );
    }

    #[test]
    fn invalid_catalog_shapes_return_distinct_typed_failures() {
        let unknown_field = SessionTemplateConfiguration::parse_at(
            &inline_catalog("unexpected = true"),
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        );
        let conflicting_prompt = SessionTemplateConfiguration::parse_at(
            &inline_catalog("system_prompt_file = \"prompt.txt\""),
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        );

        assert_eq!(
            unknown_field.expect_err("unknown template field is rejected"),
            SessionTemplateConfigurationError::UnknownField
        );
        assert_eq!(
            conflicting_prompt.expect_err("dual prompt sources are rejected"),
            SessionTemplateConfigurationError::ConflictingPrompt
        );
    }

    #[test]
    fn unknown_model_and_unavailable_home_return_typed_failures() {
        let unknown_model = inline_catalog("").replace(ALIAS_ID, TARGET_ID);
        let missing_home = inline_catalog("").replace(
            &format!("system_prompt = \"{INLINE_PROMPT}\""),
            "system_prompt_file = \"$HOME/prompts/reviewer.txt\"",
        );

        assert_eq!(
            SessionTemplateConfiguration::parse_at(
                &unknown_model,
                Path::new("deployment/session-templates.toml"),
                None,
                &models(),
            )
            .expect_err("unknown alias is rejected"),
            SessionTemplateConfigurationError::UnknownModelSelection
        );
        assert_eq!(
            SessionTemplateConfiguration::parse_at(
                &missing_home,
                Path::new("deployment/session-templates.toml"),
                None,
                &models(),
            )
            .expect_err("home reference requires HOME"),
            SessionTemplateConfigurationError::MissingHome
        );
    }
}
