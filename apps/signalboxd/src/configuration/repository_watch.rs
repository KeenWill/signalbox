use super::{
    credential_files::{credential_file_references_conflict, resolved_credential_file_reference},
    error::HubModelConfigurationError,
    numeric_bounds::NumericBoundsConfiguration,
    toml_scalars::{reject_unknown_fields, required_string},
};
use signalbox_domain::{
    BranchName, CheckConclusion, LabelName, MergeableState, PullRequestNumber,
    RepoWatchAuthorLogin, RepoWatchEventKindNameV1, RepoWatchLabelMatcher,
    RepoWatchLabelMatcherInput, RepoWatchMatcherV1, RepoWatchMatcherV1Input, RepoWatchPattern,
    RepoWatchRule, RepoWatchRuleActionV1, RepoWatchRuleId, RepoWatchRuleVersion,
    RepoWatchSingletonScope, RepositorySlug, SessionTemplateName,
};
use signalbox_model_runtime::CredentialReference;
use std::{
    collections::HashSet,
    fmt,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    num::NonZeroU64,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use toml_edit::{Item, Table};

const MAX_REPOSITORY_WATCH_RULES: usize = 128;
const MAX_REPOSITORY_WATCH_ACTIONS: usize = 32;

const MAX_WATCHED_REPOSITORIES: usize = 128;
const MAX_SIGNAL_REVIEWERS: usize = 128;

/// Loopback-only reference address selected when the webhook listener table
/// omits `bind_address`.
pub const DEFAULT_REPOSITORY_WATCH_WEBHOOK_BIND_ADDRESS: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 3333));

/// One deployment-owned local HTTP listener for authenticated GitHub hooks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryWatchWebhookConfiguration {
    bind_address: SocketAddr,
    path: Arc<str>,
}

impl RepositoryWatchWebhookConfiguration {
    /// Returns the exact local socket address the daemon must bind.
    pub const fn bind_address(&self) -> SocketAddr {
        self.bind_address
    }

    /// Returns the exact absolute local request path the listener admits.
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// One watched repository's authenticated webhook association.
#[derive(Clone, Eq, PartialEq)]
pub struct WatchedRepositoryWebhookConfiguration {
    hook_id: NonZeroU64,
    secret_file: PathBuf,
    mode: RepositoryWatchWebhookMode,
}

impl WatchedRepositoryWebhookConfiguration {
    /// Returns the positive GitHub hook identity selecting this repository.
    pub const fn hook_id(&self) -> NonZeroU64 {
        self.hook_id
    }

    /// Returns the deployment-owned webhook-secret file reference.
    pub fn secret_file(&self) -> &Path {
        &self.secret_file
    }

    /// Returns whether authenticated deliveries only acknowledge or also wake ingestion.
    pub const fn mode(&self) -> RepositoryWatchWebhookMode {
        self.mode
    }
}

impl fmt::Debug for WatchedRepositoryWebhookConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WatchedRepositoryWebhookConfiguration")
            .field("hook_id", &self.hook_id)
            .field("secret_file", &"[REDACTED REFERENCE]")
            .field("mode", &self.mode)
            .finish()
    }
}

/// Per-repository rollout mode for authenticated webhook deliveries.
///
/// Shadow authenticates and acknowledges without waking ingestion. Primary
/// wakes the repository task to fetch a complete provider observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryWatchWebhookMode {
    Shadow,
    Primary,
}

/// One repository-specific polling and credential configuration.
#[derive(Clone, Eq, PartialEq)]
pub struct WatchedRepositoryConfiguration {
    repository: RepositorySlug,
    poll_interval: Duration,
    credential_file: PathBuf,
    webhook: Option<WatchedRepositoryWebhookConfiguration>,
    convergence_pull_requests: Box<[PullRequestNumber]>,
}

impl WatchedRepositoryConfiguration {
    /// Returns the canonical repository identity authorized by this entry.
    pub const fn repository(&self) -> &RepositorySlug {
        &self.repository
    }

    /// Returns the positive start-to-start interval between scheduled polls.
    pub const fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// Returns the deployment-owned credential-file reference.
    pub fn credential_file(&self) -> &Path {
        &self.credential_file
    }

    /// Returns the non-secret request credential reference for this repository.
    pub fn credential_reference(&self) -> CredentialReference {
        CredentialReference::new(format!("repository-watch:{}", self.repository.as_str()))
    }

