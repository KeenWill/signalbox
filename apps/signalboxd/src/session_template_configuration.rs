//! Deployment-owned static session-template catalog.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt, fs,
    io::Read,
    path::{Path, PathBuf},
    str::FromStr,
};

use rustix::fs::{Mode, OFlags, open};
use sha2::{Digest, Sha256};
use signalbox_application::{
    RepoWatchResolvedTemplate, RepoWatchTemplateResolver, ReviewConcernSpec,
    ReviewOrchestrationAttempt, ReviewOrchestrationAttemptError, ReviewOrchestrationAttemptId,
    ReviewStageTemplateDigests, ReviewTemplateDigest,
};
use signalbox_domain::{
    DangerousToolAutoApproval, DirectModelSelection, ModelAlias, ModelSelectionRequest,
    ModelSettingsOverlay, RepoWatchDispatchContextShape, RepoWatchTemplateContextDeclaration,
    ReviewKey, ReviewPolicy, ReviewTargetId, SessionConfigurationDefaults, SessionSystemPrompt,
    SessionTemplateContentDigest, SessionTemplateName, SessionTemplateProvenance,
    SessionTemplateVersion, ValidatedModelSettings,
};
use toml_edit::{DocumentMut, Table};
use uuid::Uuid;

use crate::HubModelConfiguration;

pub(crate) const REVIEW_IMPORT_TEMPLATE_NAME: &str = "review-import";
pub(crate) const REVIEW_JUDGMENT_TEMPLATE_NAME: &str = "review-judgment";
pub(crate) const REVIEW_REPAIR_TEMPLATE_NAME: &str = "review-repair";
pub(crate) const REVIEW_PUBLICATION_TEMPLATE_NAME: &str = "review-publication";
pub(crate) const REVIEW_CONCERNS: [(&str, &str); 5] = [
    ("correctness", "review-concern-correctness"),
    (
        "interface-and-type-design",
        "review-concern-interface-and-type-design",
    ),
    ("test-quality", "review-concern-test-quality"),
    ("security", "review-concern-security"),
    (
        "documentation-code-drift",
        "review-concern-documentation-code-drift",
    ),
];

/// One immutable template resolved completely at daemon startup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSessionTemplate {
    version: SessionTemplateVersion,
    provenance: SessionTemplateProvenance,
    defaults: SessionConfigurationDefaults,
    repo_watch_contexts: Box<[RepoWatchDispatchContextShape]>,
}

impl ResolvedSessionTemplate {
    /// Returns the operator-assigned bundle version.
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

    /// Returns the explicitly accepted repository-watch dispatch shapes.
    pub fn repo_watch_contexts(&self) -> &[RepoWatchDispatchContextShape] {
        &self.repo_watch_contexts
    }
}

/// Labeled non-concern template selection supplied by a review start request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewStageTemplateSelection {
    /// Import-stage template name.
    pub import: SessionTemplateName,
    /// Judgment-stage template name.
    pub judgment: SessionTemplateName,
    /// Repair-stage template name.
    pub repair: SessionTemplateName,
    /// Publication-stage template name.
    pub publication: SessionTemplateName,
}

/// One ordered concern key and its requested session template.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewConcernTemplateSelection {
    /// Closed concern key.
    pub key: ReviewKey,
    /// Session template selected for the concern.
    pub template: SessionTemplateName,
}

/// Complete client selection that must equal the configured review library.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewLibrarySelection {
    /// Exact configured concern-set version.
    pub concern_set_version: ReviewKey,
    /// Complete labeled non-concern stage selection.
    pub stages: ReviewStageTemplateSelection,
    /// Complete ordered concern and template selection.
    pub concerns: Vec<ReviewConcernTemplateSelection>,
}

/// Validated process-lifetime session templates ordered by name.
#[derive(Clone, Debug, Default)]
pub struct SessionTemplateConfiguration {
    templates: BTreeMap<SessionTemplateName, ResolvedSessionTemplate>,
    review_library: Option<ResolvedReviewLibrary>,
}

impl SessionTemplateConfiguration {
    /// Reads and resolves a complete version-one catalog and every prompt file.
    pub fn read<F>(
        path: &Path,
        home: F,
        models: &HubModelConfiguration,
    ) -> Result<Self, SessionTemplateConfigurationError>
    where
        F: Fn() -> Option<PathBuf>,
    {
        let content =
            fs::read_to_string(path).map_err(|_| SessionTemplateConfigurationError::ReadCatalog)?;
        Self::parse_at_with_home(&content, path, &home, models)
    }

    #[cfg(test)]
    fn parse_at(
        content: &str,
        path: &Path,
        home: Option<&Path>,
        models: &HubModelConfiguration,
    ) -> Result<Self, SessionTemplateConfigurationError> {
        let home = home.map(Path::to_path_buf);
        Self::parse_at_with_home(content, path, &|| home.clone(), models)
    }

    fn parse_at_with_home(
        content: &str,
        path: &Path,
        home: &dyn Fn() -> Option<PathBuf>,
        models: &HubModelConfiguration,
    ) -> Result<Self, SessionTemplateConfigurationError> {
        let document = DocumentMut::from_str(content)
            .map_err(|_| SessionTemplateConfigurationError::InvalidDocument)?;
        reject_unknown_fields(
            document.as_table(),
            &["version", "templates", "review_library"],
        )?;
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
        let review_library = document
            .get("review_library")
            .map(|item| {
                item.as_table()
                    .ok_or(SessionTemplateConfigurationError::InvalidReviewLibrary)
                    .and_then(|table| parse_review_library(table, models, &mut templates))
            })
            .transpose()?;
        Ok(Self {
            templates,
            review_library,
        })
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

    /// Returns every nonempty repository-watch context declaration by template name.
    pub fn repo_watch_context_declarations(
        &self,
    ) -> Result<Vec<RepoWatchTemplateContextDeclaration>, SessionTemplateConfigurationError> {
        self.templates
            .values()
            .filter(|template| !template.repo_watch_contexts.is_empty())
            .map(|template| {
                RepoWatchTemplateContextDeclaration::try_new(
                    template.provenance().name().clone(),
                    template.repo_watch_contexts.to_vec(),
                )
                .map_err(|_| SessionTemplateConfigurationError::InvalidRepoWatchContexts)
            })
            .collect()
    }

    /// Resolves an attempt only when the complete client selection matches the library.
    pub fn resolve_review_attempt(
        &self,
        id: ReviewOrchestrationAttemptId,
        target: ReviewTargetId,
        selection: &ReviewLibrarySelection,
    ) -> Result<ReviewOrchestrationAttempt, ReviewLibraryResolutionError> {
        let library = self
            .review_library
            .as_ref()
            .ok_or(ReviewLibraryResolutionError::LibraryUnavailable)?;
        if &library.selection != selection {
            return Err(ReviewLibraryResolutionError::SelectionMismatch);
        }
        library
            .resolve_attempt(id, target)
            .map_err(ReviewLibraryResolutionError::InvalidAttempt)
    }

    #[cfg(test)]
    fn configured_review_selection(&self) -> Option<&ReviewLibrarySelection> {
        self.review_library
            .as_ref()
            .map(|library| &library.selection)
    }
}

impl RepoWatchTemplateResolver for SessionTemplateConfiguration {
    fn resolve_repo_watch_template(
        &self,
        name: &SessionTemplateName,
    ) -> Option<RepoWatchResolvedTemplate> {
        self.resolve(name).map(|template| {
            RepoWatchResolvedTemplate::new(
                template.provenance().clone(),
                template.defaults().clone(),
            )
        })
    }
}

#[derive(Clone, Debug)]
struct ResolvedReviewLibrary {
    selection: ReviewLibrarySelection,
    concern_set_version: ReviewKey,
    stage_templates: ReviewStageTemplateDigests,
    concerns: Vec<ReviewConcernSpec>,
}

/// Why a requested review-library selection cannot resolve.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewLibraryResolutionError {
    LibraryUnavailable,
    SelectionMismatch,
    InvalidAttempt(ReviewOrchestrationAttemptError),
}

