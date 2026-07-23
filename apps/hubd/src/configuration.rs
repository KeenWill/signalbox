//! Deployment-owned model mappings and credential delivery.

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use signalbox_domain::{
    DirectModelSelection, FrozenAliasDefinition, ModelAlias, ModelTargetCatalog,
    ModelTargetDefinition, ProviderModelIdentity, ResolvedProviderTarget,
};
use signalbox_model_provider_runtime::{RuntimeModelCatalog, RuntimeModelDefinition};
use signalbox_model_runtime::{
    CredentialAccess, CredentialAccessError, CredentialAccessFailure, CredentialReference,
    CredentialValue,
};
use toml_edit::{DocumentMut, Table};
use uuid::Uuid;

/// Non-secret reference pinned into every Anthropic operation.
pub const ANTHROPIC_CREDENTIAL_REFERENCE: &str = "anthropic-primary";

/// Validated static model and alias definitions used by hub composition.
#[derive(Clone, Debug)]
pub struct HubModelConfiguration {
    targets: ModelTargetCatalog,
    runtime_models: RuntimeModelCatalog,
    direct_selections: HashSet<DirectModelSelection>,
    aliases: HashMap<ModelAlias, FrozenAliasDefinition>,
}

impl HubModelConfiguration {
    /// Reads and validates the versioned static TOML document.
    pub fn read(path: &Path) -> Result<Self, HubModelConfigurationError> {
        let content = fs::read_to_string(path).map_err(|_| HubModelConfigurationError::Read)?;
        Self::parse(&content)
    }

    /// Parses one complete versioned configuration document.
    pub fn parse(content: &str) -> Result<Self, HubModelConfigurationError> {
        let document = DocumentMut::from_str(content)
            .map_err(|_| HubModelConfigurationError::InvalidDocument)?;
        reject_unknown_fields(document.as_table(), &["version", "models", "aliases"])?;
        if document.get("version").and_then(|item| item.as_integer()) != Some(1) {
            return Err(HubModelConfigurationError::UnsupportedVersion);
        }
        let models = document
            .get("models")
            .and_then(|item| item.as_array_of_tables())
            .ok_or(HubModelConfigurationError::MissingModels)?;
        if models.is_empty() {
            return Err(HubModelConfigurationError::MissingModels);
        }

        let mut domain_definitions = Vec::with_capacity(models.len());
        let mut runtime_definitions = Vec::with_capacity(models.len());
        let mut direct_selections = HashSet::with_capacity(models.len());
        for model in models {
            reject_unknown_fields(
                model,
                &[
                    "selection_id",
                    "target_id",
                    "provider",
                    "provider_model",
                    "max_output_tokens",
                ],
            )?;
            let selection = DirectModelSelection::from_uuid(required_uuid(model, "selection_id")?);
            if !direct_selections.insert(selection) {
                return Err(HubModelConfigurationError::DuplicateSelection);
            }
            if required_string(model, "provider")? != "anthropic" {
                return Err(HubModelConfigurationError::UnsupportedProvider);
            }
            let provider_model = required_string(model, "provider_model")?;
            if provider_model.is_empty() || provider_model.trim() != provider_model {
                return Err(HubModelConfigurationError::InvalidProviderModel);
            }
            let max_output_tokens = required_positive_u32(model, "max_output_tokens")?;
            let target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                required_uuid(model, "target_id")?,
            ));
            domain_definitions.push(ModelTargetDefinition::new(selection, target));
            runtime_definitions.push(
                RuntimeModelDefinition::try_new(
                    target,
                    provider_model.to_owned(),
                    max_output_tokens,
                )
                .map_err(|_| HubModelConfigurationError::InvalidField)?,
            );
        }

        let mut aliases = HashMap::new();
        if let Some(alias_tables) = document
            .get("aliases")
            .map(|item| {
                item.as_array_of_tables()
                    .ok_or(HubModelConfigurationError::InvalidAliases)
            })
            .transpose()?
        {
            for alias in alias_tables {
                reject_unknown_fields(alias, &["alias_id", "selection_id"])?;
                let identity = ModelAlias::from_uuid(required_uuid(alias, "alias_id")?);
                let selected =
                    DirectModelSelection::from_uuid(required_uuid(alias, "selection_id")?);
                if !direct_selections.contains(&selected) {
                    return Err(HubModelConfigurationError::DanglingAlias);
                }
                if aliases
                    .insert(identity, FrozenAliasDefinition::selecting(selected))
                    .is_some()
                {
                    return Err(HubModelConfigurationError::DuplicateAlias);
                }
            }
        }

        let targets = ModelTargetCatalog::try_from_definitions(domain_definitions)
            .map_err(|_| HubModelConfigurationError::DuplicateSelection)?;
        let runtime_models = RuntimeModelCatalog::try_from_definitions(runtime_definitions)
            .map_err(|_| HubModelConfigurationError::ConflictingTarget)?;
        Ok(Self {
            targets,
            runtime_models,
            direct_selections,
            aliases,
        })
    }

    /// Returns the immutable domain target catalog used by persistence.
    pub fn target_catalog(&self) -> ModelTargetCatalog {
        self.targets.clone()
    }

    /// Returns the exact runtime delivery catalog used by the provider bridge.
    pub fn runtime_model_catalog(&self) -> RuntimeModelCatalog {
        self.runtime_models.clone()
    }

    /// Reports whether the configuration contains one direct selection key.
    pub fn contains_selection(&self, selection: DirectModelSelection) -> bool {
        self.direct_selections.contains(&selection)
    }

    /// Resolves one configured alias to the immutable definition frozen at
    /// acceptance time.
    pub fn resolve_alias(&self, alias: ModelAlias) -> Option<FrozenAliasDefinition> {
        self.aliases.get(&alias).copied()
    }
}