    /// Returns this repository's authenticated webhook association, if enabled.
    pub const fn webhook(&self) -> Option<&WatchedRepositoryWebhookConfiguration> {
        self.webhook.as_ref()
    }

    /// Returns the explicit operator-owned convergence throttle for this repository.
    pub fn convergence_pull_requests(&self) -> &[PullRequestNumber] {
        &self.convergence_pull_requests
    }

    /// Returns the non-secret reference used to resolve this repository's
    /// webhook secret, if webhook delivery is enabled for it.
    pub fn webhook_secret_reference(&self) -> Option<CredentialReference> {
        self.webhook.as_ref().map(|_| {
            CredentialReference::new(format!(
                "repository-watch-webhook:{}",
                self.repository.as_str()
            ))
        })
    }
}

impl fmt::Debug for WatchedRepositoryConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WatchedRepositoryConfiguration")
            .field("repository", &self.repository)
            .field("poll_interval", &self.poll_interval)
            .field("credential_file", &"[REDACTED REFERENCE]")
            .field("webhook", &self.webhook)
            .field("convergence_pull_requests", &self.convergence_pull_requests)
            .finish()
    }
}

/// Complete optional repository-watch configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryWatchConfiguration {
    enabled: bool,
    signal_reviewers: Box<[RepoWatchAuthorLogin]>,
    repositories: Box<[WatchedRepositoryConfiguration]>,
    rules: Box<[RepoWatchRule]>,
    webhook: Option<RepositoryWatchWebhookConfiguration>,
    webhook_retention: Duration,
    convergence_sweep: Option<ConvergenceSweepConfiguration>,
}

/// Daemon-native convergence sweep policy, enabled only with explicit targets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConvergenceSweepConfiguration {
    template: SessionTemplateName,
    interval: Duration,
    cool_off: Duration,
}

impl ConvergenceSweepConfiguration {
    /// Returns the fenced session template used for review-response work.
    pub const fn template(&self) -> &SessionTemplateName {
        &self.template
    }
    /// Returns the census interval, never above its hard ceiling.
    pub const fn interval(&self) -> Duration {
        self.interval
    }
    /// Returns the per-pull-request dispatch cool-off, never above its hard ceiling.
    pub const fn cool_off(&self) -> Duration {
        self.cool_off
    }
}

impl RepositoryWatchConfiguration {
    pub(crate) const fn webhook_retention(&self) -> Duration {
        self.webhook_retention
    }

    /// Returns whether repository polling, webhook wakes, and dispatch are enabled.
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the exact canonical login set used for reaction ingestion.
    pub fn signal_reviewers(&self) -> &[RepoWatchAuthorLogin] {
        &self.signal_reviewers
    }

    /// Returns every independently credentialed repository task.
    pub fn repositories(&self) -> &[WatchedRepositoryConfiguration] {
        &self.repositories
    }

    /// Returns the validated structured rules in declaration order.
    pub fn rules(&self) -> &[RepoWatchRule] {
        &self.rules
    }

    /// Returns the configured local webhook listener, or absence when webhook
    /// intake is disabled.
    pub const fn webhook(&self) -> Option<&RepositoryWatchWebhookConfiguration> {
        self.webhook.as_ref()
    }

    /// Returns enabled convergence reconciliation policy, if explicitly configured.
    pub const fn convergence_sweep(&self) -> Option<&ConvergenceSweepConfiguration> {
        if self.enabled {
            self.convergence_sweep.as_ref()
        } else {
            None
        }
    }

    /// Validates the convergence template against the immutable session-template catalog.
    pub fn validate_convergence_template<'a>(
        &self,
        templates: impl Iterator<Item = &'a SessionTemplateName>,
    ) -> Result<(), HubModelConfigurationError> {
        let Some(policy) = self.convergence_sweep() else {
            return Ok(());
        };
        if templates.into_iter().any(|name| name == policy.template()) {
            Ok(())
        } else {
            Err(
                HubModelConfigurationError::UnknownConvergenceSweepTemplate {
                    template: policy.template().as_str().to_owned(),
                },
            )
        }
    }
}