impl fmt::Display for ReviewLibraryResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::LibraryUnavailable => "review library is not configured",
            Self::SelectionMismatch => "review library selection does not match configuration",
            Self::InvalidAttempt(_) => "review orchestration attempt is invalid",
        })
    }
}

impl Error for ReviewLibraryResolutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidAttempt(error) => Some(error),
            Self::LibraryUnavailable | Self::SelectionMismatch => None,
        }
    }
}

impl ResolvedReviewLibrary {
    fn resolve_attempt(
        &self,
        id: ReviewOrchestrationAttemptId,
        target: ReviewTargetId,
    ) -> Result<ReviewOrchestrationAttempt, ReviewOrchestrationAttemptError> {
        ReviewOrchestrationAttempt::try_new(
            id,
            target,
            ReviewPolicy::version_one(),
            self.concern_set_version.clone(),
            self.stage_templates,
            self.concerns.clone(),
        )
    }
}

fn parse_review_library(
    table: &Table,
    models: &HubModelConfiguration,
    templates: &mut BTreeMap<SessionTemplateName, ResolvedSessionTemplate>,
) -> Result<ResolvedReviewLibrary, SessionTemplateConfigurationError> {
    reject_unknown_fields(
        table,
        &[
            "source_version",
            "concern_set_version",
            "model",
            "alias",
            "dangerous_tool_auto_approval",
            "shared_header",
            "import_body",
            "judgment_body",
            "repair_body",
            "publication_body",
            "concerns",
        ],
    )?;
    let source_version = table
        .get("source_version")
        .and_then(|item| item.as_integer())
        .and_then(|value| u64::try_from(value).ok())
        .and_then(SessionTemplateVersion::try_from_u64)
        .ok_or(SessionTemplateConfigurationError::InvalidVersion)?;
    let concern_set_version =
        ReviewKey::try_new(required_string(table, "concern_set_version")?.to_owned())
            .map_err(|_| SessionTemplateConfigurationError::InvalidReviewKey)?;
    let model = parse_model_selection(table, models)?;
    let model_settings = models
        .validate_session_model_settings(model, ModelSettingsOverlay::inherit_all())
        .and_then(Result::ok)
        .ok_or(SessionTemplateConfigurationError::InvalidModelSettings)?;
    let approval = parse_approval(table)?;
    let shared_header = required_nonempty_string(table, "shared_header")?;
    let source = ReviewTemplateSource {
        version: source_version,
        model,
        model_settings,
        approval,
        shared_header,
    };
    let concerns = table
        .get("concerns")
        .and_then(|item| item.as_table())
        .ok_or(SessionTemplateConfigurationError::InvalidConcernInventory)?;
    reject_unknown_fields(
        concerns,
        &[
            "correctness",
            "interface-and-type-design",
            "test-quality",
            "security",
            "documentation-code-drift",
        ],
    )?;

    let import = insert_review_template(
        "import",
        REVIEW_IMPORT_TEMPLATE_NAME,
        required_nonempty_string(table, "import_body")?,
        &source,
        templates,
    )?;
    let judgment = insert_review_template(
        "judgment",
        REVIEW_JUDGMENT_TEMPLATE_NAME,
        required_nonempty_string(table, "judgment_body")?,
        &source,
        templates,
    )?;
    let repair = insert_review_template(
        "repair",
        REVIEW_REPAIR_TEMPLATE_NAME,
        required_nonempty_string(table, "repair_body")?,
        &source,
        templates,
    )?;
    let publication = insert_review_template(
        "publication",
        REVIEW_PUBLICATION_TEMPLATE_NAME,
        required_nonempty_string(table, "publication_body")?,
        &source,
        templates,
    )?;
    let concern_specs = REVIEW_CONCERNS
        .iter()
        .map(|(key, template_name)| {
            let body = required_concern_body(concerns, key)?;
            let digest = insert_review_template(key, template_name, body, &source, templates)?;
            let key = ReviewKey::try_new((*key).to_owned())
                .map_err(|_| SessionTemplateConfigurationError::InvalidReviewKey)?;
            Ok(ReviewConcernSpec::new(key, digest))
        })
        .collect::<Result<Vec<_>, SessionTemplateConfigurationError>>()?;

    let selection = ReviewLibrarySelection {
        concern_set_version: concern_set_version.clone(),
        stages: ReviewStageTemplateSelection {
            import: review_template_name(REVIEW_IMPORT_TEMPLATE_NAME)?,
            judgment: review_template_name(REVIEW_JUDGMENT_TEMPLATE_NAME)?,
            repair: review_template_name(REVIEW_REPAIR_TEMPLATE_NAME)?,
            publication: review_template_name(REVIEW_PUBLICATION_TEMPLATE_NAME)?,
        },
        concerns: REVIEW_CONCERNS
            .iter()
            .map(|(key, template)| {
                Ok(ReviewConcernTemplateSelection {
                    key: ReviewKey::try_new((*key).to_owned())
                        .map_err(|_| SessionTemplateConfigurationError::InvalidReviewKey)?,
                    template: review_template_name(template)?,
                })
            })
            .collect::<Result<Vec<_>, SessionTemplateConfigurationError>>()?,
    };
    Ok(ResolvedReviewLibrary {
        selection,
        concern_set_version,
        stage_templates: ReviewStageTemplateDigests::new(import, judgment, repair, publication),
        concerns: concern_specs,
    })
}