fn reject_unknown_fields(
    table: &Table,
    allowed: &[&str],
) -> Result<(), HubModelConfigurationError> {
    if table.iter().any(|(key, _)| !allowed.contains(&key)) {
        Err(HubModelConfigurationError::UnknownField)
    } else {
        Ok(())
    }
}

fn required_string<'a>(table: &'a Table, key: &str) -> Result<&'a str, HubModelConfigurationError> {
    table
        .get(key)
        .and_then(|item| item.as_str())
        .ok_or(HubModelConfigurationError::InvalidField)
}

fn required_uuid(table: &Table, key: &str) -> Result<Uuid, HubModelConfigurationError> {
    Uuid::parse_str(required_string(table, key)?)
        .map_err(|_| HubModelConfigurationError::InvalidIdentity)
}

fn required_positive_u32(table: &Table, key: &str) -> Result<u32, HubModelConfigurationError> {
    let value = table
        .get(key)
        .and_then(|item| item.as_integer())
        .ok_or(HubModelConfigurationError::InvalidField)?;
    let value = u32::try_from(value).map_err(|_| HubModelConfigurationError::InvalidLimit)?;
    if value == 0 {
        Err(HubModelConfigurationError::InvalidLimit)
    } else {
        Ok(value)
    }
}

/// Sanitized static-configuration failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HubModelConfigurationError {
    /// The configuration file could not be read as UTF-8 text.
    Read,
    /// The content was not a TOML document.
    InvalidDocument,
    /// The document version is absent or unsupported.
    UnsupportedVersion,
    /// No nonempty model-definition array exists.
    MissingModels,
    /// An unrecognized root or table field was present.
    UnknownField,
    /// A required field had the wrong TOML type or was absent.
    InvalidField,
    /// A configured identity was not a UUID.
    InvalidIdentity,
    /// Only the Anthropic provider is admitted by this composition slice.
    UnsupportedProvider,
    /// The provider-native model spelling was empty or padded.
    InvalidProviderModel,
    /// The output-token ceiling was zero or outside `u32`.
    InvalidLimit,
    /// One direct selection appeared more than once.
    DuplicateSelection,
    /// One target was assigned conflicting runtime meanings.
    ConflictingTarget,
    /// The aliases field was not an array of tables.
    InvalidAliases,
    /// One alias appeared more than once.
    DuplicateAlias,
    /// An alias selected no configured direct model.
    DanglingAlias,
}

impl fmt::Display for HubModelConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Read => "model configuration file could not be read",
            Self::InvalidDocument => "model configuration is not valid TOML",
            Self::UnsupportedVersion => "model configuration version is unsupported",
            Self::MissingModels => "model configuration has no model definitions",
            Self::UnknownField => "model configuration contains an unknown field",
            Self::InvalidField => "model configuration has a missing or mistyped field",
            Self::InvalidIdentity => "model configuration contains an invalid identity",
            Self::UnsupportedProvider => "model configuration names an unsupported provider",
            Self::InvalidProviderModel => "model configuration contains an invalid provider model",
            Self::InvalidLimit => "model configuration contains an invalid output limit",
            Self::DuplicateSelection => "model configuration repeats a direct selection",
            Self::ConflictingTarget => "model configuration gives one target conflicting meaning",
            Self::InvalidAliases => "model aliases are not an array of tables",
            Self::DuplicateAlias => "model configuration repeats an alias",
            Self::DanglingAlias => "model configuration contains a dangling alias",
        })
    }
}

impl Error for HubModelConfigurationError {}

/// Credential source that rereads one deployment-owned secret file for every
/// request preparation so rotation is visible without restarting hubd.
#[derive(Clone)]
pub struct FileCredentialAccess {
    path: Arc<PathBuf>,
    reference: CredentialReference,
}

impl FileCredentialAccess {
    /// Binds one non-secret credential reference to one deployment file.
    pub fn new(path: PathBuf, reference: CredentialReference) -> Self {
        Self {
            path: Arc::new(path),
            reference,
        }
    }

    /// Returns the non-secret reference accepted by this source.
    pub fn credential_reference(&self) -> CredentialReference {
        self.reference.clone()
    }
}

impl fmt::Debug for FileCredentialAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileCredentialAccess")
            .field("path", &"[credential file]")
            .field("reference", &self.reference)
            .finish()
    }
}