pub(super) fn parse_repository_watch_configuration(
    item: &Item,
    numeric_bounds: &NumericBoundsConfiguration,
) -> Result<RepositoryWatchConfiguration, HubModelConfigurationError> {
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    reject_unknown_fields(
        table,
        &[
            "version",
            "enabled",
            "signal_reviewers",
            "repositories",
            "rules",
            "webhook",
            "convergence_sweep",
        ],
    )
    .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    if table.get("version").and_then(Item::as_integer) != Some(1) {
        return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
    }
    let enabled = table
        .get("enabled")
        .map(|value| {
            value
                .as_bool()
                .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        })
        .transpose()?
        .unwrap_or(true);
    let reviewer_values = table
        .get("signal_reviewers")
        .and_then(Item::as_array)
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    if reviewer_values.len() > MAX_SIGNAL_REVIEWERS {
        return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
    }
    let mut signal_reviewers = Vec::with_capacity(reviewer_values.len());
    let mut reviewer_set = HashSet::with_capacity(reviewer_values.len());
    for value in reviewer_values {
        let login = value
            .as_str()
            .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
            .and_then(|value| {
                RepoWatchAuthorLogin::try_new(value.to_owned())
                    .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
            })?;
        if !reviewer_set.insert(login.clone()) {
            return Err(HubModelConfigurationError::DuplicateSignalReviewer);
        }
        signal_reviewers.push(login);
    }
    signal_reviewers.sort();

    let webhook = parse_repository_watch_webhook_configuration(table.get("webhook"))?;
    let convergence_sweep = parse_convergence_sweep_configuration(
        table.get("convergence_sweep"),
        numeric_bounds
            .duration("max_convergence_sweep_interval")
            .flatten(),
        numeric_bounds
            .duration("max_convergence_sweep_cool_off")
            .flatten(),
    )?;

    let repository_tables = table
        .get("repositories")
        .and_then(Item::as_array_of_tables)
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    if repository_tables.is_empty() || repository_tables.len() > MAX_WATCHED_REPOSITORIES {
        return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
    }
    let mut repositories = Vec::with_capacity(repository_tables.len());
    let mut repository_set = HashSet::with_capacity(repository_tables.len());
    let mut credential_file_references: Vec<PathBuf> = Vec::with_capacity(repository_tables.len());
    let mut webhook_hook_ids = HashSet::with_capacity(repository_tables.len());
    let mut webhook_repository_count = 0_usize;
    for repository in repository_tables {
        reject_unknown_fields(
            repository,
            &[
                "repository",
                "poll_interval_seconds",
                "credential_file",
                "webhook_hook_id",
                "webhook_secret_file",
                "webhook_mode",
                "convergence_pull_requests",
            ],
        )
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
        let repository_slug = RepositorySlug::try_new(
            required_string(repository, "repository")
                .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?
                .to_owned(),
        )
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
        if !repository_set.insert(repository_slug.clone()) {
            return Err(HubModelConfigurationError::DuplicateWatchedRepository);
        }
        let interval = repository
            .get("poll_interval_seconds")
            .and_then(Item::as_integer)
            .and_then(|value| u64::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
        let credential_file = PathBuf::from(
            required_string(repository, "credential_file")
                .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?,
        );
        if !credential_file.is_absolute() {
            return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
        }
        if credential_file
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
        }
        let resolved_credential_file = resolved_credential_file_reference(&credential_file)?;
        if credential_file_references.iter().any(|existing| {
            credential_file_references_conflict(existing, &resolved_credential_file)
        }) {
            return Err(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile);
        }
        credential_file_references.push(resolved_credential_file);
        let repository_webhook = match (
            repository.get("webhook_hook_id"),
            repository.get("webhook_secret_file"),
            repository.get("webhook_mode"),
        ) {
            (None, None, None) => None,
            (Some(hook_id), Some(secret_file), mode) => {
                let hook_id = hook_id
                    .as_integer()
                    .and_then(|value| u64::try_from(value).ok())
                    .and_then(NonZeroU64::new)
                    .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
                if !webhook_hook_ids.insert(hook_id) {
                    return Err(HubModelConfigurationError::DuplicateRepositoryWatchWebhookHookId);
                }
                let secret_file = secret_file
                    .as_str()
                    .map(PathBuf::from)
                    .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
                if !secret_file.is_absolute()
                    || secret_file
                        .components()
                        .any(|component| matches!(component, Component::ParentDir))
                {
                    return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
                }
                let resolved_secret_file = resolved_credential_file_reference(&secret_file)?;
                if credential_file_references.iter().any(|existing| {
                    credential_file_references_conflict(existing, &resolved_secret_file)
                }) {
                    return Err(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile);
                }
                credential_file_references.push(resolved_secret_file);
                // Only an absent key defaults. A present item of any other TOML
                // type is malformed configuration rather than an omission, so it
                // is refused instead of silently selecting the shadow rollout
                // mode a deployment did not ask for.
                let mode = match mode {
                    None => RepositoryWatchWebhookMode::Shadow,
                    Some(item) => match item.as_str() {
                        Some("shadow") => RepositoryWatchWebhookMode::Shadow,
                        Some("primary") => RepositoryWatchWebhookMode::Primary,
                        Some(_) | None => {
                            return Err(
                                HubModelConfigurationError::InvalidRepositoryWatchConfiguration,
                            );
                        }
                    },
                };
                webhook_repository_count += 1;
                Some(WatchedRepositoryWebhookConfiguration {
                    hook_id,
                    secret_file,
                    mode,
                })
            }
            (Some(_), None, _) | (None, Some(_), _) | (None, None, Some(_)) => {
                return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
            }
        };
        let convergence_pull_requests =
            parse_convergence_pull_requests(repository.get("convergence_pull_requests"))?;
        repositories.push(WatchedRepositoryConfiguration {
            repository: repository_slug,
            poll_interval: Duration::from_secs(interval),
            credential_file,
            webhook: repository_webhook,
            convergence_pull_requests: convergence_pull_requests.into_boxed_slice(),
        });
    }
    if webhook.is_some() != (webhook_repository_count > 0) {
        return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
    }
    repositories.sort_by(|left, right| left.repository.cmp(&right.repository));
    let rules = parse_repository_watch_rules(table)?;
    let convergence_target_count = repositories
        .iter()
        .map(|repository| repository.convergence_pull_requests.len())
        .sum::<usize>();
    let convergence_target_limit = numeric_bounds
        .integer("max_convergence_sweep_targets")
        .flatten()
        .and_then(|value| usize::try_from(value).ok());
    if convergence_target_limit.is_some_and(|limit| convergence_target_count > limit)
        || (convergence_target_count == 0) != convergence_sweep.is_none()
    {
        return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
    }
    let webhook_retention = numeric_bounds
        .duration("repository_watch_webhook_retention")
        .flatten()
        .ok_or(HubModelConfigurationError::InvalidNumericBound {
            field: "repository_watch_webhook_retention",
        })?;
    Ok(RepositoryWatchConfiguration {
        enabled,
        webhook_retention,
        signal_reviewers: signal_reviewers.into_boxed_slice(),
        repositories: repositories.into_boxed_slice(),
        rules: rules.into_boxed_slice(),
        webhook,
        convergence_sweep,
    })
}