struct ReviewTemplateSource<'a> {
    version: SessionTemplateVersion,
    model: ModelSelectionRequest,
    model_settings: ValidatedModelSettings,
    approval: DangerousToolAutoApproval,
    shared_header: &'a str,
}

fn review_template_name(
    value: &str,
) -> Result<SessionTemplateName, SessionTemplateConfigurationError> {
    SessionTemplateName::try_new(value.to_owned())
        .map_err(|_| SessionTemplateConfigurationError::InvalidName)
}

fn insert_review_template(
    review_key: &str,
    name: &str,
    body: &str,
    source: &ReviewTemplateSource<'_>,
    templates: &mut BTreeMap<SessionTemplateName, ResolvedSessionTemplate>,
) -> Result<ReviewTemplateDigest, SessionTemplateConfigurationError> {
    let name = review_template_name(name)?;
    if templates.contains_key(&name) {
        return Err(SessionTemplateConfigurationError::ReservedName);
    }
    let mut prompt = String::with_capacity(source.shared_header.len() + 2 + body.len());
    prompt.push_str(source.shared_header);
    prompt.push_str("\n\n");
    prompt.push_str(body);
    let prompt = SessionSystemPrompt::try_new(prompt)
        .map_err(|_| SessionTemplateConfigurationError::InvalidPrompt)?;
    let defaults = SessionConfigurationDefaults::complete_with_model_settings(
        source.model,
        source.approval,
        Some(prompt),
        source.model_settings,
    )
    .ok_or(SessionTemplateConfigurationError::InvalidModelSettings)?;
    let digest = SessionTemplateContentDigest::derive(source.version, &defaults)
        .ok_or(SessionTemplateConfigurationError::MissingPrompt)?;
    templates.insert(
        name.clone(),
        ResolvedSessionTemplate {
            version: source.version,
            provenance: SessionTemplateProvenance::new(name, digest),
            defaults,
            repo_watch_contexts: Box::new([]),
        },
    );
    Ok(derive_review_template_digest(
        review_key, body, source, digest,
    ))
}

fn derive_review_template_digest(
    review_key: &str,
    body: &str,
    source: &ReviewTemplateSource<'_>,
    content_digest: SessionTemplateContentDigest,
) -> ReviewTemplateDigest {
    let shared_header_digest = Sha256::digest(source.shared_header.as_bytes());
    let body_digest = Sha256::digest(body.as_bytes());
    let mut digest = Sha256::new();
    update_digest_frame(
        &mut digest,
        b"signalbox/review-template/orchestration-digest/v2",
    );
    update_digest_frame(&mut digest, review_key.as_bytes());
    update_digest_frame(&mut digest, &source.version.as_u64().to_be_bytes());
    match source.model {
        ModelSelectionRequest::Direct(selection) => {
            update_digest_frame(&mut digest, b"direct");
            update_digest_frame(&mut digest, selection.as_uuid().as_bytes());
        }
        ModelSelectionRequest::Alias(alias) => {
            update_digest_frame(&mut digest, b"alias");
            update_digest_frame(&mut digest, alias.as_uuid().as_bytes());
        }
    }
    let approval = match source.approval {
        DangerousToolAutoApproval::Disabled => b"disabled".as_slice(),
        DangerousToolAutoApproval::ApproveAll => b"approve_all".as_slice(),
    };
    update_digest_frame(&mut digest, approval);
    update_digest_frame(&mut digest, &shared_header_digest);
    update_digest_frame(&mut digest, &body_digest);
    update_digest_frame(&mut digest, content_digest.as_bytes());
    ReviewTemplateDigest::new(digest.finalize().into())
}

fn update_digest_frame(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

fn required_nonempty_string<'a>(
    table: &'a Table,
    key: &str,
) -> Result<&'a str, SessionTemplateConfigurationError> {
    let value = required_string(table, key)?;
    if value.is_empty() {
        Err(SessionTemplateConfigurationError::InvalidField)
    } else {
        Ok(value)
    }
}

fn required_concern_body<'a>(
    table: &'a Table,
    key: &str,
) -> Result<&'a str, SessionTemplateConfigurationError> {
    table
        .get(key)
        .and_then(|item| item.as_str())
        .filter(|value| !value.is_empty())
        .ok_or(SessionTemplateConfigurationError::InvalidConcernInventory)
}

fn parse_model_selection(
    table: &Table,
    models: &HubModelConfiguration,
) -> Result<ModelSelectionRequest, SessionTemplateConfigurationError> {
    let direct = optional_uuid(table, "model")?;
    let alias = optional_uuid(table, "alias")?;
    match (direct, alias) {
        (Some(_), Some(_)) => Err(SessionTemplateConfigurationError::ConflictingModelSelection),
        (None, None) => Err(SessionTemplateConfigurationError::MissingModelSelection),
        (Some(value), None) => {
            let selection = DirectModelSelection::from_uuid(value);
            if models.contains_selection(selection) {
                Ok(ModelSelectionRequest::Direct(selection))
            } else {
                Err(SessionTemplateConfigurationError::UnknownModelSelection)
            }
        }
        (None, Some(value)) => {
            let alias = ModelAlias::from_uuid(value);
            if models.resolve_alias(alias).is_some() {
                Ok(ModelSelectionRequest::Alias(alias))
            } else {
                Err(SessionTemplateConfigurationError::UnknownModelSelection)
            }
        }
    }
}

fn parse_approval(
    table: &Table,
) -> Result<DangerousToolAutoApproval, SessionTemplateConfigurationError> {
    match table
        .get("dangerous_tool_auto_approval")
        .and_then(|item| item.as_bool())
    {
        Some(true) => Ok(DangerousToolAutoApproval::ApproveAll),
        Some(false) => Ok(DangerousToolAutoApproval::Disabled),
        None => Err(SessionTemplateConfigurationError::InvalidApproval),
    }
}