impl CredentialAccess for FileCredentialAccess {
    async fn resolve(
        &self,
        reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        if reference != &self.reference {
            return Err(CredentialAccessError::new(
                reference.clone(),
                CredentialAccessFailure::Unmapped,
            ));
        }
        match tokio::fs::read(self.path.as_ref()).await {
            Ok(value) => Ok(CredentialValue::new(value)),
            Err(error) => Err(CredentialAccessError::new(
                reference.clone(),
                if error.kind() == io::ErrorKind::NotFound {
                    CredentialAccessFailure::Unavailable
                } else {
                    CredentialAccessFailure::Unreadable
                },
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use signalbox_domain::{DirectModelSelection, ModelAlias};
    use signalbox_model_runtime::{CredentialAccess, CredentialAccessFailure, CredentialReference};
    use uuid::Uuid;

    use super::{
        ANTHROPIC_CREDENTIAL_REFERENCE, FileCredentialAccess, HubModelConfiguration,
        HubModelConfigurationError,
    };

    const CONFIGURATION: &str = r#"
version = 1

[[models]]
selection_id = "10000000-0000-4000-8000-000000000001"
target_id = "20000000-0000-4000-8000-000000000001"
provider = "anthropic"
provider_model = "claude-example"
max_output_tokens = 256

[[aliases]]
alias_id = "30000000-0000-4000-8000-000000000001"
selection_id = "10000000-0000-4000-8000-000000000001"
"#;

    #[test]
    fn static_configuration_builds_correlated_domain_runtime_and_alias_mappings() {
        let configuration =
            HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");
        let selection = DirectModelSelection::from_uuid(
            Uuid::parse_str("10000000-0000-4000-8000-000000000001").expect("fixture UUID is valid"),
        );
        let alias = ModelAlias::from_uuid(
            Uuid::parse_str("30000000-0000-4000-8000-000000000001").expect("fixture UUID is valid"),
        );
        assert!(configuration.contains_selection(selection));
        assert_eq!(
            configuration
                .resolve_alias(alias)
                .expect("fixture alias resolves")
                .selected(),
            selection
        );
        assert!(
            configuration
                .target_catalog()
                .resolve(signalbox_domain::FrozenModelSelection::Direct(selection))
                .is_ok()
        );
    }

    #[test]
    fn configuration_rejects_unknown_fields_and_dangling_aliases() {
        assert_eq!(
            HubModelConfiguration::parse(&CONFIGURATION.replace(
                "max_output_tokens = 256",
                "max_output_tokens = 256\nretry = true",
            ))
            .err(),
            Some(HubModelConfigurationError::UnknownField)
        );
        let dangling = CONFIGURATION.rsplit_once("[[aliases]]").map_or_else(
            || String::from(CONFIGURATION),
            |(prefix, _)| {
                format!(
                    "{prefix}[[aliases]]\nalias_id = \"30000000-0000-4000-8000-000000000001\"\nselection_id = \"10000000-0000-4000-8000-000000000009\"\n"
                )
            },
        );
        assert_eq!(
            HubModelConfiguration::parse(&dangling).err(),
            Some(HubModelConfigurationError::DanglingAlias)
        );
    }

    /// INV-035: credential references stay scoped while paths and values stay
    /// out of errors and debug output.
    #[tokio::test]
    async fn file_credentials_are_reference_scoped_and_paths_are_redacted() {
        let source = FileCredentialAccess::new(
            PathBuf::from("/definitely/not/a/credential"),
            CredentialReference::new(ANTHROPIC_CREDENTIAL_REFERENCE),
        );
        assert_eq!(
            source
                .resolve(&CredentialReference::new("another-reference"))
                .await
                .expect_err("foreign references are rejected")
                .failure,
            CredentialAccessFailure::Unmapped
        );
        assert_eq!(
            source
                .resolve(&source.credential_reference())
                .await
                .expect_err("fixture path does not exist")
                .failure,
            CredentialAccessFailure::Unavailable
        );
        assert!(!format!("{source:?}").contains("definitely"));
    }

    /// INV-035: each operation preparation observes the file as it exists at
    /// that request, so atomic deployment replacement rotates the key without
    /// caching secret bytes in hub composition.
    #[tokio::test]
    async fn inv035_file_credentials_are_reread_for_rotation() {
        let path = std::env::temp_dir().join(format!("signalbox-credential-{}", Uuid::now_v7()));
        std::fs::write(&path, b"first-test-value").expect("fixture file is writable");
        let source = FileCredentialAccess::new(
            path.clone(),
            CredentialReference::new(ANTHROPIC_CREDENTIAL_REFERENCE),
        );
        let reference = source.credential_reference();
        assert_eq!(
            source
                .resolve(&reference)
                .await
                .expect("first fixture value resolves")
                .expose_bytes(),
            b"first-test-value"
        );
        std::fs::write(&path, b"rotated-test-value").expect("fixture file can be replaced");
        assert_eq!(
            source
                .resolve(&reference)
                .await
                .expect("rotated fixture value resolves")
                .expose_bytes(),
            b"rotated-test-value"
        );
        std::fs::remove_file(path).expect("fixture file is removable");
    }
}