fn parse_convergence_sweep_configuration(
    item: Option<&Item>,
    interval_ceiling: Option<Duration>,
    cool_off_ceiling: Option<Duration>,
) -> Result<Option<ConvergenceSweepConfiguration>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(None);
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    reject_unknown_fields(table, &["template", "interval_seconds", "cool_off_seconds"])
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    let template = SessionTemplateName::try_new(required_string(table, "template")?.to_owned())
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    let interval = bounded_positive_duration(table, "interval_seconds", interval_ceiling)?;
    let cool_off = bounded_positive_duration(table, "cool_off_seconds", cool_off_ceiling)?;
    Ok(Some(ConvergenceSweepConfiguration {
        template,
        interval,
        cool_off,
    }))
}

fn bounded_positive_duration(
    table: &Table,
    field: &str,
    ceiling: Option<Duration>,
) -> Result<Duration, HubModelConfigurationError> {
    table
        .get(field)
        .and_then(Item::as_integer)
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| *value > 0)
        .map(Duration::from_secs)
        .filter(|value| ceiling.is_none_or(|ceiling| *value <= ceiling))
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
}

fn parse_convergence_pull_requests(
    item: Option<&Item>,
) -> Result<Vec<PullRequestNumber>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(Vec::new());
    };
    let values = item
        .as_array()
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    let mut parsed = Vec::with_capacity(values.len());
    for value in values {
        let number = value
            .as_integer()
            .and_then(|value| u64::try_from(value).ok())
            .and_then(NonZeroU64::new)
            .filter(|value| value.get() <= i32::MAX as u64)
            .map(PullRequestNumber::new)
            .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
        if parsed.contains(&number) {
            return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
        }
        parsed.push(number);
    }
    parsed.sort();
    Ok(parsed)
}