fn parse_template(
    table: &Table,
    catalog_path: &Path,
    home: &dyn Fn() -> Option<PathBuf>,
    models: &HubModelConfiguration,
) -> Result<ResolvedSessionTemplate, SessionTemplateConfigurationError> {
    let max_system_prompt_utf8_bytes = models
        .numeric_bounds()
        .integer("max_system_prompt_utf8_bytes")
        .flatten()
        .map(usize::try_from)
        .transpose()
        .map_err(|_| SessionTemplateConfigurationError::InvalidPrompt)?;
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
            "repo_watch_contexts",
        ],
    )?;
    let name = SessionTemplateName::try_new(required_string(table, "name")?.to_owned())
        .map_err(|_| SessionTemplateConfigurationError::InvalidName)?;
    if is_reserved_review_template_name(&name) {
        return Err(SessionTemplateConfigurationError::ReservedName);
    }
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
            read_prompt_file(&prompt_path, max_system_prompt_utf8_bytes)?
        }
    };
    if max_system_prompt_utf8_bytes.is_some_and(|limit| prompt.len() > limit) {
        return Err(SessionTemplateConfigurationError::InvalidPrompt);
    }
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
    let repo_watch_contexts = parse_repo_watch_contexts(table)?;
    let model_settings = models
        .validate_session_model_settings(model, ModelSettingsOverlay::inherit_all())
        .and_then(Result::ok)
        .ok_or(SessionTemplateConfigurationError::InvalidModelSettings)?;
    let defaults = SessionConfigurationDefaults::complete_with_model_settings(
        model,
        dangerous_tool_auto_approval,
        Some(prompt),
        model_settings,
    )
    .ok_or(SessionTemplateConfigurationError::InvalidModelSettings)?;
    let digest = SessionTemplateContentDigest::derive(version, &defaults)
        .ok_or(SessionTemplateConfigurationError::MissingPrompt)?;
    Ok(ResolvedSessionTemplate {
        version,
        provenance: SessionTemplateProvenance::new(name, digest),
        defaults,
        repo_watch_contexts: repo_watch_contexts.into_boxed_slice(),
    })
}

fn parse_repo_watch_contexts(
    table: &Table,
) -> Result<Vec<RepoWatchDispatchContextShape>, SessionTemplateConfigurationError> {
    let Some(item) = table.get("repo_watch_contexts") else {
        return Ok(Vec::new());
    };
    let values = item
        .as_array()
        .ok_or(SessionTemplateConfigurationError::InvalidRepoWatchContexts)?;
    if values.is_empty() {
        return Err(SessionTemplateConfigurationError::InvalidRepoWatchContexts);
    }
    let mut contexts = Vec::with_capacity(values.len());
    for value in values {
        let context = match value.as_str() {
            Some("pull_request") => RepoWatchDispatchContextShape::PullRequest,
            Some("branch") => RepoWatchDispatchContextShape::Branch,
            _ => return Err(SessionTemplateConfigurationError::InvalidRepoWatchContexts),
        };
        if contexts.contains(&context) {
            return Err(SessionTemplateConfigurationError::InvalidRepoWatchContexts);
        }
        contexts.push(context);
    }
    contexts.sort();
    Ok(contexts)
}

fn is_reserved_review_template_name(name: &SessionTemplateName) -> bool {
    name.as_str() == REVIEW_IMPORT_TEMPLATE_NAME
        || name.as_str() == REVIEW_JUDGMENT_TEMPLATE_NAME
        || name.as_str() == REVIEW_REPAIR_TEMPLATE_NAME
        || name.as_str() == REVIEW_PUBLICATION_TEMPLATE_NAME
        || REVIEW_CONCERNS
            .iter()
            .any(|(_, template_name)| name.as_str() == *template_name)
}

fn read_prompt_file(
    path: &Path,
    max_utf8_bytes: Option<usize>,
) -> Result<String, SessionTemplateConfigurationError> {
    let file = open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(fs::File::from)
    .map_err(|_| SessionTemplateConfigurationError::ReadPrompt)?;
    let metadata = file
        .metadata()
        .map_err(|_| SessionTemplateConfigurationError::ReadPrompt)?;
    if !metadata.is_file() {
        return Err(SessionTemplateConfigurationError::ReadPrompt);
    }
    let mut bytes = Vec::new();
    match max_utf8_bytes {
        Some(maximum_bytes) => {
            let maximum_bytes = u64::try_from(maximum_bytes)
                .map_err(|_| SessionTemplateConfigurationError::InvalidPrompt)?;
            if metadata.len() > maximum_bytes {
                return Err(SessionTemplateConfigurationError::InvalidPrompt);
            }
            let read_limit = maximum_bytes
                .checked_add(1)
                .ok_or(SessionTemplateConfigurationError::InvalidPrompt)?;
            file.take(read_limit)
                .read_to_end(&mut bytes)
                .map_err(|_| SessionTemplateConfigurationError::ReadPrompt)?;
        }
        None => {
            file.take(u64::MAX)
                .read_to_end(&mut bytes)
                .map_err(|_| SessionTemplateConfigurationError::ReadPrompt)?;
        }
    }
    if max_utf8_bytes.is_some_and(|limit| bytes.len() > limit) {
        return Err(SessionTemplateConfigurationError::InvalidPrompt);
    }
    String::from_utf8(bytes).map_err(|_| SessionTemplateConfigurationError::ReadPrompt)
}

fn resolve_prompt_path(
    reference: &str,
    catalog_path: &Path,
    home: &dyn Fn() -> Option<PathBuf>,
) -> Result<PathBuf, SessionTemplateConfigurationError> {
    if reference.is_empty()
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
        let home = home().ok_or(SessionTemplateConfigurationError::MissingHome)?;
        if !home.is_absolute() {
            return Err(SessionTemplateConfigurationError::InvalidHome);
        }
        return Ok(home.join(suffix));
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
            let identity = Uuid::parse_str(value)
                .map_err(|_| SessionTemplateConfigurationError::InvalidIdentity)?;
            if identity.hyphenated().to_string() != value {
                return Err(SessionTemplateConfigurationError::InvalidIdentity);
            }
            Ok(identity)
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
    InvalidReviewLibrary,
    UnknownField,
    InvalidField,
    InvalidName,
    DuplicateName,
    ReservedName,
    InvalidVersion,
    InvalidIdentity,
    MissingModelSelection,
    ConflictingModelSelection,
    UnknownModelSelection,
    InvalidModelSettings,
    MissingPrompt,
    ConflictingPrompt,
    InvalidPromptPath,
    MissingHome,
    InvalidHome,
    ReadPrompt,
    InvalidPrompt,
    InvalidApproval,
    InvalidReviewKey,
    InvalidConcernInventory,
    InvalidRepoWatchContexts,
}