fn parse_repository_watch_webhook_configuration(
    item: Option<&Item>,
) -> Result<Option<RepositoryWatchWebhookConfiguration>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(None);
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    reject_unknown_fields(table, &["bind_address", "path"])
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    let bind_address = table
        .get("bind_address")
        .map(|item| {
            item.as_str()
                .and_then(|value| value.parse::<SocketAddr>().ok())
                .filter(|address| address.port() != 0)
                .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        })
        .transpose()?
        .unwrap_or(DEFAULT_REPOSITORY_WATCH_WEBHOOK_BIND_ADDRESS);
    let path = required_string(table, "path")
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    if !valid_repository_watch_webhook_path(path) {
        return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
    }
    Ok(Some(RepositoryWatchWebhookConfiguration {
        bind_address,
        path: Arc::from(path),
    }))
}

/// Whether the configured path names exactly one literal request path.
///
/// Configuration promises one exact path, but `Router::route` reads its argument
/// as a route pattern: Axum 0.8 treats `{name}` and `{*name}` as captures that
/// match many paths, and it panics on the legacy `:name` and `*name` forms. Both
/// are rejected here rather than at listener start.
fn valid_repository_watch_webhook_path(path: &str) -> bool {
    path.starts_with('/')
        && path.bytes().all(|byte| byte.is_ascii_graphic())
        && !path.contains(['?', '#'])
        && !path.contains(REPOSITORY_WATCH_WEBHOOK_ROUTE_METACHARACTERS)
}

/// Characters Axum reads as routing syntax rather than as literal path bytes.
const REPOSITORY_WATCH_WEBHOOK_ROUTE_METACHARACTERS: [char; 4] = ['*', ':', '{', '}'];

fn parse_repository_watch_rules(
    table: &Table,
) -> Result<Vec<RepoWatchRule>, HubModelConfigurationError> {
    let Some(item) = table.get("rules") else {
        return Ok(Vec::new());
    };
    let tables = item
        .as_array_of_tables()
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    if tables.len() > MAX_REPOSITORY_WATCH_RULES {
        return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
    }
    let mut rules = Vec::with_capacity(tables.len());
    let mut identities = HashSet::with_capacity(tables.len());
    for table in tables {
        reject_unknown_fields(
            table,
            &[
                "id",
                "version",
                "matcher",
                "actions",
                "singleton_per",
                "cooldown_seconds",
            ],
        )
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
        let id = RepoWatchRuleId::try_new(
            required_string(table, "id")
                .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?
                .to_owned(),
        )
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
        if !identities.insert(id.clone()) {
            return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
        }
        let version = table
            .get("version")
            .and_then(Item::as_integer)
            .and_then(|value| u64::try_from(value).ok())
            .and_then(NonZeroU64::new)
            .and_then(RepoWatchRuleVersion::new)
            .ok_or_else(|| HubModelConfigurationError::InvalidRepositoryWatchRule {
                rule: id.as_str().to_owned(),
                reason: String::from(
                    "field `version` must be a positive integer within signed 64-bit range",
                ),
            })?;
        let matcher = table
            .get("matcher")
            .and_then(Item::as_table)
            .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
            .and_then(parse_repository_watch_matcher)?;
        let actions = parse_repository_watch_actions(table)?;
        let singleton_per = match table.get("singleton_per").and_then(Item::as_str) {
            None | Some("pull_request") => RepoWatchSingletonScope::PullRequest,
            Some("stack") => RepoWatchSingletonScope::Stack,
            Some("rule") => RepoWatchSingletonScope::Rule,
            Some("repo") => RepoWatchSingletonScope::Repository,
            Some(_) => return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration),
        };
        let cooldown = table
            .get("cooldown_seconds")
            .map(|item| {
                item.as_integer()
                    .and_then(|value| u64::try_from(value).ok())
                    .filter(|value| *value <= i64::MAX as u64)
                    .map(Duration::from_secs)
                    .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
            })
            .transpose()?
            .unwrap_or(Duration::ZERO);
        let rule = RepoWatchRule::try_new(
            id.clone(),
            version,
            matcher,
            actions,
            singleton_per,
            cooldown,
        )
        .map_err(
            |error| HubModelConfigurationError::InvalidRepositoryWatchRule {
                rule: id.as_str().to_owned(),
                reason: error.to_string(),
            },
        )?;
        rules.push(rule);
    }
    Ok(rules)
}

fn parse_repository_watch_matcher(
    table: &Table,
) -> Result<RepoWatchMatcherV1, HubModelConfigurationError> {
    reject_unknown_fields(
        table,
        &[
            "event_kinds",
            "repo",
            "base_branch",
            "head_branch_regex",
            "title_regex",
            "body_regex",
            "labels",
            "draft",
            "author",
            "mergeable_state",
            "conclusion",
        ],
    )
    .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    Ok(RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
        event_kinds: parse_event_kind_list(table.get("event_kinds"))?,
        repository: optional_repo_watch_string(table, "repo", RepositorySlug::try_new)?,
        base_branch: optional_repo_watch_string(table, "base_branch", BranchName::try_new)?,
        head_branch: optional_repo_watch_string(
            table,
            "head_branch_regex",
            RepoWatchPattern::try_new,
        )?,
        title: optional_repo_watch_string(table, "title_regex", RepoWatchPattern::try_new)?,
        body: optional_repo_watch_string(table, "body_regex", RepoWatchPattern::try_new)?,
        labels: parse_label_matcher(table.get("labels"))?,
        draft: table
            .get("draft")
            .map(|item| {
                item.as_bool()
                    .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
            })
            .transpose()?,
        author: optional_repo_watch_string(table, "author", RepoWatchAuthorLogin::try_new)?,
        mergeable_state: parse_mergeable_state_list(table.get("mergeable_state"))?,
        conclusion: parse_conclusion_list(table.get("conclusion"))?,
    }))
}

fn optional_repo_watch_string<T>(
    table: &Table,
    key: &str,
    constructor: impl FnOnce(String) -> Result<T, signalbox_domain::RepoWatchTextError>,
) -> Result<Option<T>, HubModelConfigurationError> {
    table
        .get(key)
        .map(|item| {
            item.as_str()
                .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
                .and_then(|value| {
                    constructor(value.to_owned()).map_err(|_| {
                        HubModelConfigurationError::InvalidRepositoryWatchConfiguration
                    })
                })
        })
        .transpose()
}

fn parse_repo_watch_any_of(
    item: Option<&Item>,
) -> Result<Option<&toml_edit::Array>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(None);
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    reject_unknown_fields(table, &["any_of"])
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    table
        .get("any_of")
        .and_then(Item::as_array)
        .map(Some)
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
}

fn parse_event_kind_list(
    item: Option<&Item>,
) -> Result<Vec<RepoWatchEventKindNameV1>, HubModelConfigurationError> {
    parse_repo_watch_string_array(item, |value| match value {
        "pull_request_opened" => Some(RepoWatchEventKindNameV1::PullRequestOpened),
        "pull_request_closed" => Some(RepoWatchEventKindNameV1::PullRequestClosed),
        "pull_request_merged" => Some(RepoWatchEventKindNameV1::PullRequestMerged),
        "head_changed" => Some(RepoWatchEventKindNameV1::HeadChanged),
        "mergeable_state_changed" => Some(RepoWatchEventKindNameV1::MergeableStateChanged),
        "checks_completed" => Some(RepoWatchEventKindNameV1::ChecksCompleted),
        "check_run_completed" => Some(RepoWatchEventKindNameV1::CheckRunCompleted),
        "branch_workflow_run_completed" => {
            Some(RepoWatchEventKindNameV1::BranchWorkflowRunCompleted)
        }
        "review_submitted" => Some(RepoWatchEventKindNameV1::ReviewSubmitted),
        "thread_opened" => Some(RepoWatchEventKindNameV1::ThreadOpened),
        "thread_resolved" => Some(RepoWatchEventKindNameV1::ThreadResolved),
        "labeled" => Some(RepoWatchEventKindNameV1::Labeled),
        "unlabeled" => Some(RepoWatchEventKindNameV1::Unlabeled),
        "base_advanced" => Some(RepoWatchEventKindNameV1::BaseAdvanced),
        "reaction_changed" => Some(RepoWatchEventKindNameV1::ReactionChanged),
        _ => None,
    })
}