impl fmt::Display for SessionTemplateConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ReadCatalog => "session-template configuration file could not be read",
            Self::InvalidDocument => "session-template configuration is not valid TOML",
            Self::UnsupportedVersion => "session-template configuration version is unsupported",
            Self::InvalidTemplates => "session templates are not an array of tables",
            Self::InvalidReviewLibrary => "review library is not a table",
            Self::UnknownField => "session-template configuration contains an unknown field",
            Self::InvalidField => "session-template configuration has a missing or mistyped field",
            Self::InvalidName => "session-template configuration contains an invalid name",
            Self::DuplicateName => "session-template configuration repeats a name",
            Self::ReservedName => "session-template configuration uses a reserved review name",
            Self::InvalidVersion => "session-template configuration contains an invalid version",
            Self::InvalidIdentity => "session-template configuration contains an invalid identity",
            Self::MissingModelSelection => "session template has no model selection",
            Self::ConflictingModelSelection => "session template has multiple model selections",
            Self::UnknownModelSelection => "session template names an unknown model selection",
            Self::InvalidModelSettings => {
                "session template model settings are invalid for its model selection"
            }
            Self::MissingPrompt => "session template has no system prompt",
            Self::ConflictingPrompt => "session template has multiple system prompts",
            Self::InvalidPromptPath => "session template contains an invalid prompt path",
            Self::MissingHome => "session template prompt requires a missing home directory",
            Self::InvalidHome => "session template prompt requires an absolute home directory",
            Self::ReadPrompt => "session template prompt file could not be read as UTF-8",
            Self::InvalidPrompt => "session template contains an invalid system prompt",
            Self::InvalidApproval => "session template has a missing or mistyped approval posture",
            Self::InvalidReviewKey => "review library contains an invalid key",
            Self::InvalidConcernInventory => "review library concern inventory is incomplete",
            Self::InvalidRepoWatchContexts => {
                "session template has an invalid repository-watch context declaration"
            }
        })
    }
}

impl Error for SessionTemplateConfigurationError {}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use signalbox_application::ReviewOrchestrationAttemptId;
    use signalbox_domain::{
        DangerousToolAutoApproval, ModelSelectionRequest, ModelSettingSource, ReasoningLevel,
        RepoWatchDispatchContextShape, ReviewTargetId, SessionTemplateName,
    };

    use super::{
        REVIEW_CONCERNS, ReviewLibraryResolutionError, SessionTemplateConfiguration,
        SessionTemplateConfigurationError,
    };
    use crate::HubModelConfiguration;

    const SELECTION_ID: &str = "10000000-0000-4000-8000-000000000001";
    const TARGET_ID: &str = "20000000-0000-4000-8000-000000000002";
    const ALIAS_ID: &str = "30000000-0000-4000-8000-000000000003";
    const TEMPLATE_NAME: &str = "reviewer";
    const TEMPLATE_VERSION: u64 = 7;
    const REVIEW_SOURCE_VERSION: u64 = 3;
    const REVIEW_CONCERN_SET_VERSION: &str = "initial-v1";
    const INLINE_PROMPT: &str = "Review the change and report concrete findings.";
    const EXPECTED_TEMPLATE_DIGEST: [u8; 32] = [
        0x88, 0xde, 0x5b, 0xe7, 0x9c, 0x61, 0x30, 0x05, 0x8e, 0x68, 0x54, 0x1d, 0x50, 0x8a, 0x2c,
        0xea, 0xbe, 0x99, 0xe2, 0x53, 0x31, 0xbd, 0x24, 0xa9, 0xfd, 0xc4, 0xa9, 0xe3, 0x4d, 0x34,
        0xd8, 0xba,
    ];

    fn models() -> HubModelConfiguration {
        HubModelConfiguration::parse_test_fixture(&format!(
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
prompt = "Summarize the prior conversation faithfully for continuation."

[[models]]
selection_id = "{SELECTION_ID}"
target_id = "{TARGET_ID}"
model_family = "anthropic"
provider_model = "synthetic-model"
max_output_tokens = 1024
context_window_tokens = 200000

[[aliases]]
alias_id = "{ALIAS_ID}"
selection_id = "{SELECTION_ID}"
"#,
        ))
        .expect("synthetic model fixture is valid")
    }

    struct GlobalReasoningModels {
        configuration: HubModelConfiguration,
        expected_reasoning_level: ReasoningLevel,
        expected_reasoning_source: ModelSettingSource,
    }

    impl GlobalReasoningModels {
        const fn configuration(&self) -> &HubModelConfiguration {
            &self.configuration
        }

        const fn expected_reasoning_level(&self) -> ReasoningLevel {
            self.expected_reasoning_level
        }

        const fn expected_reasoning_source(&self) -> ModelSettingSource {
            self.expected_reasoning_source
        }
    }

    fn models_with_global_reasoning() -> GlobalReasoningModels {
        let configuration = HubModelConfiguration::parse_test_fixture(&format!(
            r#"
version = 1

[model_settings]
reasoning_level = "low"

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
prompt = "Summarize the prior conversation faithfully for continuation."

[[models]]
selection_id = "{SELECTION_ID}"
target_id = "{TARGET_ID}"
model_family = "anthropic"
provider_model = "synthetic-model"
max_output_tokens = 1024
context_window_tokens = 200000
reasoning_levels = ["low"]

[[aliases]]
alias_id = "{ALIAS_ID}"
selection_id = "{SELECTION_ID}"
"#,
        ))
        .expect("synthetic lower-layer model fixture is valid");
        GlobalReasoningModels {
            configuration,
            expected_reasoning_level: ReasoningLevel::Low,
            expected_reasoning_source: ModelSettingSource::GlobalDefault,
        }
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

    fn review_catalog(extra: &str) -> String {
        format!(
            r#"
version = 1

[review_library]
source_version = {REVIEW_SOURCE_VERSION}
concern_set_version = "{REVIEW_CONCERN_SET_VERSION}"
alias = "{ALIAS_ID}"
dangerous_tool_auto_approval = false
shared_header = "Shared header."
import_body = "Import exact change evidence."
judgment_body = "Judge the complete finding set."
repair_body = "Repair accepted findings."
publication_body = "Publish only the reserved result."
{extra}

[review_library.concerns]
correctness = "Find behavioral defects."
interface-and-type-design = "Find invalid interface states."
test-quality = "Find material test gaps."
security = "Find security boundary failures."
documentation-code-drift = "Find documentation drift."
"#,
        )
    }

    #[test]
    fn inline_template_resolves_a_complete_digest_bound_bundle() {
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
        assert_eq!(
            template.provenance().content_digest().as_bytes(),
            &EXPECTED_TEMPLATE_DIGEST
        );
    }

    #[test]
    fn ordinary_template_declares_repository_watch_context_shapes() {
        let configuration = SessionTemplateConfiguration::parse_at(
            &inline_catalog("repo_watch_contexts = [\"branch\", \"pull_request\"]"),
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        )
        .expect("repository-watch context declaration is valid");
        let name = SessionTemplateName::try_new(String::from(TEMPLATE_NAME))
            .expect("template fixture name is valid");
        let template = configuration
            .resolve(&name)
            .expect("configured template resolves");

        assert_eq!(
            template.repo_watch_contexts(),
            [
                RepoWatchDispatchContextShape::PullRequest,
                RepoWatchDispatchContextShape::Branch,
            ]
        );
        assert_eq!(
            configuration
                .repo_watch_context_declarations()
                .expect("configured declarations remain valid")
                .len(),
            1
        );
    }

    #[test]
    fn ordinary_template_rejects_an_empty_repository_watch_context_declaration() {
        let result = SessionTemplateConfiguration::parse_at(
            &inline_catalog("repo_watch_contexts = []"),
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        );

        assert!(matches!(
            result,
            Err(SessionTemplateConfigurationError::InvalidRepoWatchContexts)
        ));
    }

    #[test]
    fn template_defaults_copy_the_selected_models_lower_settings_layers() {
        let reasoning_models = models_with_global_reasoning();
        let configuration = SessionTemplateConfiguration::parse_at(
            &inline_catalog(""),
            Path::new("deployment/session-templates.toml"),
            None,
            reasoning_models.configuration(),
        )
        .expect("valid settings-aware template catalog loads");
        let name = SessionTemplateName::try_new(TEMPLATE_NAME.to_owned())
            .expect("fixture template name is admitted");
        let settings = configuration
            .resolve(&name)
            .expect("configured template resolves")
            .defaults()
            .model_settings();

        assert_eq!(
            settings.effective().reasoning_level(),
            Some(reasoning_models.expected_reasoning_level())
        );
        assert_eq!(
            settings.resolved().reasoning_source(),
            Some(reasoning_models.expected_reasoning_source())
        );
    }

    /// immutable template provenance binds the copied settings
    /// snapshot as well as the model, approval posture, and prompt.
    #[test]
    fn template_content_digest_commits_copied_model_settings() {
        let reasoning_models = models_with_global_reasoning();
        let provider_defaults = SessionTemplateConfiguration::parse_at(
            &inline_catalog(""),
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        )
        .expect("provider-default template catalog loads");
        let configured_reasoning = SessionTemplateConfiguration::parse_at(
            &inline_catalog(""),
            Path::new("deployment/session-templates.toml"),
            None,
            reasoning_models.configuration(),
        )
        .expect("settings-aware template catalog loads");
        let name = SessionTemplateName::try_new(TEMPLATE_NAME.to_owned())
            .expect("fixture template name is admitted");
        let provider_default_digest = provider_defaults
            .resolve(&name)
            .expect("provider-default template resolves")
            .provenance()
            .content_digest();
        let configured_digest = configured_reasoning
            .resolve(&name)
            .expect("settings-aware template resolves")
            .provenance()
            .content_digest();

        assert_ne!(provider_default_digest, configured_digest);
    }

    #[test]
    fn template_version_uses_the_complete_positive_toml_integer_range() {
        const MAXIMUM_TEMPLATE_VERSION: u64 = i64::MAX as u64;
        let maximum_catalog = inline_catalog("").replace(
            &format!("version = {TEMPLATE_VERSION}"),
            &format!("version = {MAXIMUM_TEMPLATE_VERSION}"),
        );
        let maximum = SessionTemplateConfiguration::parse_at(
            &maximum_catalog,
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        )
        .expect("maximum positive TOML integer version is admitted");
        let name = SessionTemplateName::try_new(TEMPLATE_NAME.to_owned())
            .expect("fixture template name is admitted");
        let zero_catalog =
            inline_catalog("").replace(&format!("version = {TEMPLATE_VERSION}"), "version = 0");

        assert_eq!(
            maximum
                .resolve(&name)
                .expect("maximum-version template resolves")
                .version()
                .as_u64(),
            MAXIMUM_TEMPLATE_VERSION
        );
        assert_eq!(
            SessionTemplateConfiguration::parse_at(
                &zero_catalog,
                Path::new("deployment/session-templates.toml"),
                None,
                &models(),
            )
            .expect_err("zero template version is rejected"),
            SessionTemplateConfigurationError::InvalidVersion
        );
    }

    #[test]
    fn home_prompt_reference_copies_exact_file_content() {
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

    #[cfg(unix)]
    #[test]
    fn unix_relative_prompt_file_accepts_backslash_in_component() {
        let temporary = tempfile::tempdir().expect("temporary deployment root");
        let prompt_path = temporary.path().join(r"review\guide.txt");
        fs::write(&prompt_path, INLINE_PROMPT).expect("synthetic prompt is written");
        let catalog = inline_catalog("").replace(
            &format!("system_prompt = \"{INLINE_PROMPT}\""),
            r"system_prompt_file = 'review\guide.txt'",
        );
        let configuration = SessionTemplateConfiguration::parse_at(
            &catalog,
            &temporary.path().join("session-templates.toml"),
            None,
            &models(),
        )
        .expect("a Unix backslash path component is admitted");
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
    fn oversized_prompt_file_returns_precise_typed_failure() {
        let temporary = tempfile::tempdir().expect("temporary deployment root");
        let prompt_path = temporary.path().join("oversized.txt");
        let models = models();
        let configured_limit = models
            .numeric_bounds()
            .integer("max_system_prompt_utf8_bytes")
            .expect("the required prompt bound is present")
            .expect("the fixture prompt bound is finite");
        fs::write(&prompt_path, "x".repeat(configured_limit as usize + 1))
            .expect("oversized synthetic prompt is written");
        let catalog = inline_catalog("").replace(
            &format!("system_prompt = \"{INLINE_PROMPT}\""),
            "system_prompt_file = \"oversized.txt\"",
        );
        let result = SessionTemplateConfiguration::parse_at(
            &catalog,
            &temporary.path().join("session-templates.toml"),
            None,
            &models,
        );

        assert_eq!(
            result.expect_err("oversized prompt file is rejected"),
            SessionTemplateConfigurationError::InvalidPrompt
        );
    }

    #[test]
    fn non_regular_prompt_source_returns_precise_typed_failure() {
        let temporary = tempfile::tempdir().expect("temporary deployment root");
        fs::create_dir(temporary.path().join("prompt-directory"))
            .expect("synthetic prompt directory is created");
        let catalog = inline_catalog("").replace(
            &format!("system_prompt = \"{INLINE_PROMPT}\""),
            "system_prompt_file = \"prompt-directory\"",
        );
        let result = SessionTemplateConfiguration::parse_at(
            &catalog,
            &temporary.path().join("session-templates.toml"),
            None,
            &models(),
        );

        assert_eq!(
            result.expect_err("non-regular prompt source is rejected"),
            SessionTemplateConfigurationError::ReadPrompt
        );
    }

    #[test]
    fn invalid_home_values_return_precise_typed_failures() {
        let catalog = inline_catalog("").replace(
            &format!("system_prompt = \"{INLINE_PROMPT}\""),
            "system_prompt_file = \"$HOME/prompts/reviewer.txt\"",
        );
        let empty_home = SessionTemplateConfiguration::parse_at(
            &catalog,
            Path::new("deployment/session-templates.toml"),
            Some(Path::new("")),
            &models(),
        );
        let relative_home = SessionTemplateConfiguration::parse_at(
            &catalog,
            Path::new("deployment/session-templates.toml"),
            Some(Path::new("relative-home")),
            &models(),
        );

        assert_eq!(
            empty_home.expect_err("empty HOME is rejected"),
            SessionTemplateConfigurationError::InvalidHome
        );
        assert_eq!(
            relative_home.expect_err("relative HOME is rejected"),
            SessionTemplateConfigurationError::InvalidHome
        );
    }

    #[test]
    fn unknown_catalog_field_returns_precise_typed_failure() {
        let result = SessionTemplateConfiguration::parse_at(
            &inline_catalog("unexpected = true"),
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        );

        assert_eq!(
            result.expect_err("unknown template field is rejected"),
            SessionTemplateConfigurationError::UnknownField
        );
    }

    #[test]
    fn conflicting_prompt_sources_return_precise_typed_failure() {
        let result = SessionTemplateConfiguration::parse_at(
            &inline_catalog("system_prompt_file = \"prompt.txt\""),
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        );

        assert_eq!(
            result.expect_err("dual prompt sources are rejected"),
            SessionTemplateConfigurationError::ConflictingPrompt
        );
    }

    #[test]
    fn unknown_model_selection_returns_precise_typed_failure() {
        let unknown_model = inline_catalog("").replace(ALIAS_ID, TARGET_ID);

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
    }

    #[test]
    fn unavailable_home_returns_precise_typed_failure() {
        let missing_home = inline_catalog("").replace(
            &format!("system_prompt = \"{INLINE_PROMPT}\""),
            "system_prompt_file = \"$HOME/prompts/reviewer.txt\"",
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

    #[test]
    fn noncanonical_model_identity_returns_precise_typed_failure() {
        let noncanonical_model = inline_catalog("").replace(
            &format!("alias = \"{ALIAS_ID}\""),
            &format!("model = \"{}\"", SELECTION_ID.replace("-", "")),
        );

        assert_eq!(
            SessionTemplateConfiguration::parse_at(
                &noncanonical_model,
                Path::new("deployment/session-templates.toml"),
                None,
                &models(),
            )
            .expect_err("noncanonical model identity is rejected"),
            SessionTemplateConfigurationError::InvalidIdentity
        );
    }

    #[test]
    fn noncanonical_alias_identity_returns_precise_typed_failure() {
        let unhyphenated_alias = inline_catalog("").replace(ALIAS_ID, &ALIAS_ID.replace("-", ""));

        assert_eq!(
            SessionTemplateConfiguration::parse_at(
                &unhyphenated_alias,
                Path::new("deployment/session-templates.toml"),
                None,
                &models(),
            )
            .expect_err("unhyphenated alias identity is rejected"),
            SessionTemplateConfigurationError::InvalidIdentity
        );
    }

    #[cfg(not(target_vendor = "apple"))]
    #[test]
    fn fifo_prompt_source_is_rejected_without_blocking() {
        let temporary = tempfile::tempdir().expect("temporary deployment root");
        let prompt_path = temporary.path().join("prompt.fifo");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &prompt_path,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .expect("synthetic prompt FIFO is created");
        let catalog = inline_catalog("").replace(
            &format!("system_prompt = \"{INLINE_PROMPT}\""),
            "system_prompt_file = \"prompt.fifo\"",
        );
        let result = SessionTemplateConfiguration::parse_at(
            &catalog,
            &temporary.path().join("session-templates.toml"),
            None,
            &models(),
        );

        assert_eq!(
            result.expect_err("FIFO prompt source is rejected"),
            SessionTemplateConfigurationError::ReadPrompt
        );
    }
    #[test]
    fn review_library_generates_exact_header_separator_and_body_bytes() {
        let configuration = SessionTemplateConfiguration::parse_at(
            &review_catalog(""),
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        )
        .expect("closed review library loads");
        let name = SessionTemplateName::try_new(String::from("review-concern-correctness"))
            .expect("reserved fixture name is valid");
        let template = configuration
            .resolve(&name)
            .expect("generated template resolves");

        assert_eq!(configuration.summaries().len(), REVIEW_CONCERNS.len() + 4);
        assert_eq!(template.version().as_u64(), REVIEW_SOURCE_VERSION);
        assert_eq!(
            template
                .defaults()
                .system_prompt()
                .expect("generated template has a prompt")
                .as_str(),
            "Shared header.\n\nFind behavioral defects."
        );
        assert_eq!(
            template.defaults().dangerous_tool_auto_approval(),
            DangerousToolAutoApproval::Disabled
        );
    }

    #[test]
    fn review_attempt_preserves_closed_concern_order_and_configured_version() {
        let configuration = SessionTemplateConfiguration::parse_at(
            &review_catalog(""),
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        )
        .expect("closed review library loads");
        let selection = configuration
            .configured_review_selection()
            .expect("configured review library has a selection")
            .clone();
        let attempt = configuration
            .resolve_review_attempt(
                ReviewOrchestrationAttemptId::from_uuid(uuid::Uuid::from_u128(41)),
                ReviewTargetId::from_uuid(uuid::Uuid::from_u128(42)),
                &selection,
            )
            .expect("matching selection constructs an attempt");

        assert_eq!(
            attempt.concern_set_version().as_str(),
            REVIEW_CONCERN_SET_VERSION
        );
        assert_eq!(attempt.concerns().len(), selection.concerns.len());
        assert_eq!(attempt.concerns()[0].key(), &selection.concerns[0].key);
        assert_eq!(attempt.concerns()[1].key(), &selection.concerns[1].key);
        assert_eq!(attempt.concerns()[2].key(), &selection.concerns[2].key);
        assert_eq!(attempt.concerns()[3].key(), &selection.concerns[3].key);
        assert_eq!(attempt.concerns()[4].key(), &selection.concerns[4].key);
    }

    #[test]
    fn review_digest_commits_the_stage_or_concern_key() {
        let catalog = review_catalog("").replace(
            "security = \"Find security boundary failures.\"",
            "security = \"Find behavioral defects.\"",
        );
        let configuration = SessionTemplateConfiguration::parse_at(
            &catalog,
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        )
        .expect("same-body concern fixture loads");
        let selection = configuration
            .configured_review_selection()
            .expect("configured review library has a selection")
            .clone();
        let attempt = configuration
            .resolve_review_attempt(
                ReviewOrchestrationAttemptId::from_uuid(uuid::Uuid::from_u128(43)),
                ReviewTargetId::from_uuid(uuid::Uuid::from_u128(44)),
                &selection,
            )
            .expect("matching selection constructs an attempt");
        let correctness = attempt.concerns()[0].template_digest();
        let security = attempt.concerns()[3].template_digest();

        assert_ne!(correctness, security);
    }

    #[test]
    fn review_digest_commits_copied_model_settings() {
        let reasoning_models = models_with_global_reasoning();
        let provider_defaults = SessionTemplateConfiguration::parse_at(
            &review_catalog(""),
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        )
        .expect("provider-default review library loads");
        let configured_reasoning = SessionTemplateConfiguration::parse_at(
            &review_catalog(""),
            Path::new("deployment/session-templates.toml"),
            None,
            reasoning_models.configuration(),
        )
        .expect("settings-aware review library loads");
        let provider_default_selection = provider_defaults
            .configured_review_selection()
            .expect("provider-default review library has a selection")
            .clone();
        let configured_selection = configured_reasoning
            .configured_review_selection()
            .expect("settings-aware review library has a selection")
            .clone();
        let provider_default_attempt = provider_defaults
            .resolve_review_attempt(
                ReviewOrchestrationAttemptId::from_uuid(uuid::Uuid::from_u128(45)),
                ReviewTargetId::from_uuid(uuid::Uuid::from_u128(46)),
                &provider_default_selection,
            )
            .expect("provider-default review attempt resolves");
        let configured_attempt = configured_reasoning
            .resolve_review_attempt(
                ReviewOrchestrationAttemptId::from_uuid(uuid::Uuid::from_u128(47)),
                ReviewTargetId::from_uuid(uuid::Uuid::from_u128(48)),
                &configured_selection,
            )
            .expect("settings-aware review attempt resolves");

        assert_ne!(
            provider_default_attempt.stage_templates().import(),
            configured_attempt.stage_templates().import()
        );
    }

    #[test]
    fn review_library_rejects_an_unknown_member() {
        let result = SessionTemplateConfiguration::parse_at(
            &review_catalog("unexpected = true"),
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        );

        assert_eq!(
            result.expect_err("unknown review member is rejected"),
            SessionTemplateConfigurationError::UnknownField
        );
    }

    #[test]
    fn review_library_rejects_a_missing_concern() {
        let catalog =
            review_catalog("").replace("security = \"Find security boundary failures.\"\n", "");
        let result = SessionTemplateConfiguration::parse_at(
            &catalog,
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        );

        assert_eq!(
            result.expect_err("incomplete concern inventory is rejected"),
            SessionTemplateConfigurationError::InvalidConcernInventory
        );
    }

    #[test]
    fn review_library_rejects_an_unknown_concern_key() {
        let catalog = review_catalog("").replace(
            "security = \"Find security boundary failures.\"",
            "portability = \"Find portability failures.\"",
        );
        let result = SessionTemplateConfiguration::parse_at(
            &catalog,
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        );

        assert_eq!(
            result.expect_err("unknown concern key is rejected"),
            SessionTemplateConfigurationError::UnknownField
        );
    }

    #[test]
    fn ordinary_template_rejects_a_reserved_review_name() {
        let catalog = inline_catalog("").replace(TEMPLATE_NAME, "review-import");
        let result = SessionTemplateConfiguration::parse_at(
            &catalog,
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        );

        assert_eq!(
            result.expect_err("reserved generated name is rejected"),
            SessionTemplateConfigurationError::ReservedName
        );
    }

    #[test]
    fn review_library_rejects_multiple_model_selections() {
        let catalog = review_catalog(&format!("model = \"{SELECTION_ID}\""));
        let result = SessionTemplateConfiguration::parse_at(
            &catalog,
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        );

        assert_eq!(
            result.expect_err("multiple model selections are rejected"),
            SessionTemplateConfigurationError::ConflictingModelSelection
        );
    }

    #[test]
    fn absent_review_library_resolves_no_attempt() {
        let configuration = SessionTemplateConfiguration::parse_at(
            "version = 1",
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        )
        .expect("review library is optional");
        assert_eq!(configuration.configured_review_selection(), None);
    }
    #[test]
    fn review_library_rejects_an_empty_concern_inventory() {
        let catalog = review_catalog("")
            .replace("correctness = \"Find behavioral defects.\"\n", "")
            .replace(
                "interface-and-type-design = \"Find invalid interface states.\"\n",
                "",
            )
            .replace("test-quality = \"Find material test gaps.\"\n", "")
            .replace("security = \"Find security boundary failures.\"\n", "")
            .replace(
                "documentation-code-drift = \"Find documentation drift.\"\n",
                "",
            );
        let result = SessionTemplateConfiguration::parse_at(
            &catalog,
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        );

        assert_eq!(
            result.expect_err("empty concern inventory is rejected"),
            SessionTemplateConfigurationError::InvalidConcernInventory
        );
    }

    #[test]
    fn example_review_library_is_immediately_parseable_with_its_documented_alias() {
        let catalog = include_str!("../../../config/session-templates.example.toml")
            .replace("7fde05bc-b4c3-44f7-8a87-748814c80191", ALIAS_ID)
            .replace("540ce009-c2ec-4a04-b823-c411ea189778", ALIAS_ID);
        let configuration = SessionTemplateConfiguration::parse_at(
            &catalog,
            Path::new("config/session-templates.example.toml"),
            None,
            &models(),
        )
        .expect("example review library is valid");

        assert_eq!(configuration.summaries().len(), REVIEW_CONCERNS.len() + 5);
    }
    #[test]
    fn review_attempt_rejects_a_reordered_concern_selection() {
        let configuration = SessionTemplateConfiguration::parse_at(
            &review_catalog(""),
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        )
        .expect("closed review library loads");
        let mut selection = configuration
            .configured_review_selection()
            .expect("configured review library has a selection")
            .clone();
        selection.concerns.swap(0, 1);
        let result = configuration.resolve_review_attempt(
            ReviewOrchestrationAttemptId::from_uuid(uuid::Uuid::from_u128(47)),
            ReviewTargetId::from_uuid(uuid::Uuid::from_u128(48)),
            &selection,
        );

        assert_eq!(
            result.expect_err("reordered concern selection is rejected"),
            ReviewLibraryResolutionError::SelectionMismatch
        );
    }

    #[test]
    fn review_attempt_rejects_a_selection_when_the_library_is_absent() {
        let configured = SessionTemplateConfiguration::parse_at(
            &review_catalog(""),
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        )
        .expect("closed review library loads");
        let selection = configured
            .configured_review_selection()
            .expect("configured review library has a selection")
            .clone();
        let empty = SessionTemplateConfiguration::parse_at(
            "version = 1",
            Path::new("deployment/session-templates.toml"),
            None,
            &models(),
        )
        .expect("review library is optional");
        let result = empty.resolve_review_attempt(
            ReviewOrchestrationAttemptId::from_uuid(uuid::Uuid::from_u128(49)),
            ReviewTargetId::from_uuid(uuid::Uuid::from_u128(50)),
            &selection,
        );

        assert_eq!(
            result.expect_err("absent review library rejects selection"),
            ReviewLibraryResolutionError::LibraryUnavailable
        );
    }
}