fn parse_label_matcher(
    item: Option<&Item>,
) -> Result<RepoWatchLabelMatcher, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(RepoWatchLabelMatcher::default());
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    reject_unknown_fields(table, &["any_of", "all_of", "none_of"])
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    Ok(RepoWatchLabelMatcher::new(RepoWatchLabelMatcherInput {
        any_of: parse_repo_watch_text_array(table.get("any_of"), LabelName::try_new)?,
        all_of: parse_repo_watch_text_array(table.get("all_of"), LabelName::try_new)?,
        none_of: parse_repo_watch_text_array(table.get("none_of"), LabelName::try_new)?,
    }))
}

fn parse_mergeable_state_list(
    item: Option<&Item>,
) -> Result<Vec<MergeableState>, HubModelConfigurationError> {
    let array = parse_repo_watch_any_of(item)?;
    parse_repo_watch_array_values(array, |value| match value {
        "mergeable" => Some(MergeableState::Mergeable),
        "conflicting" => Some(MergeableState::Conflicting),
        "unknown" => Some(MergeableState::Unknown),
        _ => None,
    })
}

fn parse_conclusion_list(
    item: Option<&Item>,
) -> Result<Vec<CheckConclusion>, HubModelConfigurationError> {
    let array = parse_repo_watch_any_of(item)?;
    parse_repo_watch_array_values(array, |value| match value {
        "success" => Some(CheckConclusion::Success),
        "failure" => Some(CheckConclusion::Failure),
        "neutral" => Some(CheckConclusion::Neutral),
        "cancelled" => Some(CheckConclusion::Cancelled),
        "skipped" => Some(CheckConclusion::Skipped),
        "timed_out" => Some(CheckConclusion::TimedOut),
        "action_required" => Some(CheckConclusion::ActionRequired),
        "stale" => Some(CheckConclusion::Stale),
        "startup_failure" => Some(CheckConclusion::StartupFailure),
        _ => None,
    })
}

fn parse_repo_watch_text_array<T>(
    item: Option<&Item>,
    constructor: impl Fn(String) -> Result<T, signalbox_domain::RepoWatchTextError>,
) -> Result<Vec<T>, HubModelConfigurationError>
where
    T: Eq,
{
    parse_repo_watch_string_array(item, |value| constructor(value.to_owned()).ok())
}

fn parse_repo_watch_string_array<T>(
    item: Option<&Item>,
    parser: impl Fn(&str) -> Option<T>,
) -> Result<Vec<T>, HubModelConfigurationError>
where
    T: Eq,
{
    let Some(item) = item else {
        return Ok(Vec::new());
    };
    let array = item
        .as_array()
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    parse_repo_watch_array_values(Some(array), parser)
}

fn parse_repo_watch_array_values<T>(
    array: Option<&toml_edit::Array>,
    parser: impl Fn(&str) -> Option<T>,
) -> Result<Vec<T>, HubModelConfigurationError>
where
    T: Eq,
{
    let Some(array) = array else {
        return Ok(Vec::new());
    };
    let mut parsed = Vec::with_capacity(array.len());
    for value in array {
        let value = value
            .as_str()
            .and_then(&parser)
            .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
        if parsed.contains(&value) {
            return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
        }
        parsed.push(value);
    }
    Ok(parsed)
}

fn parse_repository_watch_actions(
    table: &Table,
) -> Result<Vec<RepoWatchRuleActionV1>, HubModelConfigurationError> {
    let actions = table
        .get("actions")
        .and_then(Item::as_array_of_tables)
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    if actions.is_empty() || actions.len() > MAX_REPOSITORY_WATCH_ACTIONS {
        return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
    }
    actions
        .iter()
        .map(|action| {
            reject_unknown_fields(action, &["kind", "template"])
                .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
            if required_string(action, "kind")
                .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?
                != "dispatch_session"
            {
                return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
            }
            let template = SessionTemplateName::try_new(
                required_string(action, "template")
                    .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?
                    .to_owned(),
            )
            .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
            Ok(RepoWatchRuleActionV1::DispatchSession { template })
        })
        .collect()
}
