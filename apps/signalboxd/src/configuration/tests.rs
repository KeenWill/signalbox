use std::{
    collections::HashSet,
    net::SocketAddr,
    num::NonZeroU64,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use rust_decimal::Decimal;
use signalbox_domain::{
    AnthropicServiceTier, DirectModelSelection, FastMode, FastModeOverlay, MergeableState,
    ModelAlias, ModelSelectionRequest, ModelSettingSource, ModelSettingsOverlay,
    ProviderModelIdentity, PullRequestNumber, ReasoningLevel, RepoWatchEventKindNameV1,
    RepoWatchRuleVersion, RepoWatchSingletonScope, ResolvedProviderTarget, ServiceTier,
    SessionTemplateName, SettingOverlay, ToolApprovalPosture,
};
use signalbox_model_runtime::{CredentialAccess, CredentialAccessFailure, CredentialReference};
use signalbox_persistence::{
    model_execution::{
        CredentialPoolRuntimeAction, CredentialPoolRuntimeExhaustion, CredentialPoolRuntimeMember,
        CredentialPoolRuntimePolicy, CredentialPoolRuntimeTieBreak,
    },
    process_read::ProcessModelCallInputTokenSemantics,
};
use signalbox_tools_basic::{CURRENT_TIME_NAME, ECHO_NAME};
use signalbox_tools_web::{WEB_FETCH_NAME, WebFetchEgressPolicy};
use uuid::Uuid;

use crate::credential_pools::{
    CredentialDelivery, CredentialPoolAction, CredentialPoolExhaustion, CredentialPoolTieBreak,
    CredentialPoolTrigger, MAX_CREDENTIAL_CATALOG_NAME_UTF8_BYTES,
    MAX_CREDENTIAL_DELIVERY_PATH_UTF8_BYTES, MAX_CREDENTIAL_HOME_CONCURRENT_INVOCATIONS,
    MAX_CREDENTIAL_POOL_MEMBERS,
};

use super::{
    ANTHROPIC_CREDENTIAL_REFERENCE, BillingKind, DEFAULT_CONVERSATION_IMPORT_MAX_SOURCE_BYTES,
    DEFAULT_REPOSITORY_WATCH_WEBHOOK_BIND_ADDRESS, FileCredentialAccess, HubModelConfiguration,
    HubModelConfigurationError, MAX_COMPACTION_PROMPT_UTF8_BYTES, MIGRATED_ANTHROPIC_MODEL_FAMILY,
    ModelAdapter, ModelCallInputUsage, RepositoryWatchWebhookMode, UnknownSessionModel,
    absolute_search_entries, credential_bytes, resolved_mcp_bridge_reference, validate_alias_count,
    validate_model_count,
};

const CODEX_SUBSCRIPTION_PROFILE: &str = "codex-subscription-primary";
const ANTHROPIC_OVERFLOW_PROFILE: &str = "anthropic-overflow";

fn example_numeric_duration(field: &'static str) -> Duration {
    super::checked_in_example_configuration()
        .expect("checked-in example parses")
        .numeric_bounds()
        .duration(field)
        .flatten()
        .expect("example field is bounded")
}

/// The exact pool block [`CONFIGURATION`] declares, so a test that cares
/// about pool shape states its own replacement in full.
/// The exact pool name [`ANTHROPIC_POOL`] declares.
///
/// Bound once so a rename of the fixture cannot leave an assertion
/// comparing against a stale literal (testing-style rule 6); the helper
/// below asserts the fixture still spells it.
const ANTHROPIC_POOL_NAME: &str = "anthropic-main";

const ANTHROPIC_POOL: &str = r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]"#;

/// The exact Codex pool block [`CONFIGURATION`] declares.
const CODEX_POOL: &str = r#"[[credential_pools]]
name = "codex-main"
tie_break = "first_listed"
on_pool_exhausted = "fail"
members = [{ profile = "codex-subscription-primary", priority = 1 }]"#;

const WATCH_REPOSITORY: &str = "namespace/project";
const SECOND_WATCH_REPOSITORY: &str = "namespace/second";
const WATCH_CREDENTIAL_FILE: &str = "/run/credentials/repository-watch-token";
const SECOND_WATCH_CREDENTIAL_FILE: &str = "/run/credentials/second-watch-token";
const WATCH_WEBHOOK_SECRET_FILE: &str = "/run/credentials/repository-watch-webhook-secret";
const SECOND_WATCH_WEBHOOK_SECRET_FILE: &str =
    "/run/credentials/second-repository-watch-webhook-secret";
const RELATIVE_WATCH_WEBHOOK_SECRET_FILE: &str = "relative/webhook-secret";
const PARENT_COMPONENT_WATCH_WEBHOOK_SECRET_FILE: &str =
    "/run/credentials/alias/../repository-watch-webhook-secret";
const PARENT_COMPONENT_WATCH_CREDENTIAL_FILE: &str =
    "/run/credentials/alias/../repository-watch-token";
const WATCH_CREDENTIAL_REFERENCE: &str = "repository-watch:namespace/project";
const WATCH_WEBHOOK_SECRET_REFERENCE: &str = "repository-watch-webhook:namespace/project";
const WATCH_WEBHOOK_HOOK_ID: NonZeroU64 =
    NonZeroU64::new(123_456_789).expect("fixture hook ID is positive");
const SECOND_WATCH_WEBHOOK_HOOK_ID: NonZeroU64 =
    NonZeroU64::new(987_654_321).expect("fixture hook ID is positive");
const WATCH_WEBHOOK_BIND_ADDRESS: &str = "127.0.0.1:3333";
const IPV6_WATCH_WEBHOOK_BIND_ADDRESS: &str = "[::1]:4444";
const INVALID_WATCH_WEBHOOK_BIND_ADDRESS: &str = "localhost:3333";
const WATCH_WEBHOOK_PATH: &str = "/";
const RELATIVE_WATCH_WEBHOOK_PATH: &str = "github/webhooks";
const QUERY_WATCH_WEBHOOK_PATH: &str = "/github/webhooks?mode=shadow";
const CAPTURE_WATCH_WEBHOOK_PATH: &str = "/github/{delivery}";
const LEGACY_CAPTURE_WATCH_WEBHOOK_PATH: &str = "/github/:delivery";
const WILDCARD_WATCH_WEBHOOK_PATH: &str = "/github/*rest";
const WATCH_INTERVAL_SECONDS: u64 = 90;
const SECOND_WATCH_INTERVAL_SECONDS: u64 = 120;
const CONVERGENCE_PULL_REQUEST: u64 = 892;
const SIGNAL_REVIEWER: &str = "signal-reviewer";
const SECOND_SIGNAL_REVIEWER: &str = "review-bot[bot]";
const GIT_AUTHOR_NAME: &str = "Signalbox Daemon";
const GIT_AUTHOR_EMAIL: &str = "signalbox@example.test";
const EXEC_SUPERVISOR_EXECUTABLE: &str = "/bin/sh";
const PROVIDER_WATCH_REPOSITORY: &str = "Namespace/Project";
const PROVIDER_SECOND_WATCH_REPOSITORY: &str = "Namespace/Second";
const PROVIDER_SIGNAL_REVIEWER: &str = "Signal-Reviewer";
const PROVIDER_SECOND_SIGNAL_REVIEWER: &str = "Review-Bot[bot]";
const DUPLICATE_PROVIDER_WATCH_REPOSITORY: &str = "NAMESPACE/PROJECT";
const DUPLICATE_PROVIDER_SIGNAL_REVIEWER: &str = "SIGNAL-REVIEWER";
const RELATIVE_WATCH_CREDENTIAL_FILE: &str = "relative/watch-token";
const WATCH_RULE_ID: &str = "watch-forward";
const EAGER_WATCH_RULE_ID: &str = "merge-forward-on-base-advance";
const EAGER_WATCH_HEAD_PATTERN: &str = "^agent/.+$";
const WATCH_TEMPLATE: &str = "merge-forward";
const REGISTERED_INSTRUCTION_ROOT: &str = "/srv/signalbox/instruction-library";
pub(crate) const CONFIGURATION: &str = r#"
version = 1

[numeric_bounds]
max_git_object_bytes = "none"
client_frame_deadline = "30s"
client_write_progress_deadline = "30s"
repository_watch_webhook_retention = "604800s"
fenced_pool_min_connections = 48
fenced_pool_floor_reconciliation_interval = "5s"
fenced_pool_floor_reconciliation_attempt_bound = "30s"
max_concurrent_snapshot_readers = 8
max_blob_replica_count = 32
max_session_metadata_tags = 256
max_session_metadata_attributes = 256
max_session_metadata_required_tags = 256
max_system_prompt_utf8_bytes = 1048576
max_imported_text_preview_utf8_bytes = 256
max_review_orchestration_concerns = 32
max_imported_conversation_display_title_scalars = 256
graceful_shutdown_cleanup_window = "30s"
model_exchange_timeout = "600s"
codex_cli_version_probe_bound = "10s"
expired_pass_recovery_attempts = 4
expired_pass_recovery_attempt_bound = "3s"
expired_pass_recovery_lock_retry_delay = "6s"
expired_pass_recovery_conservative_retry_delay = "120s"
convergence_sweep_request_timeout = "30s"
max_convergence_sweep_connection_pages = 100
max_concurrent_convergence_sweep_targets = 8
max_convergence_sweep_request_attempts = 3
convergence_sweep_request_retry_delay = "250ms"
convergence_sweep_retry_backoff_base = "60s"
convergence_sweep_retry_backoff_cap = "900s"
terminalizations_per_liveness_scan = 64
turn_liveness_recovery_attempt_bound = "10s"
automatic_reconciliations_per_liveness_scan = 64
automatic_reconciliation_attempt_bound = "60s"
max_convergence_sweep_targets = 256
max_convergence_sweep_interval = "300s"
max_convergence_sweep_cool_off = "1800s"
automatic_resume_base_backoff = "120s"
automatic_resume_backoff_cap = "1800s"
automatic_resume_attempt_budget = 20
automatic_resume_attempt_ceiling = 100
automatic_resume_startup_retry_delay = "1s"
post_kill_reap_bound = "5s"
stale_active_turn_bound = "1800s"
turn_liveness_scan_interval = "60s"
automatic_reconciliation_base_backoff = "120s"
automatic_reconciliation_backoff_cap = "1800s"
automatic_reconciliation_attempt_budget = 5
terminal_input_channel_capacity = 1
max_message_utf8_bytes = 1048576
min_metadata_page_size = 1
max_metadata_page_size = 100
max_review_findings_per_run = 32
max_automatic_tool_rounds_per_turn = 32
max_same_credential_attempts_per_turn = 2
max_required_tags = 256
reconciliation_sweep_interval = "1s"
nudge_buffer_capacity = 1024
scheduler_pass_admission_cap = 16
scheduler_pass_occupancy_bound = "3600s"
max_native_message_bytes = 2048
terminalization_lock_wait = "250ms"
terminalization_acquire_wait = "250ms"
terminalization_write_lock_wait = "1s"
disposable_postgres_state_ceiling_bytes = 536870912
diagnostic_model_identity_limit = 128
code_host_request_timeout = "none"
max_job_log_bytes = "none"
max_stack_comparisons_in_flight = "none"
max_code_host_result_text_bytes = "none"
max_code_host_result_items = "none"
max_repository_file_content_bytes = "none"
session_admission_deadline = "none"
session_active_stall_deadline = "none"
session_waiting_deadline = "none"
session_lifecycle_metric_scan_interval = "none"

[[credential_profiles]]
name = "anthropic-primary"
adapter = "anthropic"
billing_kind = "api_metered"
delivery = "file"
file = "/run/secrets/anthropic-primary"

[[credential_profiles]]
name = "anthropic-overflow"
adapter = "anthropic"
billing_kind = "api_metered"
delivery = "file"
file = "/run/secrets/anthropic-overflow"

[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient"

[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]

[[credential_pools]]
name = "codex-main"
tie_break = "first_listed"
on_pool_exhausted = "fail"
members = [{ profile = "codex-subscription-primary", priority = 1 }]

[[adapter_mappings]]
model_family = "anthropic"
adapter = "anthropic"
credential_pool = "anthropic-main"

[compaction]
prompt = "Summarize the prior conversation faithfully for continuation."

[web_fetch]
allowed_origins = ["https://example.com"]

[[tool_mappings]]
family = "code_host"
adapter = "github"
credential_profile = "github-primary"
egress_policy = "github_api_only"

[[tool_mappings]]
family = "github"
adapter = "github"
credential_profile = "github-primary"
egress_policy = "github_api_only"

[[tool_mappings]]
family = "workspace"
adapter = "local"
workspace_root = "/srv/signalbox/workspace"

[[tool_mappings]]
family = "conversations"
adapter = "application"

[daemon_tools]
exec_supervisor_executable = "/bin/sh"
sandboxed_exec_timeout_bound = "none"

[git_identity]
author_name = "Signalbox Daemon"
author_email = "signalbox@example.test"

[[models]]
selection_id = "10000000-0000-4000-8000-000000000001"
target_id = "20000000-0000-4000-8000-000000000001"
model_family = "anthropic"
provider_model = "claude-example"
max_output_tokens = 256
context_window_tokens = 200000
rate_version = "fixture-rates-v1"
input_usd_per_million_tokens = "3"
output_usd_per_million_tokens = "15"
cache_creation_input_usd_per_million_tokens = "3.75"
cache_read_input_usd_per_million_tokens = "0.30"

[[aliases]]
alias_id = "30000000-0000-4000-8000-000000000001"
selection_id = "10000000-0000-4000-8000-000000000001"
"#;

/// Replaces the whole Anthropic pool block, leaving every other table as
/// [`CONFIGURATION`] declares it.
fn configuration_with_anthropic_pool(pool: &str) -> String {
    assert!(
        CONFIGURATION.contains(ANTHROPIC_POOL),
        "fixture declares the pool block tests replace"
    );
    assert!(
        ANTHROPIC_POOL.contains(ANTHROPIC_POOL_NAME),
        "the bound pool name is the one the fixture block declares"
    );
    CONFIGURATION.replace(ANTHROPIC_POOL, pool)
}

#[test]
fn client_socket_deadlines_are_required_configuration_keys() {
    let mut document = CONFIGURATION.parse::<toml_edit::DocumentMut>().unwrap();
    document["numeric_bounds"]
        .as_table_mut()
        .unwrap()
        .remove("client_frame_deadline");
    document["numeric_bounds"]
        .as_table_mut()
        .unwrap()
        .remove("client_write_progress_deadline");
    assert_eq!(
        HubModelConfiguration::parse(&document.to_string()).unwrap_err(),
        HubModelConfigurationError::MissingNumericBounds {
            fields: vec!["client_frame_deadline", "client_write_progress_deadline"],
        }
    );
}

#[test]
fn configuration_lists_every_missing_required_numeric_bound() {
    const FIRST_FIELD: &str = "max_session_metadata_tags";
    const SECOND_FIELD: &str = "max_message_utf8_bytes";
    let missing = CONFIGURATION
        .replace("max_session_metadata_tags = 256\n", "")
        .replace("max_message_utf8_bytes = 1048576\n", "");

    let error = HubModelConfiguration::parse(&missing)
        .expect_err("a configuration missing required numeric bounds is refused");

    assert_eq!(
        error,
        HubModelConfigurationError::MissingNumericBounds {
            fields: vec![FIRST_FIELD, SECOND_FIELD],
        }
    );
    assert_eq!(
        error.to_string(),
        format!(
            "model configuration is missing required numeric bounds: {FIRST_FIELD}, {SECOND_FIELD}"
        )
    );
}

#[test]
fn configuration_admits_none_for_optional_integer_and_duration_bounds() {
    let unbounded = CONFIGURATION
        .replace(
            "max_message_utf8_bytes = 1048576",
            "max_message_utf8_bytes = \"none\"",
        )
        .replace(
            "turn_liveness_scan_interval = \"60s\"",
            "turn_liveness_scan_interval = \"none\"",
        );

    let configuration = HubModelConfiguration::parse(&unbounded)
        .expect("the exact none spelling is admitted for optional bounds");

    assert_eq!(
        configuration
            .numeric_bounds()
            .integer("max_message_utf8_bytes"),
        Some(None)
    );
    assert_eq!(
        configuration
            .numeric_bounds()
            .duration("turn_liveness_scan_interval"),
        Some(None)
    );
}

#[test]
fn repository_watch_webhook_retention_is_required_and_must_be_positive_and_finite() {
    const FIELD: &str = "repository_watch_webhook_retention";
    const ENTRY: &str = "repository_watch_webhook_retention = \"604800s\"";
    let missing = CONFIGURATION.replace(ENTRY, "");
    assert_eq!(
        HubModelConfiguration::parse(&missing).expect_err("required retention"),
        HubModelConfigurationError::MissingNumericBounds {
            fields: vec![FIELD]
        }
    );
    for invalid in ["none", "0s"] {
        let configuration = CONFIGURATION.replace(ENTRY, &format!("{FIELD} = {invalid:?}"));
        assert_eq!(
            HubModelConfiguration::parse(&configuration).expect_err("finite positive expiry"),
            HubModelConfigurationError::InvalidNumericBound { field: FIELD }
        );
    }
    assert_eq!(
        HubModelConfiguration::parse(CONFIGURATION)
            .expect("seven-day retention")
            .numeric_bounds()
            .duration(FIELD),
        Some(Some(Duration::from_secs(7 * 24 * 60 * 60)))
    );
}

#[test]
fn numeric_bound_durations_accept_jiff_friendly_input() {
    assert_eq!(
        super::parse_numeric_bound_duration("2 minutes 30 seconds"),
        Some(Duration::from_secs(150))
    );
    assert_eq!(
        super::parse_numeric_bound_duration("1.5s"),
        Some(Duration::from_millis(1_500))
    );
    assert_eq!(super::parse_numeric_bound_duration("-1s"), None);
}

const OPENAI_PROFILE: &str = "openai-primary";
const OPENAI_MAPPING_AND_MODEL: &str = r#"
[[credential_profiles]]
name = "openai-primary"
adapter = "openai"
billing_kind = "api_metered"
delivery = "file"
file = "/run/secrets/openai-primary"

[[credential_pools]]
name = "openai-main"
tie_break = "first_listed"
on_pool_exhausted = "fail"
members = [{ profile = "openai-primary", priority = 1 }]

[[adapter_mappings]]
model_family = "openai"
adapter = "openai"
credential_pool = "openai-main"

[[models]]
selection_id = "10000000-0000-4000-8000-00000000000e"
target_id = "20000000-0000-4000-8000-00000000000e"
model_family = "openai"
provider_model = "gpt-example"
max_output_tokens = 256
context_window_tokens = 200000
reasoning_levels = ["minimal", "medium", "xhigh"]
fast_mode = "request_control"
service_tiers = ["flex", "priority"]
"#;

const CLAUDE_SUBSCRIPTION_PROFILE: &str = "claude-subscription-primary";
const CLAUDE_MODEL_ENTRY: &str = r#"
[[models]]
selection_id = "10000000-0000-4000-8000-00000000000c"
target_id = "20000000-0000-4000-8000-00000000000c"
model_family = "claude_code"
provider_model = "claude-cli-example"
max_output_tokens = 256
context_window_tokens = 200000
reasoning_levels = ["high"]
"#;

const CLAUDE_MCP_BRIDGE_NAME: &str = "signalbox-claude-mcp-bridge";

/// A bridge name no installation holds, so a fixture naming it fails
/// resolution because of the fixture and not the developer's own `PATH`.
const ABSENT_MCP_BRIDGE_NAME: &str = "signalbox-synthetic-absent-mcp-bridge";

/// One Claude process table whose executable and working directory both
/// exist, so the bridge is the only value a test states.
///
/// The returned directory is that working directory and is held by the
/// caller: dropping it would delete the path the document names.
fn configuration_varying_the_claude_bridge(
    mcp_bridge_executable: &Path,
) -> (String, tempfile::TempDir) {
    let workspace = tempfile::tempdir().expect("fixture directory is available");
    let executable = std::env::current_exe().expect("the test executable has a path");
    let configuration =
        configuration_with_claude_paths(&executable, mcp_bridge_executable, workspace.path());
    (configuration, workspace)
}

fn synthetic_search_directory(root: &Path, name: &str) -> PathBuf {
    let directory = root.join(name);
    std::fs::create_dir(&directory).expect("fixture search entry is creatable");
    directory
}

fn synthetic_search_path(entries: &[&Path]) -> std::ffi::OsString {
    std::env::join_paths(entries.iter().copied()).expect("fixture search entries join")
}

fn synthetic_executable(directory: &Path, name: &str) -> PathBuf {
    let path = synthetic_file(directory, name);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("fixture program is markable executable");
    }
    path
}

#[cfg(unix)]
fn synthetic_unexecutable_file(directory: &Path, name: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = synthetic_file(directory, name);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("fixture file is markable unexecutable");
    path
}

fn synthetic_file(directory: &Path, name: &str) -> PathBuf {
    let path = directory.join(name);
    std::fs::write(&path, b"").expect("fixture file is writable");
    path
}

fn configuration_with_claude_paths(
    executable: &Path,
    mcp_bridge_executable: &Path,
    working_directory: &Path,
) -> String {
    format!(
        r#"{CONFIGURATION}
[[credential_profiles]]
name = "{CLAUDE_SUBSCRIPTION_PROFILE}"
adapter = "claude_cli"
billing_kind = "subscription"
delivery = "ambient"

[[credential_pools]]
name = "claude-code-main"
tie_break = "first_listed"
on_pool_exhausted = "fail"
members = [{{ profile = "{CLAUDE_SUBSCRIPTION_PROFILE}", priority = 1 }}]

[[adapter_mappings]]
model_family = "claude_code"
adapter = "claude_cli"
credential_pool = "claude-code-main"

[claude_cli]
executable = "{}"
mcp_bridge_executable = "{}"
working_directory = "{}"
"#,
        executable.display(),
        mcp_bridge_executable.display(),
        working_directory.display(),
    )
}

fn configuration_with_codex_paths(executable: &Path, working_directory: &Path) -> String {
    format!(
        r#"{CONFIGURATION}
[[adapter_mappings]]
model_family = "codex"
adapter = "codex_cli"
credential_pool = "codex-main"

[codex_cli]
executable = "{}"
working_directory = "{}"
"#,
        executable.display(),
        working_directory.display(),
    )
}

fn configuration_with_api_metered_codex_model(
    executable: &Path,
    working_directory: &Path,
) -> String {
    let configuration = CONFIGURATION
        .replace("codex-subscription-primary", "codex-api-primary")
        .replace(
            "billing_kind = \"subscription\"",
            "billing_kind = \"api_metered\"",
        );
    format!(
        r#"{configuration}
[[adapter_mappings]]
model_family = "codex-api"
adapter = "codex_cli"
credential_pool = "codex-main"

[codex_cli]
executable = "{}"
working_directory = "{}"

[[models]]
selection_id = "10000000-0000-4000-8000-000000000002"
target_id = "20000000-0000-4000-8000-000000000002"
model_family = "codex-api"
provider_model = "gpt-example"
max_output_tokens = 256
context_window_tokens = 200000
rate_version = "fixture-codex-rates-v1"
input_usd_per_million_tokens = "1"
output_usd_per_million_tokens = "2"
cache_creation_input_usd_per_million_tokens = "3"
cache_read_input_usd_per_million_tokens = "4"
"#,
        executable.display(),
        working_directory.display(),
    )
}

fn configuration_without_tool_mappings() -> String {
    let start = CONFIGURATION
        .find("[[tool_mappings]]")
        .expect("fixture has tool mappings");
    let end = CONFIGURATION
        .find("[[models]]")
        .expect("fixture has model definitions");
    format!("{}{}", &CONFIGURATION[..start], &CONFIGURATION[end..])
}

fn configuration_with_repository_watch() -> String {
    format!(
        r#"{CONFIGURATION}

[repository_watch]
version = 1
signal_reviewers = ["{PROVIDER_SIGNAL_REVIEWER}", "{PROVIDER_SECOND_SIGNAL_REVIEWER}"]

[[repository_watch.repositories]]
repository = "{PROVIDER_WATCH_REPOSITORY}"
poll_interval_seconds = {WATCH_INTERVAL_SECONDS}
credential_file = "{WATCH_CREDENTIAL_FILE}"

[[repository_watch.repositories]]
repository = "{PROVIDER_SECOND_WATCH_REPOSITORY}"
poll_interval_seconds = {SECOND_WATCH_INTERVAL_SECONDS}
credential_file = "{SECOND_WATCH_CREDENTIAL_FILE}"
"#,
    )
}

fn configuration_with_convergence_sweep() -> String {
    format!(
            r#"{}

[repository_watch.convergence_sweep]
template = "{WATCH_TEMPLATE}"
interval_seconds = {}
cool_off_seconds = {}
"#,
            configuration_with_repository_watch().replace(
                &format!("repository = \"{PROVIDER_WATCH_REPOSITORY}\""),
                &format!(
                    "repository = \"{PROVIDER_WATCH_REPOSITORY}\"\nconvergence_pull_requests = [{CONVERGENCE_PULL_REQUEST}]"
                ),
            ),
            example_numeric_duration("max_convergence_sweep_interval").as_secs(),
            example_numeric_duration("max_convergence_sweep_cool_off").as_secs(),
        )
}

fn configuration_with_repository_watch_webhook_entry() -> String {
    configuration_with_repository_watch().replace(
            &format!("credential_file = \"{WATCH_CREDENTIAL_FILE}\""),
            &format!(
                "credential_file = \"{WATCH_CREDENTIAL_FILE}\"\nwebhook_hook_id = {WATCH_WEBHOOK_HOOK_ID}\nwebhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\""
            ),
        )
}

fn configuration_with_repository_watch_webhook() -> String {
    format!(
        r#"{}

[repository_watch.webhook]
bind_address = "{WATCH_WEBHOOK_BIND_ADDRESS}"
path = "{WATCH_WEBHOOK_PATH}"
"#,
        configuration_with_repository_watch_webhook_entry()
    )
}

#[test]
fn credential_admission_checks_every_configured_file_kind() {
    const PUSH_CREDENTIAL_FILE: &str = "/run/credentials/push-token";
    let directory = tempfile::tempdir().expect("credential fixture directory");
    let mut source = configuration_with_repository_watch_webhook().replace(
        &format!("credential_file = \"{WATCH_CREDENTIAL_FILE}\""),
        &format!("credential_file = \"{WATCH_CREDENTIAL_FILE}\"\npush_credential_file = \"{PUSH_CREDENTIAL_FILE}\""),
    );
    let mut files = Vec::new();
    for configured_path in [
        "/run/secrets/anthropic-primary",
        "/run/secrets/anthropic-overflow",
        WATCH_CREDENTIAL_FILE,
        SECOND_WATCH_CREDENTIAL_FILE,
        WATCH_WEBHOOK_SECRET_FILE,
        PUSH_CREDENTIAL_FILE,
    ] {
        let file = tempfile::NamedTempFile::new_in(directory.path()).expect("private credential");
        source = source.replace(configured_path, file.path().to_str().expect("fixture path"));
        files.push(file);
    }
    let configuration = HubModelConfiguration::parse(&source).expect("catalog with private files");
    configuration
        .validate_credential_files()
        .expect("all configured files admitted");
    for file in &files {
        std::fs::set_permissions(
            file.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o644),
        )
        .expect("expose one file");
        assert_eq!(
            configuration
                .validate_credential_files()
                .expect_err("each configured credential must be private")
                .failure,
            CredentialAccessFailure::InsecurePermissions
        );
        std::fs::set_permissions(
            file.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .expect("restore private file");
        configuration
            .validate_credential_files()
            .expect("remaining files still admitted");
    }
}

fn configuration_with_repository_watch_rule() -> String {
    format!(
        r#"{}

[[repository_watch.rules]]
id = "{WATCH_RULE_ID}"
version = 1
singleton_per = "pull_request"
cooldown_seconds = 30

[repository_watch.rules.matcher]
event_kinds = ["mergeable_state_changed"]
repo = "{PROVIDER_WATCH_REPOSITORY}"
base_branch = "main"
head_branch_regex = "^stack/.+$"
title_regex = "^.*$"
body_regex = "^.*$"
draft = false
author = "{PROVIDER_SIGNAL_REVIEWER}"

[repository_watch.rules.matcher.labels]
any_of = ["stack"]
all_of = ["owned"]
none_of = ["hold"]

[repository_watch.rules.matcher.mergeable_state]
any_of = ["conflicting"]

[repository_watch.rules.matcher.conclusion]
any_of = []

[[repository_watch.rules.actions]]
kind = "dispatch_session"
template = "{WATCH_TEMPLATE}"
"#,
        configuration_with_repository_watch()
    )
}

fn configuration_with_eager_merge_forward_rule() -> String {
    format!(
        r#"{}

[[repository_watch.rules]]
id = "{EAGER_WATCH_RULE_ID}"
version = 1
singleton_per = "pull_request"
cooldown_seconds = 0

[repository_watch.rules.matcher]
event_kinds = ["base_advanced"]
repo = "{PROVIDER_WATCH_REPOSITORY}"
head_branch_regex = "{EAGER_WATCH_HEAD_PATTERN}"

[[repository_watch.rules.actions]]
kind = "dispatch_session"
template = "{WATCH_TEMPLATE}"
"#,
        configuration_with_repository_watch()
    )
}

fn watch_interval_fixture() -> Duration {
    Duration::from_secs(WATCH_INTERVAL_SECONDS)
}

fn judged_direct_selection_fixture() -> DirectModelSelection {
    DirectModelSelection::from_uuid(Uuid::from_u128(2))
}

fn configured_judge_selection_fixture() -> DirectModelSelection {
    DirectModelSelection::from_uuid(
        Uuid::parse_str("10000000-0000-4000-8000-000000000001")
            .expect("configured judge fixture UUID is valid"),
    )
}
#[test]
fn configured_tool_postures_are_typed() {
    let configured = HubModelConfiguration::parse(&format!(
        r#"{CONFIGURATION}

[tool_approval_postures]
{echo} = "auto"
{current_time} = "delegated"
{web_fetch} = "human"
"#,
        echo = ECHO_NAME,
        current_time = CURRENT_TIME_NAME,
        web_fetch = WEB_FETCH_NAME
    ))
    .expect("posture settings are valid");
    let postures = configured.tool_approval_postures().collect::<Vec<_>>();

    assert_eq!(postures[0].0.as_str(), CURRENT_TIME_NAME);
    assert_eq!(postures[0].1, ToolApprovalPosture::Delegated);
    assert_eq!(postures[1].0.as_str(), ECHO_NAME);
    assert_eq!(postures[1].1, ToolApprovalPosture::Auto);
    assert_eq!(postures[2].0.as_str(), WEB_FETCH_NAME);
    assert_eq!(postures[2].1, ToolApprovalPosture::Human);
}
#[test]
fn configured_judge_selection_is_typed() {
    let configured = HubModelConfiguration::parse(&format!(
        r#"{CONFIGURATION}

[approval_judge]
selection_id = "10000000-0000-4000-8000-000000000001"
"#
    ))
    .expect("judge setting is valid");
    let judged = judged_direct_selection_fixture();

    assert_eq!(
        configured.approval_judge_selection(judged),
        configured_judge_selection_fixture()
    );
}

#[test]
fn absent_tool_postures_preserve_legacy_policy() {
    let configured =
        HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");

    assert_eq!(configured.tool_approval_postures().count(), 0);
}

#[test]
fn absent_judge_selection_preserves_the_judged_model() {
    let configured =
        HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");
    let judged = judged_direct_selection_fixture();

    assert_eq!(configured.approval_judge_selection(judged), judged);
}

#[test]
fn absent_repository_watch_configuration_starts_no_watch_tasks() {
    let configured =
        HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");

    assert_eq!(configured.repository_watch(), None);
}

#[test]
fn repository_watch_is_enabled_by_default() {
    let configured = HubModelConfiguration::parse(&configuration_with_repository_watch())
        .expect("repository-watch configuration");
    assert!(
        configured
            .repository_watch()
            .expect("configured watch")
            .enabled()
    );
}

#[test]
fn repository_watch_can_be_disabled_explicitly() {
    let configured = HubModelConfiguration::parse(&configuration_with_repository_watch().replace(
        "[repository_watch]\nversion = 1",
        "[repository_watch]\nversion = 1\nenabled = false",
    ))
    .expect("disabled repository-watch configuration");
    assert!(
        !configured
            .repository_watch()
            .expect("configured watch")
            .enabled()
    );
}

#[test]
fn repository_watch_normalizes_signal_reviewer_logins() {
    let configured = HubModelConfiguration::parse(&configuration_with_repository_watch())
        .expect("repository-watch fixture is valid");
    let repository_watch = configured
        .repository_watch()
        .expect("fixture configures repository watch");

    assert_eq!(
        repository_watch.signal_reviewers()[0].as_str(),
        SECOND_SIGNAL_REVIEWER
    );
    assert_eq!(
        repository_watch.signal_reviewers()[1].as_str(),
        SIGNAL_REVIEWER
    );
}

#[test]
fn repository_watch_builds_a_canonical_repository_inventory() {
    let configured = HubModelConfiguration::parse(&configuration_with_repository_watch())
        .expect("repository-watch fixture is valid");
    let repositories = configured
        .repository_watch()
        .expect("fixture configures repository watch")
        .repositories();

    assert_eq!(repositories[0].repository().as_str(), WATCH_REPOSITORY);
    assert_eq!(
        repositories[1].repository().as_str(),
        SECOND_WATCH_REPOSITORY
    );
}

#[test]
fn repository_watch_preserves_each_repository_interval() {
    let configured = HubModelConfiguration::parse(&configuration_with_repository_watch())
        .expect("repository-watch fixture is valid");
    let watched = &configured
        .repository_watch()
        .expect("fixture configures repository watch")
        .repositories()[0];

    assert_eq!(watched.poll_interval(), watch_interval_fixture());
}

#[test]
fn repository_watch_preserves_each_credential_file_reference() {
    let configured = HubModelConfiguration::parse(&configuration_with_repository_watch())
        .expect("repository-watch fixture is valid");
    let repositories = configured
        .repository_watch()
        .expect("fixture configures repository watch")
        .repositories();

    assert_eq!(
        repositories[0].credential_file(),
        Path::new(WATCH_CREDENTIAL_FILE)
    );
    assert_eq!(
        repositories[1].credential_file(),
        Path::new(SECOND_WATCH_CREDENTIAL_FILE)
    );
}

#[test]
fn repository_watch_derives_a_repository_scoped_credential_reference() {
    let configured = HubModelConfiguration::parse(&configuration_with_repository_watch())
        .expect("repository-watch fixture is valid");
    let watched = &configured
        .repository_watch()
        .expect("fixture configures repository watch")
        .repositories()[0];

    assert_eq!(
        watched.credential_reference().as_str(),
        WATCH_CREDENTIAL_REFERENCE
    );
}

#[test]
fn repository_watch_debug_redacts_the_credential_file_reference() {
    let configured = HubModelConfiguration::parse(&configuration_with_repository_watch())
        .expect("repository-watch fixture is valid");
    let watched = &configured
        .repository_watch()
        .expect("fixture configures repository watch")
        .repositories()[0];
    let debug = format!("{watched:?}");

    assert!(!debug.contains(WATCH_CREDENTIAL_FILE));
    assert!(debug.contains("[REDACTED REFERENCE]"));
}

#[test]
fn repository_watch_webhook_is_absent_by_default() {
    let configured = HubModelConfiguration::parse(&configuration_with_repository_watch())
        .expect("repository-watch fixture is valid");
    let repository_watch = configured
        .repository_watch()
        .expect("fixture configures repository watch");

    assert_eq!(repository_watch.webhook(), None);
    assert_eq!(repository_watch.repositories()[0].webhook(), None);
}

#[test]
fn repository_watch_webhook_preserves_the_local_listener() {
    let configured = HubModelConfiguration::parse(&configuration_with_repository_watch_webhook())
        .expect("repository-watch webhook fixture is valid");
    let webhook = configured
        .repository_watch()
        .expect("fixture configures repository watch")
        .webhook()
        .expect("fixture configures the webhook listener");

    assert_eq!(
        webhook.bind_address(),
        DEFAULT_REPOSITORY_WATCH_WEBHOOK_BIND_ADDRESS
    );
    assert_eq!(webhook.path(), WATCH_WEBHOOK_PATH);
}

#[test]
fn repository_watch_webhook_defaults_the_bind_address() {
    let configured = configuration_with_repository_watch_webhook().replace(
        &format!("bind_address = \"{WATCH_WEBHOOK_BIND_ADDRESS}\"\n"),
        "",
    );
    let parsed = HubModelConfiguration::parse(&configured)
        .expect("the omitted bind address selects the reference default");
    let webhook = parsed
        .repository_watch()
        .expect("fixture configures repository watch")
        .webhook()
        .expect("fixture configures the webhook listener");

    assert_eq!(
        webhook.bind_address(),
        DEFAULT_REPOSITORY_WATCH_WEBHOOK_BIND_ADDRESS
    );
}

#[test]
fn repository_watch_webhook_accepts_a_configured_socket_address() {
    let configured = configuration_with_repository_watch_webhook().replace(
        &format!("bind_address = \"{WATCH_WEBHOOK_BIND_ADDRESS}\""),
        &format!("bind_address = \"{IPV6_WATCH_WEBHOOK_BIND_ADDRESS}\""),
    );
    let parsed = HubModelConfiguration::parse(&configured)
        .expect("the configured IPv6 loopback listener is valid");
    let webhook = parsed
        .repository_watch()
        .expect("fixture configures repository watch")
        .webhook()
        .expect("fixture configures the webhook listener");

    assert_eq!(
        webhook.bind_address(),
        IPV6_WATCH_WEBHOOK_BIND_ADDRESS
            .parse::<SocketAddr>()
            .expect("fixture address is valid")
    );
}

#[test]
fn repository_watch_webhook_rejects_port_zero() {
    let configured = configuration_with_repository_watch_webhook()
        .replace(WATCH_WEBHOOK_BIND_ADDRESS, "127.0.0.1:0");

    assert!(matches!(
        HubModelConfiguration::parse(&configured),
        Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    ));
}

#[test]
fn repository_watch_webhook_associates_hook_and_secret_with_repository() {
    let configured = HubModelConfiguration::parse(&configuration_with_repository_watch_webhook())
        .expect("repository-watch webhook fixture is valid");
    let watched = &configured
        .repository_watch()
        .expect("fixture configures repository watch")
        .repositories()[0];
    let webhook = watched
        .webhook()
        .expect("the first repository configures webhook intake");

    assert_eq!(webhook.hook_id(), WATCH_WEBHOOK_HOOK_ID);
    assert_eq!(webhook.secret_file(), Path::new(WATCH_WEBHOOK_SECRET_FILE));
    assert_eq!(
        watched
            .webhook_secret_reference()
            .expect("the webhook repository has a secret reference")
            .as_str(),
        WATCH_WEBHOOK_SECRET_REFERENCE
    );
}

#[test]
fn repository_watch_webhook_defaults_to_shadow_mode() {
    let configured = HubModelConfiguration::parse(&configuration_with_repository_watch_webhook())
        .expect("repository-watch webhook fixture is valid");
    let webhook = configured
        .repository_watch()
        .expect("fixture configures repository watch")
        .repositories()[0]
        .webhook()
        .expect("the first repository configures webhook intake");

    assert_eq!(webhook.mode(), RepositoryWatchWebhookMode::Shadow);
}

#[test]
fn repository_watch_webhook_selects_primary_mode() {
    let configured = configuration_with_repository_watch_webhook().replace(
        &format!("webhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\""),
        &format!(
            "webhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\"\nwebhook_mode = \"primary\""
        ),
    );
    let configured = HubModelConfiguration::parse(&configured)
        .expect("an explicit primary webhook mode is valid");
    let webhook = configured
        .repository_watch()
        .expect("fixture configures repository watch")
        .repositories()[0]
        .webhook()
        .expect("the first repository configures webhook intake");

    assert_eq!(webhook.mode(), RepositoryWatchWebhookMode::Primary);
}

/// Only an absent key defaults. A present non-string item is malformed
/// configuration, not an omission, so it must not silently select shadow.
#[test]
fn repository_watch_webhook_rejects_a_non_string_mode() {
    let configured = configuration_with_repository_watch_webhook().replace(
        &format!("webhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\""),
        &format!("webhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\"\nwebhook_mode = true"),
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_webhook_rejects_an_unknown_mode() {
    let configured = configuration_with_repository_watch_webhook().replace(
            &format!("webhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\""),
            &format!(
                "webhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\"\nwebhook_mode = \"authoritative\""
            ),
        );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_webhook_rejects_a_mode_without_an_association() {
    let configured = configuration_with_repository_watch_webhook()
        .replace(
            &format!("webhook_hook_id = {WATCH_WEBHOOK_HOOK_ID}\n"),
            "webhook_mode = \"primary\"\n",
        )
        .replace(
            &format!("webhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\"\n"),
            "",
        );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_webhook_debug_redacts_the_secret_file_reference() {
    let configured = HubModelConfiguration::parse(&configuration_with_repository_watch_webhook())
        .expect("repository-watch webhook fixture is valid");
    let webhook = configured
        .repository_watch()
        .expect("fixture configures repository watch")
        .repositories()[0]
        .webhook()
        .expect("the first repository configures webhook intake");
    let debug = format!("{webhook:?}");

    assert!(!debug.contains(WATCH_WEBHOOK_SECRET_FILE));
    assert!(debug.contains("[REDACTED REFERENCE]"));
}

#[test]
fn repository_watch_webhook_rejects_a_listener_without_enabled_repository() {
    let configured = format!(
        "{}\n[repository_watch.webhook]\npath = \"{WATCH_WEBHOOK_PATH}\"\n",
        configuration_with_repository_watch()
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_webhook_rejects_an_entry_without_listener() {
    let configured = configuration_with_repository_watch_webhook_entry();

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_webhook_rejects_hook_id_without_secret_file() {
    let configured = configuration_with_repository_watch_webhook().replace(
        &format!("webhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\"\n"),
        "",
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_webhook_rejects_secret_file_without_hook_id() {
    let configured = configuration_with_repository_watch_webhook()
        .replace(&format!("webhook_hook_id = {WATCH_WEBHOOK_HOOK_ID}\n"), "");

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_webhook_rejects_zero_hook_id() {
    let configured = configuration_with_repository_watch_webhook().replace(
        &format!("webhook_hook_id = {WATCH_WEBHOOK_HOOK_ID}"),
        "webhook_hook_id = 0",
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_webhook_rejects_duplicate_hook_ids() {
    let configured = configuration_with_repository_watch_webhook().replace(
            &format!("credential_file = \"{SECOND_WATCH_CREDENTIAL_FILE}\""),
            &format!(
                "credential_file = \"{SECOND_WATCH_CREDENTIAL_FILE}\"\nwebhook_hook_id = {WATCH_WEBHOOK_HOOK_ID}\nwebhook_secret_file = \"{SECOND_WATCH_WEBHOOK_SECRET_FILE}\""
            ),
        );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::DuplicateRepositoryWatchWebhookHookId)
    );
}

#[test]
fn repository_watch_webhook_rejects_a_relative_local_path() {
    let configured = configuration_with_repository_watch_webhook().replace(
        &format!("path = \"{WATCH_WEBHOOK_PATH}\""),
        &format!("path = \"{RELATIVE_WATCH_WEBHOOK_PATH}\""),
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_webhook_rejects_query_in_local_path() {
    let configured = configuration_with_repository_watch_webhook().replace(
        &format!("path = \"{WATCH_WEBHOOK_PATH}\""),
        &format!("path = \"{QUERY_WATCH_WEBHOOK_PATH}\""),
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

/// Parses the webhook fixture with `path` replaced, returning any failure.
fn repository_watch_webhook_path_failure(path: &str) -> Option<HubModelConfigurationError> {
    let configured = configuration_with_repository_watch_webhook().replace(
        &format!("path = \"{WATCH_WEBHOOK_PATH}\""),
        &format!("path = \"{path}\""),
    );
    HubModelConfiguration::parse(&configured).err()
}

#[test]
fn repository_watch_webhook_rejects_a_braced_capture_local_path() {
    assert_eq!(
        repository_watch_webhook_path_failure(CAPTURE_WATCH_WEBHOOK_PATH),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_webhook_rejects_a_legacy_capture_local_path() {
    assert_eq!(
        repository_watch_webhook_path_failure(LEGACY_CAPTURE_WATCH_WEBHOOK_PATH),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_webhook_rejects_a_wildcard_local_path() {
    assert_eq!(
        repository_watch_webhook_path_failure(WILDCARD_WATCH_WEBHOOK_PATH),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_webhook_rejects_an_invalid_bind_address() {
    let configured = configuration_with_repository_watch_webhook().replace(
        &format!("bind_address = \"{WATCH_WEBHOOK_BIND_ADDRESS}\""),
        &format!("bind_address = \"{INVALID_WATCH_WEBHOOK_BIND_ADDRESS}\""),
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_webhook_rejects_poll_credential_as_secret() {
    let configured = configuration_with_repository_watch_webhook()
        .replace(WATCH_WEBHOOK_SECRET_FILE, WATCH_CREDENTIAL_FILE);

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
    );
}

#[test]
fn repository_watch_webhook_rejects_a_relative_secret_file() {
    let configured = configuration_with_repository_watch_webhook().replace(
        WATCH_WEBHOOK_SECRET_FILE,
        RELATIVE_WATCH_WEBHOOK_SECRET_FILE,
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_webhook_rejects_parent_components_in_secret_file() {
    let configured = configuration_with_repository_watch_webhook().replace(
        WATCH_WEBHOOK_SECRET_FILE,
        PARENT_COMPONENT_WATCH_WEBHOOK_SECRET_FILE,
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_webhook_rejects_a_symlink_to_poll_credential() {
    let directory = tempfile::tempdir().expect("the credential fixture directory exists");
    let credential = directory.path().join("watch-token");
    std::fs::write(&credential, []).expect("the polling credential fixture exists");
    let secret_alias = directory.path().join("webhook-secret-alias");
    std::os::unix::fs::symlink(&credential, &secret_alias)
        .expect("the webhook secret alias exists");
    let configured = configuration_with_repository_watch_webhook()
        .replace(WATCH_CREDENTIAL_FILE, &credential.display().to_string())
        .replace(
            WATCH_WEBHOOK_SECRET_FILE,
            &secret_alias.display().to_string(),
        );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
    );
}

#[cfg(unix)]
#[test]
fn repository_watch_webhook_rejects_a_hard_link_to_poll_credential() {
    let directory = tempfile::tempdir().expect("the credential fixture directory exists");
    let credential = directory.path().join("watch-token");
    std::fs::write(&credential, []).expect("the polling credential fixture exists");
    let secret_hard_link = directory.path().join("webhook-secret-hard-link");
    std::fs::hard_link(&credential, &secret_hard_link)
        .expect("the webhook secret hard link exists");
    let configured = configuration_with_repository_watch_webhook()
        .replace(WATCH_CREDENTIAL_FILE, &credential.display().to_string())
        .replace(
            WATCH_WEBHOOK_SECRET_FILE,
            &secret_hard_link.display().to_string(),
        );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
    );
}

#[test]
fn repository_watch_webhook_rejects_a_dangling_alias_to_poll_credential() {
    let directory = tempfile::tempdir().expect("the credential fixture directory exists");
    let credential = directory.path().join("pending-watch-token");
    let secret_alias = directory.path().join("pending-webhook-secret-alias");
    std::os::unix::fs::symlink(&credential, &secret_alias)
        .expect("the dangling webhook secret alias exists");
    let configured = configuration_with_repository_watch_webhook()
        .replace(WATCH_CREDENTIAL_FILE, &credential.display().to_string())
        .replace(
            WATCH_WEBHOOK_SECRET_FILE,
            &secret_alias.display().to_string(),
        );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
    );
}

#[test]
fn repository_watch_webhook_rejects_a_shared_secret_file() {
    let configured = configuration_with_repository_watch_webhook().replace(
            &format!("credential_file = \"{SECOND_WATCH_CREDENTIAL_FILE}\""),
            &format!(
                "credential_file = \"{SECOND_WATCH_CREDENTIAL_FILE}\"\nwebhook_hook_id = {SECOND_WATCH_WEBHOOK_HOOK_ID}\nwebhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\""
            ),
        );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
    );
}

#[test]
fn repository_watch_parses_the_structured_rule_fields() {
    let configured = HubModelConfiguration::parse(&configuration_with_repository_watch_rule())
        .expect("repository-watch rule fixture is valid");
    let rule = &configured
        .repository_watch()
        .expect("fixture configures repository watch")
        .rules()[0];

    assert_eq!(rule.id().as_str(), WATCH_RULE_ID);
    assert_eq!(rule.version().get(), 1);
    assert_eq!(rule.singleton_per(), RepoWatchSingletonScope::PullRequest);
    assert_eq!(rule.cooldown(), Duration::from_secs(30));
    assert_eq!(
        rule.matcher().event_kinds(),
        [RepoWatchEventKindNameV1::MergeableStateChanged]
    );
    assert_eq!(
        rule.matcher().mergeable_state(),
        [MergeableState::Conflicting]
    );
    assert_eq!(rule.actions()[0].template().as_str(), WATCH_TEMPLATE);
}

#[test]
fn repository_watch_durations_accept_friendly_strings() {
    let mut document = configuration_with_convergence_sweep()
        .parse::<toml_edit::DocumentMut>()
        .expect("fixture TOML is valid");
    document["repository_watch"]["repositories"]
        .as_array_of_tables_mut()
        .expect("fixture repositories")
        .get_mut(0)
        .expect("first repository")["poll_interval_seconds"] = toml_edit::value("30s");
    document["repository_watch"]["convergence_sweep"]["interval_seconds"] =
        toml_edit::value("2 minutes 30 seconds");
    document["repository_watch"]["convergence_sweep"]["cool_off_seconds"] = toml_edit::value("5m");
    let configured = HubModelConfiguration::parse(&document.to_string())
        .expect("friendly durations are admitted");
    let watch = configured.repository_watch().expect("repository watch");
    let sweep = watch.convergence_sweep().expect("convergence sweep");

    assert_eq!(
        watch.repositories()[0].poll_interval(),
        Duration::from_secs(30)
    );
    assert_eq!(sweep.interval(), Duration::from_secs(150));
    assert_eq!(sweep.cool_off(), Duration::from_secs(300));
}

#[test]
fn repository_watch_cooldown_accepts_friendly_strings_with_whole_second_precision() {
    let mut document = configuration_with_repository_watch_rule()
        .parse::<toml_edit::DocumentMut>()
        .expect("fixture TOML is valid");
    for (input, expected) in [("2h", 7200), ("0s", 0)] {
        document["repository_watch"]["rules"]
            .as_array_of_tables_mut()
            .expect("fixture rules")
            .get_mut(0)
            .expect("first rule")["cooldown_seconds"] = toml_edit::value(input);
        let configured = HubModelConfiguration::parse(&document.to_string())
            .expect("friendly cooldown is admitted");
        assert_eq!(
            configured
                .repository_watch()
                .expect("repository watch")
                .rules()[0]
                .cooldown(),
            Duration::from_secs(expected),
            "{input}"
        );
    }
    for input in ["1.5s", "-1s", "9223372036854775808s", "invalid"] {
        document["repository_watch"]["rules"]
            .as_array_of_tables_mut()
            .expect("fixture rules")
            .get_mut(0)
            .expect("first rule")["cooldown_seconds"] = toml_edit::value(input);
        assert!(
            HubModelConfiguration::parse(&document.to_string()).is_err(),
            "{input}"
        );
    }
}

#[test]
fn repository_watch_friendly_durations_preserve_positive_and_ceiling_checks() {
    let mut document = configuration_with_convergence_sweep()
        .parse::<toml_edit::DocumentMut>()
        .expect("fixture TOML is valid");
    // The fixture's interval ceiling is 300 seconds.
    document["numeric_bounds"]["max_convergence_sweep_interval"] = toml_edit::value("5m");
    for input in ["0s", "-1s", "301s", "invalid"] {
        document["repository_watch"]["convergence_sweep"]["interval_seconds"] =
            toml_edit::value(input);
        assert_eq!(
            HubModelConfiguration::parse(&document.to_string()).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration),
            "{input}"
        );
    }
}

#[test]
fn repository_watch_parses_the_explicit_convergence_sweep() {
    let configured = HubModelConfiguration::parse(&configuration_with_convergence_sweep())
        .expect("convergence sweep fixture is valid");
    let watch = configured
        .repository_watch()
        .expect("fixture configures repository watch");
    let policy = watch
        .convergence_sweep()
        .expect("fixture enables convergence reconciliation");
    let repository = watch
        .repositories()
        .iter()
        .find(|entry| entry.repository().as_str() == PROVIDER_WATCH_REPOSITORY.to_ascii_lowercase())
        .expect("fixture repository is retained");
    let pull_request = PullRequestNumber::new(
        NonZeroU64::new(CONVERGENCE_PULL_REQUEST).expect("fixture number is positive"),
    );

    assert_eq!(policy.template().as_str(), WATCH_TEMPLATE);
    assert_eq!(
        policy.interval(),
        example_numeric_duration("max_convergence_sweep_interval")
    );
    assert_eq!(
        policy.cool_off(),
        example_numeric_duration("max_convergence_sweep_cool_off")
    );
    assert_eq!(repository.convergence_pull_requests(), [pull_request]);
}

#[tokio::test]
async fn disabled_repository_watch_does_not_activate_configured_convergence_targets() {
    use signalbox_application::InProcessEligibilityWorkSource;
    use signalbox_persistence::scheduler::PostgresEligibilitySweep;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    let configured = HubModelConfiguration::parse(
        &configuration_with_convergence_sweep()
            .replace("[repository_watch]", "[repository_watch]\nenabled = false"),
    )
    .expect("disabled repository watch retains valid convergence configuration");
    let watch = configured
        .repository_watch()
        .expect("repository watch configured");
    assert!(
        watch
            .repositories()
            .iter()
            .any(|repository| !repository.convergence_pull_requests().is_empty()),
        "explicit targets remain configured for a later enable"
    );
    assert!(
        watch.convergence_sweep().is_none(),
        "startup has no policy from which to collect targets"
    );

    let pool = PgPoolOptions::new().connect_lazy_with(PgConnectOptions::new());
    let (nudge, _work) =
        InProcessEligibilityWorkSource::new(PostgresEligibilitySweep::new(pool.clone()));
    let unused_runtime_bounds =
        crate::ConvergenceSweepNumericBounds::new(None, None, None, None, None, None, None);
    let runtime = crate::ConvergenceSweepRuntime::try_new(
        pool,
        watch,
        crate::SessionTemplateConfiguration::default(),
        configured.clone(),
        nudge,
        unused_runtime_bounds,
    )
    .expect("disabled sweep requires no transport or database");
    assert!(
        runtime.is_none(),
        "disabled repository watch cannot start a commissioning sweep"
    );
}

#[test]
fn repository_watch_rejects_an_unknown_convergence_template() {
    let configured = HubModelConfiguration::parse(&configuration_with_convergence_sweep())
        .expect("convergence sweep fixture is valid");
    let watch = configured
        .repository_watch()
        .expect("fixture configures repository watch");
    let available = SessionTemplateName::try_new(String::from("another-template"))
        .expect("available template fixture is valid");

    assert_eq!(
        watch.validate_convergence_template(std::iter::once(&available)),
        Err(
            HubModelConfigurationError::UnknownConvergenceSweepTemplate {
                template: String::from(WATCH_TEMPLATE),
            }
        )
    );
}

#[test]
fn repository_watch_rejects_convergence_policy_without_targets() {
    let configured = configuration_with_convergence_sweep().replace(
        &format!("convergence_pull_requests = [{CONVERGENCE_PULL_REQUEST}]\n"),
        "",
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_rejects_convergence_targets_without_policy() {
    let policy = format!(
        r#"
[repository_watch.convergence_sweep]
template = "{WATCH_TEMPLATE}"
interval_seconds = {}
cool_off_seconds = {}
"#,
        example_numeric_duration("max_convergence_sweep_interval").as_secs(),
        example_numeric_duration("max_convergence_sweep_cool_off").as_secs(),
    );
    let configured = configuration_with_convergence_sweep().replace(&policy, "");

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_rejects_a_convergence_interval_above_its_ceiling() {
    let configured = configuration_with_convergence_sweep().replace(
        &format!(
            "interval_seconds = {}",
            example_numeric_duration("max_convergence_sweep_interval").as_secs()
        ),
        &format!(
            "interval_seconds = {}",
            example_numeric_duration("max_convergence_sweep_interval").as_secs() + 1
        ),
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_rejects_a_convergence_cool_off_above_its_ceiling() {
    let configured = configuration_with_convergence_sweep().replace(
        &format!(
            "cool_off_seconds = {}",
            example_numeric_duration("max_convergence_sweep_cool_off").as_secs()
        ),
        &format!(
            "cool_off_seconds = {}",
            example_numeric_duration("max_convergence_sweep_cool_off").as_secs() + 1
        ),
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_rejects_a_convergence_pull_request_above_graphql_int() {
    let configured = configuration_with_convergence_sweep().replace(
        &format!("convergence_pull_requests = [{CONVERGENCE_PULL_REQUEST}]"),
        &format!("convergence_pull_requests = [{}]", i64::from(i32::MAX) + 1),
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_accepts_a_positive_rule_revision() {
    let revision =
        RepoWatchRuleVersion::new(NonZeroU64::new(2).expect("configured revision is positive"))
            .expect("configured revision is within the durable range");
    let configured = configuration_with_repository_watch_rule().replace(
        &format!(
            "id = \"{WATCH_RULE_ID}\"\nversion = {}",
            RepoWatchRuleVersion::V1.get()
        ),
        &format!("id = \"{WATCH_RULE_ID}\"\nversion = {}", revision.get()),
    );
    let configured = HubModelConfiguration::parse(&configured)
        .expect("repository-watch revision fixture is valid");
    let rule = &configured
        .repository_watch()
        .expect("fixture configures repository watch")
        .rules()[0];

    assert_eq!(rule.id().as_str(), WATCH_RULE_ID);
    assert_eq!(rule.version(), revision);
}

#[test]
fn repository_watch_parses_the_eager_merge_forward_rule() {
    let configured = HubModelConfiguration::parse(&configuration_with_eager_merge_forward_rule())
        .expect("eager merge-forward rule fixture is valid");
    let rule = &configured
        .repository_watch()
        .expect("fixture configures repository watch")
        .rules()[0];

    assert_eq!(rule.id().as_str(), EAGER_WATCH_RULE_ID);
    assert_eq!(rule.version().get(), 1);
    assert_eq!(rule.singleton_per(), RepoWatchSingletonScope::PullRequest);
    assert_eq!(rule.cooldown(), Duration::ZERO);
    assert_eq!(
        rule.matcher().event_kinds(),
        [RepoWatchEventKindNameV1::BaseAdvanced]
    );
    assert_eq!(
        rule.matcher()
            .head_branch()
            .expect("live rule narrows dispatched pull requests")
            .as_str(),
        EAGER_WATCH_HEAD_PATTERN
    );
    assert_eq!(rule.matcher().base_branch(), None);
    assert!(rule.matcher().mergeable_state().is_empty());
    assert!(rule.matcher().conclusion().is_empty());
    assert_eq!(rule.actions()[0].template().as_str(), WATCH_TEMPLATE);
}

#[test]
fn repository_watch_rejects_a_missing_credential_file_reference() {
    let configured = configuration_with_repository_watch().replace(
        &format!("credential_file = \"{WATCH_CREDENTIAL_FILE}\""),
        "credential_path_was_omitted = true",
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_rejects_a_relative_credential_file_reference() {
    let configured = configuration_with_repository_watch()
        .replace(WATCH_CREDENTIAL_FILE, RELATIVE_WATCH_CREDENTIAL_FILE);

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_rejects_a_zero_poll_interval() {
    let configured = configuration_with_repository_watch().replace(
        &format!("poll_interval_seconds = {WATCH_INTERVAL_SECONDS}"),
        "poll_interval_seconds = 0",
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_rejects_duplicate_canonical_repositories() {
    let configured = configuration_with_repository_watch().replace(
        &format!("repository = \"{PROVIDER_SECOND_WATCH_REPOSITORY}\""),
        &format!("repository = \"{DUPLICATE_PROVIDER_WATCH_REPOSITORY}\""),
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::DuplicateWatchedRepository)
    );
}

#[test]
fn repository_watch_rejects_duplicate_canonical_signal_reviewers() {
    let configured = configuration_with_repository_watch().replace(
            &format!(
                "signal_reviewers = [\"{PROVIDER_SIGNAL_REVIEWER}\", \"{PROVIDER_SECOND_SIGNAL_REVIEWER}\"]"
            ),
            &format!(
                "signal_reviewers = [\"{PROVIDER_SIGNAL_REVIEWER}\", \"{DUPLICATE_PROVIDER_SIGNAL_REVIEWER}\"]"
            ),
        );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::DuplicateSignalReviewer)
    );
}

#[test]
fn repository_watch_rejects_a_shared_credential_file_reference() {
    let configured = configuration_with_repository_watch()
        .replace(SECOND_WATCH_CREDENTIAL_FILE, WATCH_CREDENTIAL_FILE);

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
    );
}

#[test]
fn repository_watch_rejects_a_shared_credential_file_alias() {
    let directory = tempfile::tempdir().expect("the credential fixture directory exists");
    let credential = directory.path().join("watch-token");
    std::fs::write(&credential, []).expect("the credential fixture exists");
    let alias = directory.path().join("watch-token-alias");
    std::os::unix::fs::symlink(&credential, &alias).expect("the credential alias exists");
    let configured = configuration_with_repository_watch()
        .replace(WATCH_CREDENTIAL_FILE, &credential.display().to_string())
        .replace(SECOND_WATCH_CREDENTIAL_FILE, &alias.display().to_string());

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
    );
}

#[cfg(unix)]
#[test]
fn repository_watch_rejects_a_shared_hard_linked_credential_file() {
    let directory = tempfile::tempdir().expect("the credential fixture directory exists");
    let credential = directory.path().join("watch-token");
    std::fs::write(&credential, []).expect("the credential fixture exists");
    let hard_link = directory.path().join("watch-token-hard-link");
    std::fs::hard_link(&credential, &hard_link).expect("the credential hard link exists");
    let configured = configuration_with_repository_watch()
        .replace(WATCH_CREDENTIAL_FILE, &credential.display().to_string())
        .replace(
            SECOND_WATCH_CREDENTIAL_FILE,
            &hard_link.display().to_string(),
        );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
    );
}

#[test]
fn repository_watch_rejects_a_dangling_shared_credential_file_alias() {
    let directory = tempfile::tempdir().expect("the credential fixture directory exists");
    let credential = directory.path().join("watch-token");
    let alias = directory.path().join("watch-token-alias");
    std::os::unix::fs::symlink(&credential, &alias).expect("the credential alias exists");
    let configured = configuration_with_repository_watch()
        .replace(WATCH_CREDENTIAL_FILE, &credential.display().to_string())
        .replace(SECOND_WATCH_CREDENTIAL_FILE, &alias.display().to_string());

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
    );
}

#[test]
fn repository_watch_rejects_a_dangling_intermediate_credential_alias() {
    let directory = tempfile::tempdir().expect("the credential fixture directory exists");
    let target_directory = directory.path().join("pending-target");
    let alias_directory = directory.path().join("pending-alias");
    std::os::unix::fs::symlink(&target_directory, &alias_directory)
        .expect("the intermediate credential alias exists");
    let credential = target_directory.join("watch-token");
    let alias = alias_directory.join("watch-token");
    let configured = configuration_with_repository_watch()
        .replace(WATCH_CREDENTIAL_FILE, &credential.display().to_string())
        .replace(SECOND_WATCH_CREDENTIAL_FILE, &alias.display().to_string());

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
    );
}

#[test]
fn repository_watch_defers_an_unreadable_credential_file_reference() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().expect("the credential fixture directory exists");
    let credential = directory.path().join("watch-token");
    let configured = configuration_with_repository_watch()
        .replace(WATCH_CREDENTIAL_FILE, &credential.display().to_string());
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o000))
        .expect("the credential fixture directory becomes unreadable");

    let parsed = HubModelConfiguration::parse(&configured);

    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
        .expect("the credential fixture directory becomes removable");
    parsed.expect("credential readability is deferred until request preparation");
}

#[test]
fn repository_watch_rejects_parent_components_in_credential_paths() {
    let configured = configuration_with_repository_watch().replace(
        SECOND_WATCH_CREDENTIAL_FILE,
        PARENT_COMPONENT_WATCH_CREDENTIAL_FILE,
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_rejects_unknown_repository_fields() {
    let configured = configuration_with_repository_watch().replace(
        &format!("credential_file = \"{WATCH_CREDENTIAL_FILE}\""),
        &format!(
            "credential_file = \"{WATCH_CREDENTIAL_FILE}\"\nwebhook_secret_path = \"/not-v1\""
        ),
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn repository_watch_rejects_an_unsupported_version() {
    let configured = configuration_with_repository_watch().replace(
        "[repository_watch]\nversion = 1",
        "[repository_watch]\nversion = 2",
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
    );
}

#[test]
fn tool_approval_postures_reject_a_non_table_shape() {
    let configured = CONFIGURATION.replacen(
        "version = 1",
        "version = 1\ntool_approval_postures = \"delegated\"",
        1,
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidToolApprovalPostures)
    );
}

#[test]
fn tool_approval_postures_reject_an_invalid_tool_name_key() {
    let configured = format!("{CONFIGURATION}\n[tool_approval_postures]\n\"\" = \"auto\"\n");

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidToolApprovalPostures)
    );
}

#[test]
fn tool_approval_postures_reject_an_unknown_posture() {
    let configured = format!("{CONFIGURATION}\n[tool_approval_postures]\necho = \"ask\"\n");

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidToolApprovalPostures)
    );
}

#[test]
fn tool_approval_postures_reject_a_non_string_posture() {
    let configured = format!("{CONFIGURATION}\n[tool_approval_postures]\necho = true\n");

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidToolApprovalPostures)
    );
}

#[test]
fn approval_judge_rejects_a_non_table_shape() {
    let configured =
        CONFIGURATION.replacen("version = 1", "version = 1\napproval_judge = \"same\"", 1);

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidApprovalJudge)
    );
}

#[test]
fn approval_judge_rejects_an_unknown_field() {
    let configured = format!(
        "{CONFIGURATION}\n[approval_judge]\nselection_id = \"10000000-0000-4000-8000-000000000001\"\nextra = true\n"
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidApprovalJudge)
    );
}

#[test]
fn approval_judge_rejects_a_malformed_selection_identity() {
    let configured = format!("{CONFIGURATION}\n[approval_judge]\nselection_id = \"not-a-uuid\"\n");

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidApprovalJudge)
    );
}

#[test]
fn approval_judge_rejects_an_unconfigured_direct_selection() {
    let configured = format!(
        "{CONFIGURATION}\n[approval_judge]\nselection_id = \"10000000-0000-4000-8000-000000000002\"\n"
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::DanglingApprovalJudgeSelection)
    );
}

#[test]
fn conversation_import_bound_defaults_to_256_mib() {
    let configuration =
        HubModelConfiguration::parse(CONFIGURATION).expect("the canonical configuration is valid");

    assert_eq!(
        configuration.conversation_import_max_source_bytes(),
        DEFAULT_CONVERSATION_IMPORT_MAX_SOURCE_BYTES
    );
}

#[test]
fn conversation_import_bound_accepts_an_explicit_positive_byte_count() {
    let max_source_bytes = 1_048_576;
    let configured = CONFIGURATION.replace(
        "[compaction]",
        &format!("[conversation_import]\nmax_source_bytes = {max_source_bytes}\n\n[compaction]"),
    );
    let configuration =
        HubModelConfiguration::parse(&configured).expect("the explicit import bound is valid");

    assert_eq!(
        configuration.conversation_import_max_source_bytes(),
        max_source_bytes
    );
}

#[test]
fn conversation_import_bound_rejects_zero() {
    let configured = CONFIGURATION.replace(
        "[compaction]",
        "[conversation_import]\nmax_source_bytes = 0\n\n[compaction]",
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidConversationImportLimit)
    );
}

#[test]
fn conversation_import_bound_rejects_unknown_fields() {
    let configured = CONFIGURATION.replace(
        "[compaction]",
        "[conversation_import]\nmax_source_bytes = 1048576\nextra = 1\n\n[compaction]",
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::InvalidConversationImportLimit)
    );
}

#[test]
fn retired_scheduler_table_is_an_unknown_top_level_field() {
    let configured = CONFIGURATION.replace(
        "[compaction]",
        "[scheduler]\nmax_in_flight_passes = 4\n\n[compaction]",
    );

    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::UnknownField)
    );
}

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
        configuration.web_fetch_egress_policy(),
        WebFetchEgressPolicy::try_from_allowed_origins([String::from("https://example.com")])
            .expect("fixture egress origin is valid")
    );
    let daemon_tools = configuration
        .daemon_tools()
        .expect("fixture tool mappings are complete");
    let expected_exec_supervisor = std::fs::canonicalize(EXEC_SUPERVISOR_EXECUTABLE)
        .expect("fixture supervisor path has a canonical target");
    assert_eq!(
        daemon_tools.workspace_root(),
        Path::new("/srv/signalbox/workspace")
    );
    assert_eq!(daemon_tools.github_credential_profile(), "github-primary");
    assert_eq!(daemon_tools.git_identity().name(), GIT_AUTHOR_NAME);
    assert_eq!(daemon_tools.git_identity().email(), GIT_AUTHOR_EMAIL);
    assert_eq!(
        daemon_tools.exec_supervisor_executable(),
        expected_exec_supervisor
    );
    assert_eq!(
        daemon_tools.github_egress_policy().admitted_origin(),
        "https://api.github.com"
    );
    assert_eq!(
        configuration
            .resolve_alias(alias)
            .expect("fixture alias resolves")
            .selected(),
        selection
    );
    assert_eq!(
        configuration.model_aliases().collect::<Vec<_>>(),
        vec![(alias, selection)]
    );
    assert!(
        configuration
            .target_catalog()
            .resolve(signalbox_domain::FrozenModelSelection::Direct(selection))
            .is_ok()
    );
    let route = configuration
        .resolve_direct_model(selection)
        .expect("fixture selection has an adapter route");
    assert_eq!(route.adapter(), ModelAdapter::Anthropic);
    assert_eq!(
        route.migration_credential_family(),
        Some(MIGRATED_ANTHROPIC_MODEL_FAMILY)
    );
}

fn configured_target(
    configuration: &HubModelConfiguration,
) -> signalbox_domain::ResolvedProviderTarget {
    let selection = DirectModelSelection::from_uuid(
        Uuid::parse_str("10000000-0000-4000-8000-000000000001").expect("fixture UUID is valid"),
    );
    configuration
        .resolve_direct_model(selection)
        .expect("fixture selection has a route")
        .target()
}

#[test]
fn configured_rates_fold_only_reported_axes_with_version_provenance() {
    let configuration =
        HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");
    let cost = configuration
        .derive_model_call_cost(
            configured_target(&configuration),
            "anthropic-primary",
            ModelCallInputUsage::new(
                Some(1_000_000),
                ProcessModelCallInputTokenSemantics::CacheExclusive,
            ),
            Some(2),
            None,
            Some(10),
        )
        .expect("rated reported axes derive a cost");

    assert_eq!(cost.amount_usd().to_string(), "3.000033");
    assert_eq!(cost.rate_version(), "fixture-rates-v1");
    assert_eq!(cost.billing_kind(), BillingKind::ApiMetered);
}

#[test]
fn historical_unknown_input_semantics_yield_no_dollar_figure() {
    let configuration =
        HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");

    assert_eq!(
        configuration.derive_model_call_cost(
            configured_target(&configuration),
            "anthropic-primary",
            ModelCallInputUsage::from_persisted(Some(1_000_000), None),
            Some(2),
            Some(3),
            Some(4),
        ),
        None
    );
}

#[test]
fn inexact_rate_arithmetic_yields_no_dollar_figure() {
    let configuration = HubModelConfiguration::parse(&CONFIGURATION.replace(
        "input_usd_per_million_tokens = \"3\"",
        "input_usd_per_million_tokens = \"0.0000000000000000000000000001\"",
    ))
    .expect("the representable high-precision rate is valid configuration");

    assert_eq!(
        configuration.derive_model_call_cost(
            configured_target(&configuration),
            "anthropic-primary",
            ModelCallInputUsage::new(Some(1), ProcessModelCallInputTokenSemantics::CacheExclusive),
            None,
            None,
            None,
        ),
        None
    );
}

#[test]
fn rounded_rate_multiplication_yields_no_dollar_figure() {
    let configuration = HubModelConfiguration::parse(&CONFIGURATION.replace(
        "input_usd_per_million_tokens = \"3\"",
        "input_usd_per_million_tokens = \"1.2345678901234567890123456789\"",
    ))
    .expect("the representable high-precision rate is valid configuration");

    assert_eq!(
        configuration.derive_model_call_cost(
            configured_target(&configuration),
            "anthropic-primary",
            ModelCallInputUsage::new(
                Some(u64::MAX),
                ProcessModelCallInputTokenSemantics::CacheExclusive
            ),
            None,
            None,
            None,
        ),
        None
    );
}

#[test]
fn cost_sum_that_loses_an_earlier_axis_yields_no_dollar_figure() {
    let configuration = HubModelConfiguration::parse(
        &CONFIGURATION
            .replace(
                "input_usd_per_million_tokens = \"3\"",
                "input_usd_per_million_tokens = \"0.0000000000000000000000000001\"",
            )
            .replace(
                "output_usd_per_million_tokens = \"15\"",
                "output_usd_per_million_tokens = \"10000000000000000000000000000\"",
            ),
    )
    .expect("both extreme rates are representable configuration values");

    assert_eq!(
        configuration.derive_model_call_cost(
            configured_target(&configuration),
            "anthropic-primary",
            ModelCallInputUsage::new(
                Some(1_000_000),
                ProcessModelCallInputTokenSemantics::CacheExclusive
            ),
            Some(1),
            None,
            None,
        ),
        None
    );
}

#[test]
fn exact_rate_multiplication_may_reduce_scale() {
    let configuration = HubModelConfiguration::parse(&CONFIGURATION.replace(
        "input_usd_per_million_tokens = \"3\"",
        "input_usd_per_million_tokens = \"7922816251426433759354395033.5\"",
    ))
    .expect("the representable high-precision rate is valid configuration");

    let cost = configuration
        .derive_model_call_cost(
            configured_target(&configuration),
            "anthropic-primary",
            ModelCallInputUsage::new(
                Some(10),
                ProcessModelCallInputTokenSemantics::CacheExclusive,
            ),
            None,
            None,
            None,
        )
        .expect("exact multiplication may reduce decimal scale");
    let expected = Decimal::MAX
        .checked_div(Decimal::from(1_000_000_u64))
        .expect("fixture quotient is representable");

    assert_eq!(cost.amount_usd(), expected);
}

#[test]
fn widened_usage_aggregate_cost_prices_totals_above_u64() {
    let configuration =
        HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");
    let cost = configuration
        .derive_usage_aggregate_cost(
            configured_target(&configuration),
            "anthropic-primary",
            ProcessModelCallInputTokenSemantics::CacheExclusive,
            [None, Some(u128::from(u64::MAX) + 1), None, None],
        )
        .expect("the widened output total is exactly priceable");

    assert!(cost.amount_usd() > Decimal::ZERO);
}

#[test]
fn widened_usage_aggregate_cost_fails_whole_when_one_reported_axis_is_unpriceable() {
    let configuration =
        HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");
    let cost = configuration.derive_usage_aggregate_cost(
        configured_target(&configuration),
        "anthropic-primary",
        ProcessModelCallInputTokenSemantics::CacheExclusive,
        [Some(u128::MAX), Some(1_000_000), None, None],
    );

    assert_eq!(cost, None, "an unpriceable input axis must not be dropped");
}

#[test]
fn an_unrated_model_yields_no_dollar_figure() {
    let unrated = CONFIGURATION
        .lines()
        .filter(|line| {
            !line.starts_with("rate_version") && !line.contains("usd_per_million_tokens")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let configuration =
        HubModelConfiguration::parse(&unrated).expect("an unrated model remains valid");

    assert_eq!(
        configuration.derive_model_call_cost(
            configured_target(&configuration),
            "anthropic-primary",
            ModelCallInputUsage::new(Some(1), ProcessModelCallInputTokenSemantics::CacheExclusive),
            Some(1),
            Some(1),
            Some(1),
        ),
        None
    );
}

#[test]
fn one_target_cannot_mix_rated_and_unrated_model_entries() {
    let conflicting = format!(
        r#"{CONFIGURATION}
[[models]]
selection_id = "10000000-0000-4000-8000-000000000002"
target_id = "20000000-0000-4000-8000-000000000001"
model_family = "anthropic"
provider_model = "claude-example"
max_output_tokens = 256
context_window_tokens = 200000
"#
    );

    assert_eq!(
        HubModelConfiguration::parse(&conflicting).err(),
        Some(HubModelConfigurationError::ConflictingTarget)
    );
}

#[test]
fn a_partial_model_rate_set_is_rejected() {
    let partial = CONFIGURATION.replace("cache_read_input_usd_per_million_tokens = \"0.30\"\n", "");

    assert_eq!(
        HubModelConfiguration::parse(&partial).err(),
        Some(HubModelConfigurationError::IncompleteBillingRates)
    );
}

#[test]
fn configuration_rejects_a_rate_that_requires_rounding() {
    let too_precise = CONFIGURATION.replace(
        "input_usd_per_million_tokens = \"3\"",
        "input_usd_per_million_tokens = \"0.00000000000000000000000000001\"",
    );

    assert_eq!(
        HubModelConfiguration::parse(&too_precise).err(),
        Some(HubModelConfigurationError::InvalidBillingRate)
    );
}

#[test]
fn cost_label_follows_the_credential_profile() {
    let configuration =
        HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");
    let cost = configuration
        .derive_model_call_cost(
            configured_target(&configuration),
            "codex-subscription-primary",
            ModelCallInputUsage::new(Some(1), ProcessModelCallInputTokenSemantics::CacheExclusive),
            None,
            None,
            None,
        )
        .expect("the historical subscription profile is declared");

    assert_eq!(cost.billing_kind(), BillingKind::Subscription);
}

#[test]
fn codex_cli_on_an_api_metered_profile_derives_real_cost() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let executable = std::env::current_exe().expect("the test executable has a path");
    let configuration = HubModelConfiguration::parse(&configuration_with_api_metered_codex_model(
        &executable,
        temporary.path(),
    ))
    .expect("the API-metered Codex fixture is valid");
    let selection = DirectModelSelection::from_uuid(
        Uuid::parse_str("10000000-0000-4000-8000-000000000002").expect("fixture UUID is valid"),
    );
    let route = configuration
        .resolve_direct_model(selection)
        .expect("the Codex fixture has a route");
    let cost = configuration
        .derive_model_call_cost(
            route.target(),
            route.credential_profile(),
            ModelCallInputUsage::new(Some(1), ProcessModelCallInputTokenSemantics::CacheInclusive),
            None,
            Some(0),
            Some(0),
        )
        .expect("the API-metered Codex fixture has rates");

    assert_eq!(route.adapter(), ModelAdapter::CodexCli);
    assert_eq!(route.credential_profile(), "codex-api-primary");
    assert_eq!(cost.billing_kind(), BillingKind::ApiMetered);
}

#[test]
fn codex_cache_breakdowns_are_not_charged_twice_as_input() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let executable = std::env::current_exe().expect("the test executable has a path");
    let configuration = HubModelConfiguration::parse(&configuration_with_api_metered_codex_model(
        &executable,
        temporary.path(),
    ))
    .expect("the API-metered Codex fixture is valid");
    let selection = DirectModelSelection::from_uuid(
        Uuid::parse_str("10000000-0000-4000-8000-000000000002").expect("fixture UUID is valid"),
    );
    let route = configuration
        .resolve_direct_model(selection)
        .expect("the Codex fixture has a route");
    let cost = configuration
        .derive_model_call_cost(
            route.target(),
            route.credential_profile(),
            ModelCallInputUsage::new(
                Some(1_000_000),
                ProcessModelCallInputTokenSemantics::CacheInclusive,
            ),
            None,
            Some(100_000),
            Some(200_000),
        )
        .expect("the consistent Codex breakdown derives a cost");

    assert_eq!(cost.amount_usd().to_string(), "1.8");
}

#[test]
fn codex_unreported_cache_axis_suppresses_only_ordinary_input_cost() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let executable = std::env::current_exe().expect("the test executable has a path");
    let configuration = HubModelConfiguration::parse(&configuration_with_api_metered_codex_model(
        &executable,
        temporary.path(),
    ))
    .expect("the API-metered Codex fixture is valid");
    let selection = DirectModelSelection::from_uuid(
        Uuid::parse_str("10000000-0000-4000-8000-000000000002").expect("fixture UUID is valid"),
    );
    let route = configuration
        .resolve_direct_model(selection)
        .expect("the Codex fixture has a route");
    let partial_breakdown = configuration
        .derive_model_call_cost(
            route.target(),
            route.credential_profile(),
            ModelCallInputUsage::new(
                Some(1_000_000),
                ProcessModelCallInputTokenSemantics::CacheInclusive,
            ),
            None,
            Some(100_000),
            None,
        )
        .expect("the independently reported cache axis derives a cost");
    let cache_axis_only = configuration
        .derive_model_call_cost(
            route.target(),
            route.credential_profile(),
            ModelCallInputUsage::new(None, ProcessModelCallInputTokenSemantics::CacheInclusive),
            None,
            Some(100_000),
            None,
        )
        .expect("the independently reported cache axis derives the reference cost");

    assert_eq!(partial_breakdown, cache_axis_only);
}

#[test]
fn historical_input_semantics_survive_an_adapter_route_change() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let executable = std::env::current_exe().expect("the test executable has a path");
    let configuration = HubModelConfiguration::parse(&configuration_with_api_metered_codex_model(
        &executable,
        temporary.path(),
    ))
    .expect("the API-metered Codex fixture is valid");
    let selection = DirectModelSelection::from_uuid(
        Uuid::parse_str("10000000-0000-4000-8000-000000000002").expect("fixture UUID is valid"),
    );
    let route = configuration
        .resolve_direct_model(selection)
        .expect("the Codex fixture has a route");
    let cost = configuration
        .derive_model_call_cost(
            route.target(),
            route.credential_profile(),
            ModelCallInputUsage::new(
                Some(1_000_000),
                ProcessModelCallInputTokenSemantics::CacheExclusive,
            ),
            None,
            Some(100_000),
            Some(200_000),
        )
        .expect("historically exclusive input axes derive a cost");

    assert_eq!(route.adapter(), ModelAdapter::CodexCli);
    assert_eq!(cost.amount_usd().to_string(), "2.1");
}

#[test]
fn absent_tool_mappings_preserve_the_base_daemon_catalog() {
    let configuration = HubModelConfiguration::parse(&configuration_without_tool_mappings())
        .expect("model configuration without tool mappings remains parseable");

    assert_eq!(configuration.daemon_tools(), None);
}

#[test]
fn tool_mapping_registry_rejects_a_duplicate_family() {
    let duplicate = format!(
        "{CONFIGURATION}\n[[tool_mappings]]\nfamily = \"github\"\nadapter = \"github\"\ncredential_profile = \"github-primary\"\negress_policy = \"github_api_only\"\n"
    );

    assert_eq!(
        HubModelConfiguration::parse(&duplicate).err(),
        Some(HubModelConfigurationError::DuplicateToolFamily)
    );
}

#[test]
fn tool_mapping_registry_rejects_an_unpinned_workspace_root() {
    let relative = CONFIGURATION.replace(
        "workspace_root = \"/srv/signalbox/workspace\"",
        "workspace_root = \"relative/workspace\"",
    );

    assert_eq!(
        HubModelConfiguration::parse(&relative).err(),
        Some(HubModelConfigurationError::InvalidToolMappings)
    );
}

#[test]
fn tool_mapping_registry_rejects_noncanonical_workspace_root_spellings() {
    let trailing_separator = CONFIGURATION.replace(
        "workspace_root = \"/srv/signalbox/workspace\"",
        "workspace_root = \"/srv/signalbox/workspace/\"",
    );
    let dot_component = CONFIGURATION.replace(
        "workspace_root = \"/srv/signalbox/workspace\"",
        "workspace_root = \"/srv/signalbox/./workspace\"",
    );

    assert_eq!(
        HubModelConfiguration::parse(&trailing_separator).err(),
        Some(HubModelConfigurationError::InvalidToolMappings)
    );
    assert_eq!(
        HubModelConfiguration::parse(&dot_component).err(),
        Some(HubModelConfigurationError::InvalidToolMappings)
    );
}

#[test]
fn tool_mapping_registry_requires_git_identity() {
    let missing = CONFIGURATION.replace(
            "[git_identity]\nauthor_name = \"Signalbox Daemon\"\nauthor_email = \"signalbox@example.test\"\n\n",
            "",
        );

    assert_eq!(
        HubModelConfiguration::parse(&missing).err(),
        Some(HubModelConfigurationError::MissingGitIdentityConfiguration)
    );
}

#[test]
fn git_identity_rejects_an_unsafe_value() {
    let invalid = CONFIGURATION.replace(
        "author_email = \"signalbox@example.test\"",
        "author_email = \"signalbox@example.test>\"",
    );

    assert_eq!(
        HubModelConfiguration::parse(&invalid).err(),
        Some(HubModelConfigurationError::InvalidGitIdentityConfiguration)
    );
}

#[test]
fn git_identity_rejects_an_unknown_field() {
    let unknown = CONFIGURATION.replace(
        "author_email = \"signalbox@example.test\"",
        "author_email = \"signalbox@example.test\"\ncommitter_name = \"Ambient User\"",
    );

    assert_eq!(
        HubModelConfiguration::parse(&unknown).err(),
        Some(HubModelConfigurationError::InvalidGitIdentityConfiguration)
    );
}

#[test]
fn tool_mapping_registry_requires_daemon_tool_process_settings() {
    let missing = CONFIGURATION.replace(
        &format!(
            "[daemon_tools]\nexec_supervisor_executable = \"{EXEC_SUPERVISOR_EXECUTABLE}\"\nsandboxed_exec_timeout_bound = \"none\"\n\n"
        ),
        "",
    );

    assert_eq!(
        HubModelConfiguration::parse(&missing).err(),
        Some(HubModelConfigurationError::MissingDaemonToolSettings)
    );
}

#[test]
fn daemon_tool_process_settings_reject_a_relative_supervisor() {
    let relative = CONFIGURATION.replace(
        &format!("exec_supervisor_executable = \"{EXEC_SUPERVISOR_EXECUTABLE}\""),
        "exec_supervisor_executable = \"relative/signalbox-exec-supervisor\"",
    );

    assert_eq!(
        HubModelConfiguration::parse(&relative).err(),
        Some(HubModelConfigurationError::InvalidDaemonToolSettings)
    );
}

#[test]
fn daemon_tool_process_settings_reject_a_missing_supervisor() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let missing_supervisor = temporary.path().join("missing-supervisor");
    let missing = CONFIGURATION.replace(
        EXEC_SUPERVISOR_EXECUTABLE,
        missing_supervisor
            .to_str()
            .expect("fixture path is UTF-8 representable"),
    );

    assert_eq!(
        HubModelConfiguration::parse(&missing).err(),
        Some(HubModelConfigurationError::InvalidDaemonToolSettings)
    );
}

#[test]
fn daemon_sandboxed_exec_timeout_bound_accepts_none_and_a_friendly_duration() {
    let unbounded = HubModelConfiguration::parse(CONFIGURATION).expect("fixture parses");
    assert_eq!(
        unbounded
            .daemon_tools()
            .expect("mapped fixture has daemon tool settings")
            .sandboxed_exec_timeout_bound(),
        None
    );

    let finite = CONFIGURATION.replace(
        "sandboxed_exec_timeout_bound = \"none\"",
        "sandboxed_exec_timeout_bound = \"20 minutes\"",
    );
    assert_eq!(
        HubModelConfiguration::parse(&finite)
            .expect("friendly timeout bound parses")
            .daemon_tools()
            .expect("mapped fixture has daemon tool settings")
            .sandboxed_exec_timeout_bound(),
        Some(std::time::Duration::from_secs(20 * 60))
    );
}

#[test]
fn daemon_sandboxed_exec_timeout_bound_is_required_and_at_least_one_second_when_finite() {
    for replacement in [
        "",
        "sandboxed_exec_timeout_bound = \"0s\"",
        "sandboxed_exec_timeout_bound = \"500ms\"",
        "sandboxed_exec_timeout_bound = 120",
    ] {
        let configured =
            CONFIGURATION.replace("sandboxed_exec_timeout_bound = \"none\"", replacement);
        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidDaemonToolSettings),
            "{replacement}"
        );
    }
}

#[cfg(unix)]
#[test]
fn daemon_tool_process_settings_canonicalize_a_supervisor_symlink() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let supervisor_link = temporary.path().join("signalbox-exec-supervisor");
    std::os::unix::fs::symlink(EXEC_SUPERVISOR_EXECUTABLE, &supervisor_link)
        .expect("fixture supervisor symlink is created");
    let linked = CONFIGURATION.replace(
        EXEC_SUPERVISOR_EXECUTABLE,
        supervisor_link
            .to_str()
            .expect("fixture path is UTF-8 representable"),
    );
    let expected = std::fs::canonicalize(&supervisor_link)
        .expect("fixture supervisor symlink has a canonical target");

    let configuration =
        HubModelConfiguration::parse(&linked).expect("an absolute supervisor symlink is valid");

    assert_eq!(
        configuration
            .daemon_tools()
            .expect("mapped fixture has daemon tool settings")
            .exec_supervisor_executable(),
        expected
    );
}

#[test]
fn daemon_tool_process_settings_admit_a_canonical_cargo_registry_cache() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let registry = temporary.path().join("registry");
    std::fs::create_dir(&registry).expect("fixture registry exists");
    let configured = CONFIGURATION.replace(
            &format!("exec_supervisor_executable = \"{EXEC_SUPERVISOR_EXECUTABLE}\""),
            &format!(
                "exec_supervisor_executable = \"{EXEC_SUPERVISOR_EXECUTABLE}\"\ncargo_registry_cache = \"{}\"",
                registry.display()
            ),
        );
    let expected =
        std::fs::canonicalize(&registry).expect("fixture registry has a canonical directory");

    let configuration = HubModelConfiguration::parse(&configured)
        .expect("an absolute Cargo registry directory is valid");

    assert_eq!(
        configuration
            .daemon_tools()
            .expect("mapped fixture has daemon tool settings")
            .cargo_registry_cache(),
        Some(expected.as_path())
    );
}

#[test]
fn daemon_tool_process_settings_reject_an_unknown_field() {
    let unknown = CONFIGURATION.replace(
            &format!("exec_supervisor_executable = \"{EXEC_SUPERVISOR_EXECUTABLE}\""),
            &format!(
                "exec_supervisor_executable = \"{EXEC_SUPERVISOR_EXECUTABLE}\"\nderive_from_daemon = true"
            ),
        );

    assert_eq!(
        HubModelConfiguration::parse(&unknown).err(),
        Some(HubModelConfigurationError::InvalidDaemonToolSettings)
    );
}

#[test]
fn configuration_rejects_an_adapter_the_build_does_not_provide() {
    let unsupported_adapter_name = "openai_http";
    let unsupported_adapter = CONFIGURATION.replace(
        "adapter = \"anthropic\"",
        &format!("adapter = \"{unsupported_adapter_name}\""),
    );

    assert_eq!(
        HubModelConfiguration::parse(&unsupported_adapter).err(),
        Some(HubModelConfigurationError::UnsupportedAdapter {
            adapter: Arc::from(unsupported_adapter_name),
        })
    );
}

#[test]
fn configuration_rejects_a_mapping_naming_an_undeclared_pool() {
    let undeclared_pool_name = "undeclared-pool";
    let undeclared_pool = CONFIGURATION.replace(
        "credential_pool = \"anthropic-main\"",
        &format!("credential_pool = \"{undeclared_pool_name}\""),
    );

    assert_eq!(
        HubModelConfiguration::parse(&undeclared_pool).err(),
        Some(HubModelConfigurationError::UnknownCredentialPool {
            model_family: Arc::from("anthropic"),
            credential_pool: Arc::from(undeclared_pool_name),
        })
    );
}

#[test]
fn configuration_rejects_a_pool_member_naming_an_undeclared_profile() {
    let undeclared_profile_name = "undeclared-profile";
    let undeclared_profile = CONFIGURATION.replace(
        "members = [{ profile = \"anthropic-primary\", priority = 1 }]",
        &format!("members = [{{ profile = \"{undeclared_profile_name}\", priority = 1 }}]"),
    );

    assert_eq!(
        HubModelConfiguration::parse(&undeclared_profile).err(),
        Some(HubModelConfigurationError::UnknownPoolMemberProfile {
            credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
            credential_profile: Arc::from(undeclared_profile_name),
        })
    );
}

#[test]
fn configuration_admits_a_profile_name_no_build_constant_states() {
    let deployment_chosen_name = "acct-7f3";
    let renamed = CONFIGURATION.replace("anthropic-primary", deployment_chosen_name);
    let configured =
        HubModelConfiguration::parse(&renamed).expect("a deployment names its own accounts");

    let route = configured
        .resolve_direct_model(configured_judge_selection_fixture())
        .expect("the configured selection resolves");

    assert_eq!(route.credential_profile(), deployment_chosen_name);
}

#[test]
fn route_pins_the_preferred_member_of_its_pool() {
    let pool_name = "anthropic-main";
    let preferred_profile = ANTHROPIC_CREDENTIAL_REFERENCE;
    let pool = format!(
        r#"[[credential_pools]]
name = "{pool_name}"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [
  {{ profile = "anthropic-overflow", priority = 2 }},
  {{ profile = "{preferred_profile}", priority = 1 }},
]"#
    );
    let configured = HubModelConfiguration::parse(&configuration_with_anthropic_pool(&pool))
        .expect("a two-member pool is valid");

    let route = configured
        .resolve_direct_model(configured_judge_selection_fixture())
        .expect("the configured selection resolves");

    assert_eq!(route.credential_pool(), pool_name);
    assert_eq!(route.credential_profile(), preferred_profile);
}

#[test]
fn equal_priorities_resolve_to_the_first_listed_member() {
    let first_listed_profile = ANTHROPIC_OVERFLOW_PROFILE;
    let pool = format!(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [
  {{ profile = "{first_listed_profile}", priority = 1 }},
  {{ profile = "anthropic-primary", priority = 1 }},
]"#,
    );
    let configured = HubModelConfiguration::parse(&configuration_with_anthropic_pool(&pool))
        .expect("equal priorities are valid");

    let route = configured
        .resolve_direct_model(configured_judge_selection_fixture())
        .expect("the configured selection resolves");

    assert_eq!(route.credential_profile(), first_listed_profile);
}

#[test]
fn omitted_trigger_keys_select_the_staying_action() {
    let configured =
        HubModelConfiguration::parse(CONFIGURATION).expect("the fixture omits every trigger");

    let pool = configured
        .credential_pool("anthropic-main")
        .expect("the fixture declares the pool");

    assert_eq!(
        pool.action(CredentialPoolTrigger::QuotaExhausted),
        CredentialPoolAction::Stay
    );
    assert_eq!(
        pool.action(CredentialPoolTrigger::RateLimited),
        CredentialPoolAction::Stay
    );
    assert_eq!(
        pool.action(CredentialPoolTrigger::Overloaded),
        CredentialPoolAction::Stay
    );
    assert_eq!(
        pool.action(CredentialPoolTrigger::CredentialRejected),
        CredentialPoolAction::Stay
    );
    assert_eq!(
        pool.action(CredentialPoolTrigger::HeadroomLow),
        CredentialPoolAction::Stay
    );
}

#[test]
fn configured_trigger_actions_are_typed() {
    let configured = HubModelConfiguration::parse(&configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "fail"
members = [{ profile = "anthropic-primary", priority = 1 }]
on_quota_exhausted = "switch_next_turn"
on_rate_limited = "switch_now"
on_overloaded = "avoid_new_sessions"
on_credential_rejected = "quarantine""#,
    ))
    .expect("every configured action is admitted for its trigger");

    let pool = configured
        .credential_pool("anthropic-main")
        .expect("the fixture declares the pool");

    assert_eq!(pool.tie_break(), CredentialPoolTieBreak::FirstListed);
    assert_eq!(pool.on_pool_exhausted(), CredentialPoolExhaustion::Fail);
    // Anthropic's mapping has no quota token, so only rate limiting can
    // carry the proof `switch_now` requires.
    assert_eq!(
        pool.action(CredentialPoolTrigger::QuotaExhausted),
        CredentialPoolAction::SwitchNextTurn
    );
    assert_eq!(
        pool.action(CredentialPoolTrigger::RateLimited),
        CredentialPoolAction::SwitchNow
    );
    assert_eq!(
        pool.action(CredentialPoolTrigger::Overloaded),
        CredentialPoolAction::AvoidNewSessions
    );
    assert_eq!(
        pool.action(CredentialPoolTrigger::CredentialRejected),
        CredentialPoolAction::Quarantine
    );
}

#[test]
fn configuration_requires_at_least_one_credential_pool() {
    let without_pools = configuration_with_anthropic_pool("").replace(CODEX_POOL, "");

    assert_eq!(
        HubModelConfiguration::parse(&without_pools).err(),
        Some(HubModelConfigurationError::MissingCredentialPools)
    );
}

#[test]
fn configuration_rejects_an_oversized_credential_profile_name() {
    let oversized_name = "p".repeat(MAX_CREDENTIAL_CATALOG_NAME_UTF8_BYTES + 1);
    let configuration = CONFIGURATION.replacen(
        "name = \"anthropic-primary\"",
        &format!("name = \"{oversized_name}\""),
        1,
    );

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidField)
    );
}

#[test]
fn configuration_rejects_an_oversized_credential_pool_name() {
    let oversized_name = "p".repeat(MAX_CREDENTIAL_CATALOG_NAME_UTF8_BYTES + 1);
    let configuration = CONFIGURATION.replacen(
        "name = \"anthropic-main\"",
        &format!("name = \"{oversized_name}\""),
        1,
    );

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidField)
    );
}

#[test]
fn configuration_rejects_too_many_credential_pool_members() {
    let repeated_member = "{ profile = \"anthropic-primary\", priority = 1 }";
    let members = vec![repeated_member; MAX_CREDENTIAL_POOL_MEMBERS + 1].join(",\n");
    let oversized_pool = configuration_with_anthropic_pool(&format!(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{members}]"#,
    ));

    assert_eq!(
        HubModelConfiguration::parse(&oversized_pool).err(),
        Some(HubModelConfigurationError::InvalidCredentialPoolPolicy)
    );
}

#[test]
fn configuration_rejects_a_pool_with_no_members() {
    let empty_pool = configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = []"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&empty_pool).err(),
        Some(HubModelConfigurationError::EmptyCredentialPool {
            credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
        })
    );
}

#[test]
fn configuration_rejects_a_repeated_pool_member() {
    let repeated_member = configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [
  { profile = "anthropic-primary", priority = 1 },
  { profile = "anthropic-primary", priority = 2 },
]"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&repeated_member).err(),
        Some(HubModelConfigurationError::DuplicatePoolMember {
            credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
            credential_profile: Arc::from("anthropic-primary"),
        })
    );
}

#[test]
fn configuration_rejects_a_repeated_pool_name() {
    let repeated_pool = configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]

[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-overflow", priority = 1 }]"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&repeated_pool).err(),
        Some(HubModelConfigurationError::DuplicateCredentialPool {
            credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
        })
    );
}

#[test]
fn configuration_rejects_a_member_priority_below_one() {
    let zero_priority = configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 0 }]"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&zero_priority).err(),
        Some(HubModelConfigurationError::InvalidMemberPriority {
            credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
        })
    );
}

#[test]
fn configuration_rejects_a_member_priority_above_u32() {
    let overflowing_priority = configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 4294967296 }]"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&overflowing_priority).err(),
        Some(HubModelConfigurationError::InvalidMemberPriority {
            credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
        })
    );
}

#[test]
fn configuration_rejects_pool_members_disagreeing_on_adapter() {
    let mixed_adapters = configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [
  { profile = "anthropic-primary", priority = 1 },
  { profile = "codex-subscription-primary", priority = 2 },
]"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&mixed_adapters).err(),
        Some(HubModelConfigurationError::ConflictingPoolAdapters {
            credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
        })
    );
}

#[test]
fn configuration_rejects_a_mapping_disagreeing_with_its_pool_adapter() {
    let disagreeing_mapping = CONFIGURATION.replace(
        "adapter = \"anthropic\"\ncredential_pool = \"anthropic-main\"",
        "adapter = \"codex_cli\"\ncredential_pool = \"anthropic-main\"",
    );

    assert_eq!(
        HubModelConfiguration::parse(&disagreeing_mapping).err(),
        Some(HubModelConfigurationError::ConflictingPoolAdapters {
            credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
        })
    );
}

#[test]
fn configuration_allows_a_direct_adapter_to_resolve_multiple_profiles() {
    let second_anthropic_pool = format!(
        r#"{CONFIGURATION}
[[credential_pools]]
name = "anthropic-batch"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{{ profile = "anthropic-overflow", priority = 1 }}]

[[adapter_mappings]]
model_family = "anthropic-batch"
adapter = "anthropic"
credential_pool = "anthropic-batch"
"#
    );

    let configuration = HubModelConfiguration::parse(&second_anthropic_pool)
        .expect("direct HTTP profiles resolve per operation");

    assert_eq!(
        configuration
            .session_credential_pin()
            .credentials()
            .map(|credential| credential.credential_reference())
            .collect::<Vec<_>>(),
        vec!["anthropic-primary", "anthropic-overflow"]
    );
}

#[test]
fn configuration_rejects_an_unknown_tie_break() {
    let unknown_tie_break = configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "coin_flip"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&unknown_tie_break).err(),
        Some(HubModelConfigurationError::InvalidCredentialPoolPolicy)
    );
}

#[test]
fn configuration_rejects_round_robin_until_its_durable_cursor_exists() {
    let round_robin = configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "round_robin"
on_pool_exhausted = "fail"
members = [{ profile = "anthropic-primary", priority = 1 }]"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&round_robin).err(),
        Some(HubModelConfigurationError::InvalidCredentialPoolPolicy)
    );
}

#[test]
fn configuration_rejects_an_unknown_exhaustion_behavior() {
    let unknown_exhaustion = configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "retry_forever"
members = [{ profile = "anthropic-primary", priority = 1 }]"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&unknown_exhaustion).err(),
        Some(HubModelConfigurationError::InvalidCredentialPoolPolicy)
    );
}

#[test]
fn configuration_rejects_an_unknown_trigger_action() {
    let unknown_action = configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]
on_rate_limited = "escalate""#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&unknown_action).err(),
        Some(HubModelConfigurationError::UnknownCredentialPoolAction)
    );
}

#[test]
fn configuration_admits_switching_now_on_a_rejected_credential() {
    let switching_now = configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]
on_credential_rejected = "switch_now""#,
    );

    HubModelConfiguration::parse(&switching_now)
        .expect("credential rejection authorizes immediate rotation");
}

#[test]
fn configuration_rejects_switching_now_on_low_headroom() {
    let switching_now = configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]
on_headroom_low = "switch_now""#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&switching_now).err(),
        Some(
            HubModelConfigurationError::InadmissibleCredentialPoolAction {
                trigger: Arc::from("on_headroom_low"),
            }
        )
    );
}

#[test]
fn configuration_rejects_a_headroom_reserve_without_adapter_capacity() {
    let reserved = configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
headroom_reserve_percent = 10
members = [{ profile = "anthropic-primary", priority = 1 }]"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&reserved).err(),
        Some(HubModelConfigurationError::UnobservedCapacityPolicy {
            credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
        })
    );
}

#[test]
fn configuration_rejects_a_mistyped_headroom_reserve_table() {
    let reserved = configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]

[credential_pools.headroom_reserve_percent]
value = 10"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&reserved).err(),
        Some(HubModelConfigurationError::InvalidField)
    );
}

#[test]
fn configuration_rejects_a_member_headroom_reserve_without_adapter_capacity() {
    let reserved = configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1, headroom_reserve_percent = 10 }]"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&reserved).err(),
        Some(HubModelConfigurationError::UnobservedCapacityPolicy {
            credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
        })
    );
}

#[test]
fn configuration_admits_switch_now_for_a_codex_terminal_failure() {
    let substituting = CONFIGURATION.replace(
        CODEX_POOL,
        &format!("{CODEX_POOL}\non_rate_limited = \"switch_now\""),
    );

    HubModelConfiguration::parse(&substituting)
        .expect("Codex typed failed-turn proof permits availability successors");
}

#[test]
fn configuration_admits_switch_now_where_the_adapter_proves_non_acceptance() {
    let substituting = configuration_with_anthropic_pool(&format!(
        "{ANTHROPIC_POOL}\non_rate_limited = \"switch_now\""
    ));

    HubModelConfiguration::parse(&substituting)
        .expect("a decoded native envelope authorizes the successor for this adapter");
}

#[test]
fn configuration_rejects_switch_now_for_a_cause_the_adapter_cannot_prove() {
    // Anthropic's mapping has no quota token, so this pair could reach
    // `switch_now` only through a status-derived fallback carrying no proof.
    let substituting = configuration_with_anthropic_pool(&format!(
        "{ANTHROPIC_POOL}\non_quota_exhausted = \"switch_now\""
    ));

    assert_eq!(
        HubModelConfiguration::parse(&substituting).err(),
        Some(HubModelConfigurationError::UnprovableSubstitutionPolicy {
            credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
        })
    );
}

#[test]
fn configuration_rejects_least_used_ties_without_adapter_capacity() {
    let least_used = configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "least_used"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&least_used).err(),
        Some(HubModelConfigurationError::UnobservedCapacityPolicy {
            credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
        })
    );
}

#[test]
fn configuration_rejects_a_headroom_reserve_leaving_nothing_usable() {
    let full_reserve = configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
headroom_reserve_percent = 100
members = [{ profile = "anthropic-primary", priority = 1 }]"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&full_reserve).err(),
        Some(HubModelConfigurationError::InvalidHeadroomReserve)
    );
}

#[test]
fn configuration_rejects_an_unknown_pool_field() {
    let unknown_field = configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
on_provider_moody = "quarantine"
members = [{ profile = "anthropic-primary", priority = 1 }]"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&unknown_field).err(),
        Some(HubModelConfigurationError::UnknownField)
    );
}

#[test]
fn configuration_rejects_an_unknown_pool_member_field() {
    let unknown_field = configuration_with_anthropic_pool(
        r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1, weight = 3 }]"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&unknown_field).err(),
        Some(HubModelConfigurationError::UnknownField)
    );
}

#[test]
fn configuration_rejects_a_delivery_its_adapter_does_not_admit() {
    let ambient_anthropic = CONFIGURATION.replace(
        "delivery = \"file\"\nfile = \"/run/secrets/anthropic-primary\"",
        "delivery = \"ambient\"",
    );

    assert_eq!(
        HubModelConfiguration::parse(&ambient_anthropic).err(),
        Some(HubModelConfigurationError::UnsupportedCredentialDelivery {
            adapter: ModelAdapter::Anthropic,
            delivery: Arc::from("ambient"),
        })
    );
}

#[test]
fn configuration_admits_an_existing_nonempty_credential_home() {
    let temporary = tempfile::tempdir().expect("synthetic home root is created");
    let home = temporary.path().join("account-a");
    std::fs::create_dir(&home).expect("synthetic home is created");
    std::fs::write(home.join("fixture-marker"), "synthetic").expect("synthetic home is nonempty");
    let credential_home = CONFIGURATION.replace(
        "delivery = \"ambient\"",
        &format!(
            "delivery = \"codex_home\"\ncodex_home = {:?}",
            home.to_string_lossy()
        ),
    );

    let parsed = HubModelConfiguration::parse(&credential_home)
        .expect("existing nonempty synthetic home is admitted");
    assert_eq!(
        parsed
            .credential_profile(CODEX_SUBSCRIPTION_PROFILE)
            .expect("Codex profile remains present")
            .delivery()
            .path(),
        Some(&home)
    );
}

#[test]
fn configuration_rejects_a_relative_credential_home_with_a_typed_member_error() {
    let credential_home = CONFIGURATION.replace(
        "delivery = \"ambient\"",
        "delivery = \"codex_home\"\ncodex_home = \"relative/account-a\"",
    );

    assert_eq!(
        HubModelConfiguration::parse(&credential_home).err(),
        Some(HubModelConfigurationError::InvalidCredentialHome {
            credential_profile: Arc::from(CODEX_SUBSCRIPTION_PROFILE),
            failure: crate::CredentialHomeAdmissionFailure::InvalidPath,
        })
    );
}

#[test]
fn configuration_rejects_a_missing_credential_home_with_a_typed_member_error() {
    let temporary = tempfile::tempdir().expect("synthetic home root is created");
    let missing = temporary.path().join("missing-account");
    let credential_home = CONFIGURATION.replace(
        "delivery = \"ambient\"",
        &format!(
            "delivery = \"codex_home\"\ncodex_home = {:?}",
            missing.to_string_lossy()
        ),
    );

    assert_eq!(
        HubModelConfiguration::parse(&credential_home).err(),
        Some(HubModelConfigurationError::InvalidCredentialHome {
            credential_profile: Arc::from(CODEX_SUBSCRIPTION_PROFILE),
            failure: crate::CredentialHomeAdmissionFailure::MissingOrNotDirectory,
        })
    );
}

#[test]
fn configuration_rejects_an_empty_credential_home_with_a_typed_member_error() {
    let temporary = tempfile::tempdir().expect("synthetic home root is created");
    let empty = temporary.path().join("empty-account");
    std::fs::create_dir(&empty).expect("empty synthetic home is created");
    let credential_home = CONFIGURATION.replace(
        "delivery = \"ambient\"",
        &format!(
            "delivery = \"codex_home\"\ncodex_home = {:?}",
            empty.to_string_lossy()
        ),
    );

    assert_eq!(
        HubModelConfiguration::parse(&credential_home).err(),
        Some(HubModelConfigurationError::InvalidCredentialHome {
            credential_profile: Arc::from(CODEX_SUBSCRIPTION_PROFILE),
            failure: crate::CredentialHomeAdmissionFailure::EmptyDirectory,
        })
    );
}

#[test]
fn configuration_admits_a_credential_home_concurrency_bound() {
    let temporary = tempfile::tempdir().expect("synthetic home root is created");
    let home = temporary.path().join("account-a");
    std::fs::create_dir(&home).expect("synthetic home is created");
    std::fs::write(home.join("fixture-marker"), "synthetic").expect("synthetic home is nonempty");
    let credential_home = CONFIGURATION.replace(
            "delivery = \"ambient\"",
            &format!(
                "delivery = \"codex_home\"\ncodex_home = {:?}\nmax_concurrent_invocations = {MAX_CREDENTIAL_HOME_CONCURRENT_INVOCATIONS}",
                home.to_string_lossy()
            ),
        );

    let configuration =
        HubModelConfiguration::parse(&credential_home).expect("bounded home is admitted");
    assert!(
        configuration
            .credential_invocation_registrations()
            .iter()
            .any(|(profile, bound)| profile == CODEX_SUBSCRIPTION_PROFILE
                && bound.map(std::num::NonZeroU32::get)
                    == Some(MAX_CREDENTIAL_HOME_CONCURRENT_INVOCATIONS))
    );
}

#[test]
fn configuration_rejects_an_oversized_credential_home_before_refusing_it() {
    let oversized_path = format!("/{}", "a".repeat(MAX_CREDENTIAL_DELIVERY_PATH_UTF8_BYTES));
    let credential_home = CONFIGURATION.replace(
        "delivery = \"ambient\"",
        &format!("delivery = \"codex_home\"\ncodex_home = \"{oversized_path}\""),
    );

    assert_eq!(
        HubModelConfiguration::parse(&credential_home).err(),
        Some(HubModelConfigurationError::InvalidCredentialHome {
            credential_profile: Arc::from(CODEX_SUBSCRIPTION_PROFILE),
            failure: crate::CredentialHomeAdmissionFailure::InvalidPath,
        })
    );
}

#[test]
fn configuration_rejects_a_nul_containing_credential_file_path() {
    let credential_file = CONFIGURATION.replace(
        "file = \"/run/secrets/anthropic-primary\"",
        "file = \"/run/secrets/contains\\u0000nul\"",
    );

    assert_eq!(
        HubModelConfiguration::parse(&credential_file).err(),
        Some(HubModelConfigurationError::InvalidCredentialDelivery)
    );
}

#[test]
fn configuration_validates_an_undelivered_codex_file_before_refusing_it() {
    // `file` admits only `api_metered`, so the profile takes that kind to
    // reach the env-key validation this test is about.
    let credential_file = CONFIGURATION
        .replace(
            "billing_kind = \"subscription\"",
            "billing_kind = \"api_metered\"",
        )
        .replace(
            "delivery = \"ambient\"",
            "delivery = \"file\"\nfile = \"/run/secrets/codex-primary\"\nenv_key = \"HOME\"",
        );

    assert_eq!(
        HubModelConfiguration::parse(&credential_file).err(),
        Some(HubModelConfigurationError::InvalidCredentialDelivery)
    );
}

#[test]
fn configuration_parses_a_valid_codex_file_before_refusing_it() {
    // `file` admits only `api_metered`, so the profile takes that kind and
    // the refusal this test asserts is the undelivered one.
    let credential_file = CONFIGURATION
            .replace(
                "billing_kind = \"subscription\"",
                "billing_kind = \"api_metered\"",
            )
            .replace(
                "delivery = \"ambient\"",
                "delivery = \"file\"\nfile = \"/run/secrets/codex-primary\"\nenv_key = \"OPENAI_API_KEY\"",
            );

    assert_eq!(
        HubModelConfiguration::parse(&credential_file).err(),
        Some(HubModelConfigurationError::UndeliveredCredentialDelivery {
            delivery: Arc::from("file"),
        })
    );
}

#[test]
fn configuration_validates_undelivered_oauth_before_refusing_it() {
    let oauth = CONFIGURATION.replace(
        "delivery = \"ambient\"",
        r#"delivery = "oauth"
client_id = "synthetic-client"
token_url = "http://example.test/token"
refresh_token_url = "https://example.test/oauth/token"
device_authorization_url = "https://example.test/device"
scopes = ["model:invoke"]"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&oauth).err(),
        Some(HubModelConfigurationError::InvalidCredentialDelivery)
    );
}

/// Asserts one OAuth scope element is refused by the scope-token byte set.
///
/// Single-quoted TOML literals pass the byte through verbatim, which is
/// what lets a space, quote, or backslash reach the check at all.
#[track_caller]
fn assert_oauth_scope_rejected(scope: &str) {
    let oauth = CONFIGURATION.replace(
        "delivery = \"ambient\"",
        &format!(
            "delivery = \"oauth\"\n\
                 client_id = \"synthetic-client\"\n\
                 token_url = \"https://example.test/token\"\n\
                 refresh_token_url = \"https://example.test/oauth/token\"\n\
                 device_authorization_url = \"https://example.test/device\"\n\
                 scopes = ['{scope}']"
        ),
    );

    assert_eq!(
        HubModelConfiguration::parse(&oauth).err(),
        Some(HubModelConfigurationError::InvalidCredentialDelivery)
    );
}

#[test]
fn configuration_rejects_an_oauth_scope_holding_a_space() {
    // A space would become two scopes on the wire.
    assert_oauth_scope_rejected("read write");
}

#[test]
fn configuration_rejects_an_oauth_scope_holding_a_quote() {
    assert_oauth_scope_rejected("read\"quoted");
}

#[test]
fn configuration_rejects_an_oauth_scope_holding_a_backslash() {
    assert_oauth_scope_rejected("read\\slash");
}

#[test]
fn configuration_rejects_a_non_ascii_oauth_scope() {
    // Control bytes are outside the set too, but TOML rejects them first.
    assert_oauth_scope_rejected("r\u{e9}ad");
}

#[test]
fn configuration_rejects_an_oauth_endpoint_holding_a_fragment() {
    let oauth = CONFIGURATION.replace(
        "delivery = \"ambient\"",
        r#"delivery = "oauth"
client_id = "synthetic-client"
token_url = "https://example.test/token#stale"
refresh_token_url = "https://example.test/oauth/token"
device_authorization_url = "https://example.test/device"
scopes = ["model:invoke"]"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&oauth).err(),
        Some(HubModelConfigurationError::InvalidCredentialDelivery)
    );
}

/// Asserts one OAuth `token_url` is refused by the endpoint grammar itself,
/// before the delivery's undelivered result.
///
/// User information never reaches the request target, so it cannot
/// distinguish two provisioning tuples, and it would put a secret in the
/// static catalog. The delivery is undelivered either way; these tests
/// assert the grammar refuses the endpoint first, on its own terms.
#[track_caller]
fn assert_oauth_token_url_rejected(token_url: &str) {
    let oauth = CONFIGURATION.replace(
        "delivery = \"ambient\"",
        &format!(
            r#"delivery = "oauth"
client_id = "synthetic-client"
token_url = "{token_url}"
refresh_token_url = "https://example.test/oauth/token"
device_authorization_url = "https://example.test/device"
scopes = ["model:invoke"]"#
        ),
    );

    assert_eq!(
        HubModelConfiguration::parse(&oauth).err(),
        Some(HubModelConfigurationError::InvalidCredentialDelivery)
    );
}

#[test]
fn configuration_rejects_a_subscription_billing_kind_on_a_file_delivery() {
    // `file` presents a provider API key, so its billing kind is fixed.
    // The refusal names the profile and both disagreeing spellings, because
    // naming only the profile leaves the operator to find which field to
    // edit.
    let disagreeing = CONFIGURATION.replace(
        r#"name = "anthropic-primary"
adapter = "anthropic"
billing_kind = "api_metered""#,
        r#"name = "anthropic-primary"
adapter = "anthropic"
billing_kind = "subscription""#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&disagreeing).err(),
        Some(
            HubModelConfigurationError::DisagreeingCredentialBillingKind {
                credential_profile: Arc::from("anthropic-primary"),
                delivery: Arc::from("file"),
                billing_kind: Arc::from("subscription"),
            }
        )
    );
}

#[test]
fn configuration_rejects_an_api_metered_billing_kind_on_an_oauth_delivery() {
    // `oauth` constructs a subscription login. The disagreement is refused
    // on its own terms even though the delivery is undelivered, so the
    // contradiction is not masked by the refusal that would follow it.
    let disagreeing = CONFIGURATION
        .replace(
            r#"name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription""#,
            r#"name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "api_metered""#,
        )
        .replace(
            "delivery = \"ambient\"",
            r#"delivery = "oauth"
client_id = "synthetic-client"
token_url = "https://example.test/token"
refresh_token_url = "https://example.test/oauth/token"
device_authorization_url = "https://example.test/device"
scopes = ["model:invoke"]"#,
        );

    assert_eq!(
        HubModelConfiguration::parse(&disagreeing).err(),
        Some(
            HubModelConfigurationError::DisagreeingCredentialBillingKind {
                credential_profile: Arc::from("codex-subscription-primary"),
                delivery: Arc::from("oauth"),
                billing_kind: Arc::from("api_metered"),
            }
        )
    );
}

#[test]
fn configuration_admits_a_subscription_oauth_profile() {
    let oauth = CONFIGURATION.replace(
        "delivery = \"ambient\"",
        r#"delivery = "oauth"
client_id = "synthetic-client"
token_url = "https://example.test/token"
refresh_token_url = "https://example.test/oauth/token"
device_authorization_url = "https://example.test/device"
scopes = ["model:invoke"]"#,
    );

    let configuration = HubModelConfiguration::parse(&oauth).expect("valid OAuth profile");
    assert_eq!(configuration.oauth_registrations().len(), 1);
}

#[test]
fn configuration_admits_an_api_metered_ambient_profile() {
    // `ambient` names a login the operator established outside the daemon,
    // which may be billed either way, so both kinds are admitted.
    let ambient = CONFIGURATION.replace(
        r#"name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription""#,
        r#"name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "api_metered""#,
    );

    assert!(HubModelConfiguration::parse(&ambient).is_ok());
}

#[test]
fn configuration_admits_a_subscription_ambient_profile() {
    // The checked-in fixture already pairs `ambient` with `subscription`;
    // asserting it here states the other half of that delivery's rule.
    assert!(HubModelConfiguration::parse(CONFIGURATION).is_ok());
}

#[test]
fn configuration_rejects_an_oauth_endpoint_holding_a_username() {
    assert_oauth_token_url_rejected("https://alice@example.test/token");
}

#[test]
fn configuration_rejects_an_oauth_endpoint_holding_a_username_and_password() {
    assert_oauth_token_url_rejected("https://alice:secret@example.test/token");
}

#[test]
fn configuration_rejects_an_oauth_endpoint_holding_a_password_alone() {
    assert_oauth_token_url_rejected("https://:secret@example.test/token");
}

#[test]
fn configuration_rejects_a_device_endpoint_holding_user_information() {
    let oauth = CONFIGURATION.replace(
        "delivery = \"ambient\"",
        r#"delivery = "oauth"
client_id = "synthetic-client"
token_url = "https://example.test/token"
refresh_token_url = "https://example.test/oauth/token"
device_authorization_url = "https://alice:secret@example.test/device"
scopes = ["model:invoke"]"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&oauth).err(),
        Some(HubModelConfigurationError::InvalidCredentialDelivery)
    );
}

#[test]
fn configuration_parses_valid_oauth() {
    let oauth = CONFIGURATION.replace(
        "delivery = \"ambient\"",
        r#"delivery = "oauth"
client_id = "synthetic-client"
token_url = "https://example.test/token"
refresh_token_url = "https://example.test/oauth/token"
device_authorization_url = "https://example.test/device"
scopes = ["model:invoke"]"#,
    );

    let configuration = HubModelConfiguration::parse(&oauth).expect("valid OAuth profile");
    assert_eq!(configuration.oauth_registrations().len(), 1);
}

#[test]
fn configuration_requires_a_valid_https_refresh_endpoint() {
    let oauth = CONFIGURATION.replace(
        "delivery = \"ambient\"",
        r#"delivery = "oauth"
client_id = "synthetic-client"
token_url = "https://example.test/token"
refresh_token_url = "https://EXAMPLE.test:443/oauth/token?audience=codex"
device_authorization_url = "https://example.test/device"
scopes = ["model:invoke"]"#,
    );
    let configuration = HubModelConfiguration::parse(&oauth).expect("valid OAuth profile");
    assert_eq!(
        configuration.oauth_registrations()[0].1.refresh_token_url,
        "https://example.test/oauth/token?audience=codex"
    );
    for endpoint in [
        "http://example.test/oauth/token",
        "https://example.test/oauth/token#fragment",
        "https://secret@example.test/oauth/token",
        "/oauth/token",
    ] {
        let invalid = oauth.replace(
            "https://EXAMPLE.test:443/oauth/token?audience=codex",
            endpoint,
        );
        assert_eq!(
            HubModelConfiguration::parse(&invalid).err(),
            Some(HubModelConfigurationError::InvalidCredentialDelivery)
        );
    }
    let missing = oauth.replace(
        "refresh_token_url = \"https://EXAMPLE.test:443/oauth/token?audience=codex\"\n",
        "",
    );
    assert_eq!(
        HubModelConfiguration::parse(&missing).err(),
        Some(HubModelConfigurationError::InvalidField)
    );
}

#[test]
fn configuration_rejects_an_unknown_delivery() {
    let unknown_delivery =
        CONFIGURATION.replace("delivery = \"ambient\"", "delivery = \"telepathy\"");

    assert_eq!(
        HubModelConfiguration::parse(&unknown_delivery).err(),
        Some(HubModelConfigurationError::InvalidCredentialDelivery)
    );
}

#[test]
fn configuration_rejects_a_relative_credential_file() {
    let relative_file = CONFIGURATION.replace(
        "file = \"/run/secrets/anthropic-primary\"",
        "file = \"secrets/anthropic-primary\"",
    );

    assert_eq!(
        HubModelConfiguration::parse(&relative_file).err(),
        Some(HubModelConfigurationError::InvalidCredentialDelivery)
    );
}

#[test]
fn configuration_rejects_a_direct_adapter_file_environment_key() {
    let environment_key = CONFIGURATION.replace(
        "file = \"/run/secrets/anthropic-primary\"",
        "file = \"/run/secrets/anthropic-primary\"\nenv_key = \"ANTHROPIC_API_KEY\"",
    );

    assert_eq!(
        HubModelConfiguration::parse(&environment_key).err(),
        Some(HubModelConfigurationError::InvalidCredentialDelivery)
    );
}

#[test]
fn configuration_admits_claude_file_delivery_with_its_fixed_environment_key() {
    const CLAUDE_FILE: &str = "/run/secrets/claude-api-primary";
    const CLAUDE_ENV_KEY: &str = "ANTHROPIC_API_KEY";
    let claude_file = CONFIGURATION.replace(
        r#"adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient""#,
        &format!(
            r#"adapter = "claude_cli"
billing_kind = "api_metered"
delivery = "file"
file = "{CLAUDE_FILE}"
env_key = "{CLAUDE_ENV_KEY}""#,
        ),
    );

    let configured = HubModelConfiguration::parse(&claude_file)
        .expect("Claude file delivery is part of the supplied grammar");
    let profile = configured
        .credential_profile(CODEX_SUBSCRIPTION_PROFILE)
        .expect("the replaced fixture profile remains declared");
    assert_eq!(
        profile.delivery(),
        &CredentialDelivery::File {
            path: PathBuf::from(CLAUDE_FILE),
            env_key: Some(Arc::from(CLAUDE_ENV_KEY)),
        }
    );
}

#[test]
fn configuration_rejects_another_claude_file_environment_key() {
    let claude_file = CONFIGURATION.replace(
        r#"adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient""#,
        r#"adapter = "claude_cli"
billing_kind = "api_metered"
delivery = "file"
file = "/run/secrets/claude-api-primary"
env_key = "HOME""#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&claude_file).err(),
        Some(HubModelConfigurationError::InvalidCredentialDelivery)
    );
}

#[test]
fn configuration_rejects_duplicate_normalized_file_paths_for_one_adapter() {
    let duplicate_path = CONFIGURATION.replace(
        "file = \"/run/secrets/anthropic-overflow\"",
        "file = \"/run/secrets/./nested/../anthropic-primary\"",
    );

    assert_eq!(
        HubModelConfiguration::parse(&duplicate_path).err(),
        Some(HubModelConfigurationError::InvalidCredentialDelivery)
    );
}

#[test]
fn configuration_rejects_duplicate_ambient_profiles_for_one_cli_adapter() {
    let duplicate_ambient = CONFIGURATION.replace(
        r#"[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient""#,
        r#"[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient"

[[credential_profiles]]
name = "codex-subscription-overflow"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient""#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&duplicate_ambient).err(),
        Some(HubModelConfigurationError::InvalidCredentialDelivery)
    );
}

#[test]
fn configuration_rejects_mixed_ambient_and_home_delivery_for_codex() {
    let temporary = tempfile::tempdir().expect("synthetic home root is created");
    let home = temporary.path().join("account-b");
    std::fs::create_dir(&home).expect("synthetic home is created");
    std::fs::write(home.join("fixture-marker"), "synthetic").expect("synthetic home is nonempty");
    let mixed_delivery = CONFIGURATION.replace(
        r#"[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient""#,
        &format!(
            r#"[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient"

[[credential_profiles]]
name = "codex-subscription-overflow"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "codex_home"
codex_home = {:?}"#,
            home.to_string_lossy()
        ),
    );

    assert_eq!(
        HubModelConfiguration::parse(&mixed_delivery).err(),
        Some(HubModelConfigurationError::InvalidCredentialDelivery)
    );
}

#[test]
fn configuration_rejects_mixed_home_and_ambient_delivery_for_codex_in_reverse_order() {
    let temporary = tempfile::tempdir().expect("synthetic home root is created");
    let home = temporary.path().join("account-b");
    std::fs::create_dir(&home).expect("synthetic home is created");
    std::fs::write(home.join("fixture-marker"), "synthetic").expect("synthetic home is nonempty");
    let mixed_delivery = CONFIGURATION.replace(
        r#"[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient""#,
        &format!(
            r#"[[credential_profiles]]
name = "codex-subscription-overflow"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "codex_home"
codex_home = {:?}

[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient""#,
            home.to_string_lossy()
        ),
    );

    assert_eq!(
        HubModelConfiguration::parse(&mixed_delivery).err(),
        Some(HubModelConfigurationError::InvalidCredentialDelivery)
    );
}

#[test]
fn configuration_admits_a_claude_ambient_profile_declared_before_a_codex_home() {
    let temporary = tempfile::tempdir().expect("synthetic home root is created");
    let home = temporary.path().join("account-b");
    std::fs::create_dir(&home).expect("synthetic home is created");
    std::fs::write(home.join("fixture-marker"), "synthetic").expect("synthetic home is nonempty");
    // The Claude `ambient` profile precedes the Codex home in table order,
    // which is the arrangement an adapter-blind conflict scan rejects.
    let cross_adapter = CONFIGURATION.replace(
        r#"[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient""#,
        &format!(
            r#"[[credential_profiles]]
name = "claude-subscription-primary"
adapter = "claude_cli"
billing_kind = "subscription"
delivery = "ambient"

[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "codex_home"
codex_home = {:?}"#,
            home.to_string_lossy()
        ),
    );

    let parsed = HubModelConfiguration::parse(&cross_adapter)
        .expect("a Claude ambient profile does not contest a Codex credential home");
    assert_eq!(
        parsed
            .credential_profile(CODEX_SUBSCRIPTION_PROFILE)
            .expect("Codex profile remains present")
            .delivery()
            .path(),
        Some(&home)
    );
}

#[test]
fn file_delivery_records_the_absolute_path_it_reads() {
    let configured = HubModelConfiguration::parse(CONFIGURATION).expect("the fixture is valid");

    let profile = configured
        .credential_profile("anthropic-primary")
        .expect("the fixture declares the profile");

    assert_eq!(
        profile.delivery(),
        &CredentialDelivery::File {
            path: PathBuf::from("/run/secrets/anthropic-primary"),
            env_key: None,
        }
    );
}

#[test]
fn configuration_debug_redacts_credential_file_paths() {
    let configured = HubModelConfiguration::parse(CONFIGURATION).expect("the fixture is valid");
    let credential_path = configured
        .credential_profile("anthropic-primary")
        .expect("the fixture declares the profile")
        .delivery()
        .path()
        .expect("the fixture profile uses file delivery")
        .to_string_lossy();

    assert!(!format!("{configured:?}").contains(credential_path.as_ref()));
}

#[test]
fn file_profile_catalog_is_complete_within_and_closed_across_adapters() {
    let configured =
        HubModelConfiguration::parse(&format!("{CONFIGURATION}\n{OPENAI_MAPPING_AND_MODEL}"))
            .expect("the combined fixture is valid");
    let anthropic_profiles = configured
        .file_credential_profiles(ModelAdapter::Anthropic)
        .map(|(reference, _)| reference)
        .collect::<HashSet<_>>();
    let openai_profiles = configured
        .file_credential_profiles(ModelAdapter::OpenAi)
        .map(|(reference, _)| reference)
        .collect::<HashSet<_>>();

    assert_eq!(
        anthropic_profiles,
        HashSet::from([ANTHROPIC_CREDENTIAL_REFERENCE, ANTHROPIC_OVERFLOW_PROFILE,])
    );
    assert_eq!(openai_profiles, HashSet::from([OPENAI_PROFILE]));
}

#[test]
fn codex_only_configuration_delivers_no_anthropic_file() {
    let executable = tempfile::NamedTempFile::new().expect("a temporary executable is created");
    let working_directory = tempfile::tempdir().expect("a temporary directory is created");
    let configured = HubModelConfiguration::parse_test_fixture(&format!(
        r#"
version = 1

[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient"

[[credential_pools]]
name = "codex-main"
tie_break = "first_listed"
on_pool_exhausted = "fail"
members = [{{ profile = "codex-subscription-primary", priority = 1 }}]

[[adapter_mappings]]
model_family = "codex"
adapter = "codex_cli"
credential_pool = "codex-main"

[codex_cli]
executable = "{}"
working_directory = "{}"

[compaction]
prompt = "Summarize."

[[models]]
selection_id = "10000000-0000-4000-8000-000000000009"
target_id = "20000000-0000-4000-8000-000000000009"
model_family = "codex"
provider_model = "gpt-example"
max_output_tokens = 256
context_window_tokens = 200000
"#,
        executable.path().display(),
        working_directory.path().display(),
    ))
    .expect("a Codex-only configuration is valid");

    assert_eq!(
        configured
            .file_credential_profiles(ModelAdapter::Anthropic)
            .next(),
        None
    );
}

#[test]
fn configuration_rejects_models_with_no_family_mapping() {
    let unmapped_family = "codex";
    let unmapped = CONFIGURATION.replace(
        "model_family = \"anthropic\"\nprovider_model",
        &format!("model_family = \"{unmapped_family}\"\nprovider_model"),
    );

    assert_eq!(
        HubModelConfiguration::parse(&unmapped).err(),
        Some(HubModelConfigurationError::UnmappedModelFamily {
            model_family: Arc::from(unmapped_family),
        })
    );
}

#[test]
fn configuration_rejects_a_missing_codex_executable() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let missing_executable = temporary.path().join("missing-codex");
    let configuration = configuration_with_codex_paths(&missing_executable, temporary.path());

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidCodexCliConfiguration)
    );
}

#[test]
fn configuration_rejects_a_codex_executable_that_is_not_a_file() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let configuration = configuration_with_codex_paths(temporary.path(), temporary.path());

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidCodexCliConfiguration)
    );
}

#[test]
fn codex_model_context_window_overrides_are_positive_exact_target_values() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let executable = std::env::current_exe().expect("the test executable has a path");
    let configuration = format!(
        "{}model_context_window_overrides = {{ \"gpt-5.6-sol\" = 1000000 }}\n\n\
             [[models]]\n\
             selection_id = \"10000000-0000-4000-8000-00000000000f\"\n\
             target_id = \"20000000-0000-4000-8000-00000000000f\"\n\
             model_family = \"codex\"\n\
             provider_model = \"gpt-5.6-sol\"\n\
             max_output_tokens = 8192\n\
             context_window_tokens = 828400\n",
        configuration_with_codex_paths(&executable, temporary.path())
    );

    let parsed = HubModelConfiguration::parse(&configuration)
        .expect("a positive exact-target override is valid");

    assert_eq!(
        parsed
            .codex_cli()
            .and_then(|codex| codex.model_context_window_overrides.get("gpt-5.6-sol")),
        Some(&1_000_000)
    );
}

#[test]
fn configuration_rejects_a_zero_codex_model_context_window_override() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let executable = std::env::current_exe().expect("the test executable has a path");
    let configuration = format!(
        "{}model_context_window_overrides = {{ \"gpt-5.6-sol\" = 0 }}\n",
        configuration_with_codex_paths(&executable, temporary.path())
    );

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidCodexCliConfiguration)
    );
}

#[test]
fn configuration_rejects_an_unknown_codex_model_context_window_override() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let executable = std::env::current_exe().expect("the test executable has a path");
    let configuration = format!(
        "{}model_context_window_overrides = {{ \"gpt-5.6-sol\" = 1000000 }}\n",
        configuration_with_codex_paths(&executable, temporary.path())
    );

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidCodexCliConfiguration)
    );
}

#[test]
fn configuration_rejects_a_codex_override_for_another_adapter() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let executable = std::env::current_exe().expect("the test executable has a path");
    let configuration = format!(
        "{}model_context_window_overrides = {{ \"claude-example\" = 1000000 }}\n",
        configuration_with_codex_paths(&executable, temporary.path())
    );

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidCodexCliConfiguration)
    );
}

#[test]
fn unused_codex_mapping_retains_its_declared_credential_profile() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let executable = std::env::current_exe().expect("the test executable has a path");
    let configuration = HubModelConfiguration::parse(&configuration_with_codex_paths(
        &executable,
        temporary.path(),
    ))
    .expect("the unused Codex mapping is valid configuration");

    assert_eq!(
        configuration.codex_cli_credential_profile.as_deref(),
        Some(CODEX_SUBSCRIPTION_PROFILE)
    );
    assert!(
        configuration
            .codex_cli_runtime(None, None)
            .expect("the stored profile constructs the runtime")
            .is_some()
    );
}

#[test]
fn configuration_rejects_a_missing_claude_executable() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let bridge = std::env::current_exe().expect("the test executable has a path");
    let missing_executable = temporary.path().join("missing-claude");
    let configuration =
        configuration_with_claude_paths(&missing_executable, &bridge, temporary.path());

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidClaudeCliConfiguration)
    );
}

#[test]
fn configuration_rejects_a_claude_executable_that_is_not_a_file() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let bridge = std::env::current_exe().expect("the test executable has a path");
    let configuration =
        configuration_with_claude_paths(temporary.path(), &bridge, temporary.path());

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidClaudeCliConfiguration)
    );
}

/// The MCP bridge is a second deployment-named program, so its path is
/// validated exactly as strictly as the CLI's rather than being derived.
#[test]
fn configuration_rejects_a_missing_claude_mcp_bridge_executable() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let executable = std::env::current_exe().expect("the test executable has a path");
    let missing_bridge = temporary.path().join("missing-bridge");
    let configuration =
        configuration_with_claude_paths(&executable, &missing_bridge, temporary.path());

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidClaudeCliConfiguration)
    );
}

/// A bare name is the second admitted spelling, so a name the daemon's own
/// search path does not hold fails startup as its own diagnosis rather
/// than as a malformed path.
#[test]
fn configuration_rejects_a_claude_mcp_bridge_name_no_search_entry_holds() {
    let (configuration, _workspace) =
        configuration_varying_the_claude_bridge(Path::new(ABSENT_MCP_BRIDGE_NAME));

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::UnresolvedClaudeMcpBridgeExecutable)
    );
}

/// A relative value carries a path separator, so it is a path and keeps
/// the absolute-path rule instead of being looked up as a program name.
#[test]
fn configuration_rejects_a_relative_claude_mcp_bridge_path() {
    let (configuration, _workspace) =
        configuration_varying_the_claude_bridge(Path::new("bin/signalbox-claude-mcp-bridge"));

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidClaudeCliConfiguration)
    );
}

#[test]
fn mcp_bridge_name_resolves_through_the_first_search_entry_holding_it() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let earlier = synthetic_search_directory(temporary.path(), "earlier");
    let later = synthetic_search_directory(temporary.path(), "later");
    let expected = synthetic_executable(&earlier, CLAUDE_MCP_BRIDGE_NAME);
    let shadowed = synthetic_executable(&later, CLAUDE_MCP_BRIDGE_NAME);

    let resolved = resolved_mcp_bridge_reference(
        CLAUDE_MCP_BRIDGE_NAME,
        Some(&synthetic_search_path(&[&earlier, &later])),
    )
    .expect("the fixture search path holds the bridge");

    assert_eq!(resolved, expected);
    assert_ne!(resolved, shadowed);
}

/// Resolution matches what executing the name would do: a same-named file
/// this process cannot execute shadows nothing.
#[cfg(unix)]
#[test]
fn mcp_bridge_name_skips_a_search_entry_whose_file_is_not_executable() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let earlier = synthetic_search_directory(temporary.path(), "earlier");
    let later = synthetic_search_directory(temporary.path(), "later");
    let unexecutable = synthetic_unexecutable_file(&earlier, CLAUDE_MCP_BRIDGE_NAME);
    let expected = synthetic_executable(&later, CLAUDE_MCP_BRIDGE_NAME);

    let resolved = resolved_mcp_bridge_reference(
        CLAUDE_MCP_BRIDGE_NAME,
        Some(&synthetic_search_path(&[&earlier, &later])),
    )
    .expect("the later search entry holds an executable bridge");

    assert_eq!(resolved, expected);
    assert_ne!(resolved, unexecutable);
}

#[test]
fn mcp_bridge_name_without_a_search_path_resolves_to_nothing() {
    assert_eq!(
        resolved_mcp_bridge_reference(CLAUDE_MCP_BRIDGE_NAME, None),
        Err(HubModelConfigurationError::UnresolvedClaudeMcpBridgeExecutable)
    );
}

/// A configured path is the operator's exact choice, so a same-named
/// program on the search path never displaces it.
#[test]
fn mcp_bridge_path_is_used_verbatim_over_a_search_entry_holding_the_name() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let entry = synthetic_search_directory(temporary.path(), "entry");
    let shadowing = synthetic_executable(&entry, CLAUDE_MCP_BRIDGE_NAME);
    let configured = temporary
        .path()
        .join("install")
        .join(CLAUDE_MCP_BRIDGE_NAME);

    let resolved = resolved_mcp_bridge_reference(
        configured
            .to_str()
            .expect("the fixture install path is UTF-8"),
        Some(&synthetic_search_path(&[&entry])),
    )
    .expect("a configured path needs no search entry");

    assert_eq!(resolved, configured);
    assert_ne!(resolved, shadowing);
}

/// The resolved path is written into a configuration another process
/// reads from a working directory of its own, so an entry that only means
/// something relative to this process is not a place to look.
#[cfg(unix)]
#[test]
fn search_entries_drop_the_relative_and_empty_ones() {
    let empty_entry = Path::new("");
    let relative_entry = Path::new("synthetic/relative/bin");
    let absolute_entry = Path::new("/synthetic/absolute/bin");
    let search_path = synthetic_search_path(&[empty_entry, relative_entry, absolute_entry]);

    assert_eq!(
        absolute_search_entries(Some(&search_path)),
        vec![absolute_entry.to_path_buf()]
    );
}

#[test]
fn configuration_rejects_a_claude_mapping_without_process_settings() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let executable = std::env::current_exe().expect("the test executable has a path");
    let complete = configuration_with_claude_paths(&executable, &executable, temporary.path());
    let start = complete
        .find("[claude_cli]")
        .expect("the fixture declares Claude process settings");
    let without_process_settings = &complete[..start];

    assert_eq!(
        HubModelConfiguration::parse(without_process_settings).err(),
        Some(HubModelConfigurationError::MissingClaudeCliConfiguration)
    );
}

#[test]
fn unused_claude_mapping_retains_its_declared_credential_profile() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let executable = std::env::current_exe().expect("the test executable has a path");
    let configuration = HubModelConfiguration::parse(&configuration_with_claude_paths(
        &executable,
        &executable,
        temporary.path(),
    ))
    .expect("the unused Claude mapping is valid configuration");

    assert_eq!(
        configuration.claude_cli_credential_profile.as_deref(),
        Some(CLAUDE_SUBSCRIPTION_PROFILE)
    );
    assert_eq!(
        configuration
            .claude_cli()
            .expect("the fixture declares Claude process settings")
            .mcp_bridge_executable(),
        executable.as_path()
    );
    assert!(
        configuration
            .claude_cli_runtime(None, None, None)
            .expect("the stored profile constructs the runtime")
            .is_some()
    );
}

/// Claude Code exposes no service tier, so a configured tier fails startup
/// instead of reaching preparation as an unenforceable request control.
#[test]
fn configuration_rejects_a_service_tier_on_a_claude_model() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let executable = std::env::current_exe().expect("the test executable has a path");
    let configuration = format!(
        "{}{}",
        configuration_with_claude_paths(&executable, &executable, temporary.path()),
        CLAUDE_MODEL_ENTRY.replace(
            "reasoning_levels = [\"high\"]",
            "reasoning_levels = [\"high\"]\nservice_tiers = [\"auto\"]",
        ),
    );

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidModelCapabilities)
    );
}

/// Claude Code reports input tokens exclusive of the cache axes it reports
/// separately, exactly as the Anthropic API does.
#[test]
fn configured_claude_models_route_to_the_claude_adapter_with_cache_exclusive_input() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let executable = std::env::current_exe().expect("the test executable has a path");
    let configuration = HubModelConfiguration::parse(&format!(
        "{}{CLAUDE_MODEL_ENTRY}",
        configuration_with_claude_paths(&executable, &executable, temporary.path()),
    ))
    .expect("the Claude mapping, process settings, and model are valid");
    let selection = DirectModelSelection::from_uuid(
        Uuid::parse_str("10000000-0000-4000-8000-00000000000c").expect("fixture UUID is valid"),
    );

    let route = configuration
        .resolve_direct_model(selection)
        .expect("the Claude selection has an adapter route");

    assert_eq!(route.adapter(), ModelAdapter::ClaudeCli);
    assert_eq!(route.credential_profile(), CLAUDE_SUBSCRIPTION_PROFILE);
    assert_eq!(
        configuration.adapter_for_provider_model("claude-cli-example"),
        Some(ModelAdapter::ClaudeCli)
    );
    assert!(
        !configuration
            .cache_inclusive_input_targets()
            .contains(&route.target())
    );
}

/// OpenAI is an API-key adapter, so it mirrors Anthropic: the mapping is
/// pinned to the one profile the daemon binds its credential file to, and
/// `prompt_tokens` already contains the cache axes reported beside it.
#[test]
fn configured_openai_models_route_through_the_pinned_api_key_profile() {
    let configuration =
        HubModelConfiguration::parse(&format!("{CONFIGURATION}{OPENAI_MAPPING_AND_MODEL}"))
            .expect("the OpenAI mapping, profile, and model are valid");
    let selection = DirectModelSelection::from_uuid(
        Uuid::parse_str("10000000-0000-4000-8000-00000000000e").expect("fixture UUID is valid"),
    );

    let route = configuration
        .resolve_direct_model(selection)
        .expect("the OpenAI selection has an adapter route");

    assert_eq!(route.adapter(), ModelAdapter::OpenAi);
    assert_eq!(route.credential_profile(), OPENAI_PROFILE);
    assert!(configuration.uses_openai_adapter());
    assert_eq!(
        configuration.adapter_for_provider_model("gpt-example"),
        Some(ModelAdapter::OpenAi)
    );
    assert!(
        configuration
            .cache_inclusive_input_targets()
            .contains(&route.target())
    );
}

#[test]
fn configuration_accepts_an_opaque_openai_profile_name() {
    let other_profile = "openai-secondary";
    let configuration = format!("{CONFIGURATION}{OPENAI_MAPPING_AND_MODEL}")
        .replace(
            "name = \"openai-primary\"",
            &format!("name = \"{other_profile}\""),
        )
        .replace(
            "profile = \"openai-primary\"",
            &format!("profile = \"{other_profile}\""),
        );
    let selection = DirectModelSelection::from_uuid(
        Uuid::parse_str("10000000-0000-4000-8000-00000000000e").expect("fixture UUID is valid"),
    );

    assert_eq!(
        HubModelConfiguration::parse(&configuration)
            .expect("opaque profile name is valid")
            .resolve_direct_model(selection)
            .expect("the OpenAI route exists")
            .credential_profile(),
        other_profile
    );
}

/// `ultra` is the Codex effort value, so it is unsupported here even though
/// every lower level maps onto the OpenAI wire control.
#[test]
fn configuration_rejects_an_openai_reasoning_level_the_adapter_cannot_enforce() {
    let configuration = format!("{CONFIGURATION}{OPENAI_MAPPING_AND_MODEL}").replace(
        "reasoning_levels = [\"minimal\", \"medium\", \"xhigh\"]",
        "reasoning_levels = [\"ultra\"]",
    );

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidModelCapabilities)
    );
}

/// Service-tier spellings are provider-tagged, so an Anthropic-only value
/// cannot be read as OpenAI's despite the shared word.
#[test]
fn configuration_rejects_another_providers_service_tier_on_an_openai_model() {
    let configuration = format!("{CONFIGURATION}{OPENAI_MAPPING_AND_MODEL}").replace(
        "service_tiers = [\"flex\", \"priority\"]",
        "service_tiers = [\"standard_only\"]",
    );

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidModelCapabilities)
    );
}

/// Fast mode maps an absent tier onto `fast`, so a simultaneous explicit
/// non-fast tier is an adapter-level conflict caught before startup ends.
#[test]
fn configuration_rejects_openai_fast_mode_beside_a_conflicting_configured_tier() {
    let configuration = format!(
        "{CONFIGURATION}{OPENAI_MAPPING_AND_MODEL}\n[model_settings]\nfast_mode = \"enabled\"\nservice_tier = {{ provider = \"open_ai\", value = \"flex\" }}\n"
    );

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidModelSettingsConfiguration)
    );
}

#[test]
fn unknown_session_model_rejection_names_the_requested_model() {
    let configuration =
        HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(99));
    let request = ModelSelectionRequest::Direct(selection);

    assert_eq!(
        configuration.resolve_session_model(request),
        Err(UnknownSessionModel { selection: request })
    );
    assert!(
        UnknownSessionModel { selection: request }
            .to_string()
            .contains(&format!("{request:?}"))
    );
}

#[test]
fn configuration_loads_the_shared_convergence_policy() {
    let example = include_str!("../../../../crates/convergence/examples/repository.toml");
    let policy = example.replace("[[reviewers]]", "[[convergence.reviewers]]");
    let configured = format!("{CONFIGURATION}\n[convergence]\n{policy}");
    let configuration = HubModelConfiguration::parse(&configured)
        .expect("the shared policy example is accepted under convergence");
    let parsed = configuration.convergence().expect("the policy is retained");
    let expected: signalbox_convergence::ConvergencePolicy =
        toml::from_str(example).expect("the example is a shared convergence policy");
    assert_eq!(
        serde_json::to_value(parsed).expect("policy serializes"),
        serde_json::to_value(expected).expect("policy serializes")
    );
}

#[test]
fn configuration_rejects_empty_normalized_convergence_reviewers() {
    let example = include_str!("../../../../crates/convergence/examples/repository.toml");
    for login in ["", " ", "[bot]", "[BOT]"] {
        let mut policy: toml::Value = toml::from_str(example).expect("the example is valid TOML");
        policy["reviewers"][0]["login"] = toml::Value::String(login.into());
        let policy = toml::to_string(&policy)
            .expect("policy serializes")
            .replace("[[reviewers]]", "[[convergence.reviewers]]");
        let configured = format!("{CONFIGURATION}\n[convergence]\n{policy}");
        assert!(
            HubModelConfiguration::parse(&configured).is_err(),
            "a reviewer must have an identity after bot normalization"
        );
    }
}

#[test]
fn configuration_rejects_unknown_convergence_policy_fields() {
    let example = include_str!("../../../../crates/convergence/examples/repository.toml");
    let policy = example.replace("[[reviewers]]", "[[convergence.reviewers]]");
    for configured in [
        format!("{CONFIGURATION}\n[convergence]\nobsolete = true\n{policy}"),
        format!("{CONFIGURATION}\n[convergence]\n{policy}\nobsolete = true"),
    ] {
        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidDocument)
        );
    }
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

#[test]
fn configuration_admits_explicit_workspace_instruction_roots() {
    let configured = format!(
        "{CONFIGURATION}\n[workspace_instructions]\nversion = 1\nregistered_roots = [\"{REGISTERED_INSTRUCTION_ROOT}\"]\n"
    );
    let configuration = HubModelConfiguration::parse(&configured)
        .expect("one canonical explicit instruction root is admitted");
    assert_eq!(configuration.workspace_instructions().roots().len(), 1);
    assert_eq!(
        configuration.workspace_instructions().roots()[0].as_str(),
        REGISTERED_INSTRUCTION_ROOT
    );
}

#[test]
fn configuration_defaults_instruction_roots_to_empty() {
    let configuration = HubModelConfiguration::parse(CONFIGURATION)
        .expect("the base fixture omits explicit instruction roots");
    assert!(configuration.workspace_instructions().roots().is_empty());
}

#[test]
fn configuration_rejects_relative_instruction_roots() {
    let relative = format!(
        "{CONFIGURATION}\n[workspace_instructions]\nversion = 1\nregistered_roots = [\"relative/root\"]\n"
    );
    assert_eq!(
        HubModelConfiguration::parse(&relative).err(),
        Some(HubModelConfigurationError::InvalidWorkspaceInstructionConfiguration)
    );
}

#[test]
fn configuration_rejects_each_malformed_web_fetch_policy_shape() {
    let unknown_field = CONFIGURATION.replace(
        r#"allowed_origins = ["https://example.com"]"#,
        r#"allowed_origins = ["https://example.com"]
extra = true"#,
    );
    let non_string_origin = CONFIGURATION.replace(
        r#"allowed_origins = ["https://example.com"]"#,
        "allowed_origins = [17]",
    );
    let non_origin_url = CONFIGURATION.replace(
        r#"allowed_origins = ["https://example.com"]"#,
        r#"allowed_origins = ["https://example.com/path"]"#,
    );

    assert_eq!(
        HubModelConfiguration::parse(&unknown_field).err(),
        Some(HubModelConfigurationError::InvalidWebFetchPolicy)
    );
    assert_eq!(
        HubModelConfiguration::parse(&non_string_origin).err(),
        Some(HubModelConfigurationError::InvalidWebFetchPolicy)
    );
    assert_eq!(
        HubModelConfiguration::parse(&non_origin_url).err(),
        Some(HubModelConfigurationError::InvalidWebFetchPolicy)
    );
}

#[test]
fn configuration_requires_a_positive_viable_declared_context_window() {
    let missing = CONFIGURATION.replace("\ncontext_window_tokens = 200000", "");
    assert_eq!(
        HubModelConfiguration::parse(&missing).err(),
        Some(HubModelConfigurationError::InvalidField)
    );

    let zero = CONFIGURATION.replace(
        "context_window_tokens = 200000",
        "context_window_tokens = 0",
    );
    assert_eq!(
        HubModelConfiguration::parse(&zero).err(),
        Some(HubModelConfigurationError::InvalidLimit)
    );

    let impossible_reservation =
        CONFIGURATION.replace("max_output_tokens = 256", "max_output_tokens = 200001");
    assert_eq!(
        HubModelConfiguration::parse(&impossible_reservation).err(),
        Some(HubModelConfigurationError::InvalidField)
    );
}

#[test]
fn provider_compaction_is_an_explicit_per_target_capability() {
    let disabled = HubModelConfiguration::parse(CONFIGURATION)
        .expect("omitted provider compaction defaults closed");
    let target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
        Uuid::parse_str("20000000-0000-4000-8000-000000000001").expect("fixture target is a UUID"),
    ));
    assert!(
        !disabled
            .runtime_model_catalog()
            .resolve(target)
            .expect("fixture target is configured")
            .provider_compaction_supported()
    );

    let enabled = HubModelConfiguration::parse(&CONFIGURATION.replace(
        "provider_model = \"claude-example\"",
        "provider_model = \"claude-example\"\nprovider_compaction = true",
    ))
    .expect("the Anthropic target declares provider compaction");
    assert!(
        enabled
            .runtime_model_catalog()
            .resolve(target)
            .expect("fixture target is configured")
            .provider_compaction_supported()
    );

    let malformed = CONFIGURATION.replace(
        "provider_model = \"claude-example\"",
        "provider_model = \"claude-example\"\nprovider_compaction = \"true\"",
    );
    assert_eq!(
        HubModelConfiguration::parse(&malformed).err(),
        Some(HubModelConfigurationError::InvalidModelCapabilities)
    );

    let wrong_adapter = format!(
        "{}\n[codex_cli]\nexecutable = \"/bin/true\"\nworking_directory = \"/tmp\"\n",
        CONFIGURATION
            .replace(
                "adapter = \"anthropic\"\ncredential_pool = \"anthropic-main\"",
                "adapter = \"codex_cli\"\ncredential_pool = \"codex-main\"",
            )
            .replace(
                "provider_model = \"claude-example\"",
                "provider_model = \"claude-example\"\nprovider_compaction = true",
            )
    );
    assert_eq!(
        HubModelConfiguration::parse(&wrong_adapter).err(),
        Some(HubModelConfigurationError::InvalidModelCapabilities)
    );
}

#[test]
fn provider_compaction_capability_is_keyed_by_target_not_provider_spelling() {
    let configuration = format!(
        "{}\n[[models]]\nselection_id = \"10000000-0000-4000-8000-000000000002\"\ntarget_id = \"20000000-0000-4000-8000-000000000002\"\nmodel_family = \"anthropic\"\nprovider_model = \"claude-example\"\nmax_output_tokens = 256\ncontext_window_tokens = 200000\n",
        CONFIGURATION.replace(
            "provider_model = \"claude-example\"",
            "provider_model = \"claude-example\"\nprovider_compaction = true",
        )
    );
    let configuration = HubModelConfiguration::parse(&configuration)
        .expect("distinct targets may share a provider spelling and differ in compaction");
    let models = configuration.runtime_model_catalog();
    let enabled = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
        Uuid::parse_str("20000000-0000-4000-8000-000000000001").expect("fixture target is a UUID"),
    ));
    let disabled = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
        Uuid::parse_str("20000000-0000-4000-8000-000000000002").expect("fixture target is a UUID"),
    ));

    assert!(
        models
            .resolve(enabled)
            .expect("first target is configured")
            .provider_compaction_supported()
    );
    assert!(
        !models
            .resolve(disabled)
            .expect("second target is configured")
            .provider_compaction_supported()
    );
}

#[test]
fn configuration_requires_one_bounded_exact_compaction_prompt() {
    let missing_table = CONFIGURATION.replace(
            "[compaction]\nprompt = \"Summarize the prior conversation faithfully for continuation.\"\n\n",
            "",
        );
    assert_eq!(
        HubModelConfiguration::parse(&missing_table).err(),
        Some(HubModelConfigurationError::MissingCompaction)
    );

    let missing_prompt = CONFIGURATION.replace(
        "prompt = \"Summarize the prior conversation faithfully for continuation.\"\n",
        "",
    );
    assert_eq!(
        HubModelConfiguration::parse(&missing_prompt).err(),
        Some(HubModelConfigurationError::InvalidField)
    );

    let empty = CONFIGURATION.replace(
        "Summarize the prior conversation faithfully for continuation.",
        "",
    );
    assert_eq!(
        HubModelConfiguration::parse(&empty).err(),
        Some(HubModelConfigurationError::InvalidCompactionPrompt)
    );

    let nul = CONFIGURATION.replace(
        "Summarize the prior conversation faithfully for continuation.",
        "contains\\u0000nul",
    );
    assert_eq!(
        HubModelConfiguration::parse(&nul).err(),
        Some(HubModelConfigurationError::InvalidCompactionPrompt)
    );

    let oversized_prompt = "x".repeat(MAX_COMPACTION_PROMPT_UTF8_BYTES + 1);
    let oversized = CONFIGURATION.replace(
        "Summarize the prior conversation faithfully for continuation.",
        &oversized_prompt,
    );
    assert_eq!(
        HubModelConfiguration::parse(&oversized).err(),
        Some(HubModelConfigurationError::InvalidCompactionPrompt)
    );
}

#[test]
fn configuration_enforces_the_protocol_alias_catalog_capacity() {
    assert_eq!(
        validate_alias_count(signalbox_process_protocol::MAX_MODEL_ALIAS_CATALOG_ENTRIES),
        Ok(())
    );
    assert_eq!(
        validate_alias_count(signalbox_process_protocol::MAX_MODEL_ALIAS_CATALOG_ENTRIES + 1),
        Err(HubModelConfigurationError::TooManyAliases)
    );
}

#[test]
fn configuration_enforces_the_protocol_model_capability_catalog_capacity() {
    assert_eq!(
        validate_model_count(signalbox_process_protocol::MAX_MODEL_CAPABILITY_CATALOG_ENTRIES),
        Ok(())
    );
    assert_eq!(
        validate_model_count(signalbox_process_protocol::MAX_MODEL_CAPABILITY_CATALOG_ENTRIES + 1),
        Err(HubModelConfigurationError::TooManyModels)
    );
}

#[test]
fn configuration_rejects_reasoning_levels_the_selected_adapter_cannot_map() {
    let configuration = CONFIGURATION.replace(
        "context_window_tokens = 200000",
        "context_window_tokens = 200000\nreasoning_levels = [\"ultra\"]",
    );

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidModelCapabilities)
    );
}

#[test]
fn configuration_copies_named_profile_and_global_settings_layers_per_model() {
    let configuration = CONFIGURATION
            .replace(
                "version = 1",
                "version = 1\n\n[model_settings]\nreasoning_level = \"low\"\n\n[[model_settings_profiles]]\nname = \"deliberate\"\nreasoning_level = \"high\"\nfast_mode = \"enabled\"\nservice_tier = { provider = \"anthropic\", value = \"standard_only\" }",
            )
            .replace(
                "context_window_tokens = 200000",
                "context_window_tokens = 200000\nreasoning_levels = [\"low\", \"high\"]\nfast_mode = \"request_control\"\nservice_tiers = [\"standard_only\"]\nsettings_profile = \"deliberate\"",
            );
    let configured = HubModelConfiguration::parse(&configuration)
        .expect("the selected model supports its copied lower layers");
    let (profile, global_default) = configured
        .model_settings_lower_layers(configured_judge_selection_fixture())
        .expect("the direct model has copied lower settings layers");
    let validated = configured
        .validate_session_model_settings(
            ModelSelectionRequest::Direct(configured_judge_selection_fixture()),
            ModelSettingsOverlay::inherit_all(),
        )
        .expect("the direct model is configured")
        .expect("the inherited settings chain is supported");

    assert_eq!(
        profile.reasoning_level(),
        SettingOverlay::Value(ReasoningLevel::High)
    );
    assert_eq!(
        global_default.reasoning_level(),
        SettingOverlay::Value(ReasoningLevel::Low)
    );
    assert_eq!(
        profile.fast_mode(),
        FastModeOverlay::Value(FastMode::Enabled)
    );
    assert_eq!(
        profile.service_tier(),
        SettingOverlay::Value(ServiceTier::Anthropic(AnthropicServiceTier::StandardOnly))
    );
    assert_eq!(
        validated.effective().reasoning_level(),
        Some(ReasoningLevel::High)
    );
    assert_eq!(
        validated.resolved().reasoning_source(),
        Some(ModelSettingSource::Profile)
    );
}

#[test]
fn configuration_rejects_a_lower_layer_unsupported_by_the_selected_model() {
    let configuration = CONFIGURATION.replace(
        "version = 1",
        "version = 1\n\n[model_settings]\nreasoning_level = \"low\"",
    );

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidModelSettingsConfiguration)
    );
}

/// every explicit lower layer is validated even when a higher-precedence layer masks it in the
/// effective configuration.
#[test]
fn configuration_rejects_an_unsupported_global_value_masked_by_a_profile() {
    let profile_configuration = CONFIGURATION
            .replace(
                "version = 1",
                "version = 1\n\n[[model_settings_profiles]]\nname = \"provider-defaults\"\nreasoning_level = \"provider_default\"",
            )
            .replace(
                "context_window_tokens = 200000",
                "context_window_tokens = 200000\nsettings_profile = \"provider-defaults\"",
            );
    HubModelConfiguration::parse(&profile_configuration)
        .expect("the selected profile is supported without the masked global layer");
    let configuration = profile_configuration.replace(
        "version = 1",
        "version = 1\n\n[model_settings]\nreasoning_level = \"low\"",
    );

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidModelSettingsConfiguration)
    );
}

/// an explicit unsupported selected-profile value is rejected even when the global layer is
/// valid.
#[test]
fn configuration_rejects_an_unsupported_selected_profile_value() {
    let configuration = CONFIGURATION
            .replace(
                "version = 1",
                "version = 1\n\n[[model_settings_profiles]]\nname = \"unsupported\"\nreasoning_level = \"low\"",
            )
            .replace(
                "context_window_tokens = 200000",
                "context_window_tokens = 200000\nsettings_profile = \"unsupported\"",
            );

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidModelSettingsConfiguration)
    );
}

/// a selected profile cannot combine individually supported controls that its adapter cannot
/// enforce together.
#[test]
fn configuration_rejects_an_adapter_incompatible_selected_profile() {
    let configuration = CONFIGURATION
            .replace(
                "version = 1",
                "version = 1\n\n[[model_settings_profiles]]\nname = \"incompatible\"\nfast_mode = \"enabled\"\nservice_tier = { provider = \"anthropic\", value = \"auto\" }",
            )
            .replace(
                "context_window_tokens = 200000",
                "context_window_tokens = 200000\nfast_mode = \"request_control\"\nservice_tiers = [\"auto\"]\nsettings_profile = \"incompatible\"",
            );

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidModelSettingsConfiguration)
    );
}

/// an adapter-incompatible global combination remains invalid when a selected profile masks it
/// with a supported combination.
#[test]
fn configuration_rejects_a_masked_adapter_incompatible_global_layer() {
    let configuration = CONFIGURATION
            .replace(
                "version = 1",
                "version = 1\n\n[model_settings]\nfast_mode = \"enabled\"\nservice_tier = { provider = \"anthropic\", value = \"auto\" }\n\n[[model_settings_profiles]]\nname = \"standard-tier\"\nservice_tier = { provider = \"anthropic\", value = \"standard_only\" }",
            )
            .replace(
                "context_window_tokens = 200000",
                "context_window_tokens = 200000\nfast_mode = \"request_control\"\nservice_tiers = [\"auto\", \"standard_only\"]\nsettings_profile = \"standard-tier\"",
            );

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidModelSettingsConfiguration)
    );
}

#[test]
fn configuration_rejects_a_selectable_model_as_an_alternate_fast_target() {
    let configuration = format!(
            r#"{}

[[models]]
selection_id = "10000000-0000-4000-8000-000000000002"
target_id = "20000000-0000-4000-8000-000000000002"
model_family = "anthropic"
provider_model = "synthetic-selectable-fast-target"
max_output_tokens = 256
context_window_tokens = 200000
"#,
            CONFIGURATION.replace(
                "context_window_tokens = 200000",
                "context_window_tokens = 200000\nfast_mode = \"alternate_target\"\nfast_target_id = \"20000000-0000-4000-8000-000000000002\"",
            )
        );

    assert_eq!(
        HubModelConfiguration::parse(&configuration).err(),
        Some(HubModelConfigurationError::InvalidModelCapabilities)
    );
}

#[test]
fn alternate_target_selects_its_serving_family_credential_profile() {
    let fast_family = "anthropic-fast";
    let fast_profile = ANTHROPIC_OVERFLOW_PROFILE;
    let configuration = format!(
            r#"{}

[[credential_pools]]
name = "{fast_family}"
tie_break = "first_listed"
on_pool_exhausted = "fail"
members = [{{ profile = "{fast_profile}", priority = 1 }}]

[[adapter_mappings]]
model_family = "{fast_family}"
adapter = "anthropic"
credential_pool = "{fast_family}"

[[serving_targets]]
target_id = "20000000-0000-4000-8000-000000000002"
model_family = "{fast_family}"
provider_model = "synthetic-fast-target"
max_output_tokens = 256
context_window_tokens = 200000
"#,
            CONFIGURATION.replace(
                "context_window_tokens = 200000",
                "context_window_tokens = 200000\nfast_mode = \"alternate_target\"\nfast_target_id = \"20000000-0000-4000-8000-000000000002\"",
            )
        );
    let configuration =
        HubModelConfiguration::parse(&configuration).expect("serving family is valid");
    let selected_target = configured_target(&configuration);
    let credential_pin = configuration.session_credential_pin();
    let serving_credential = credential_pin
        .credentials()
        .find(|credential| credential.model_family() == fast_family)
        .expect("serving family has a pinned credential");

    assert_eq!(
        configuration
            .credential_family_catalog()
            .family_for_call(selected_target, FastMode::Enabled),
        Some(fast_family)
    );
    assert_eq!(serving_credential.credential_reference(), fast_profile);
}

/// credential references stay scoped while paths and values stay
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
            .resolve(
                &source
                    .credential_reference()
                    .expect("the fixture source has one reference"),
            )
            .await
            .expect_err("fixture path does not exist")
            .failure,
        CredentialAccessFailure::Unavailable
    );
    assert!(!format!("{source:?}").contains("definitely"));
}

/// each operation preparation observes the file as it exists at
/// that request, so atomic deployment replacement rotates the key without
/// caching secret bytes in hub composition.
#[tokio::test]
async fn file_credentials_are_reread_for_rotation() {
    let path = std::env::temp_dir().join(format!("signalbox-credential-{}", Uuid::now_v7()));
    std::fs::write(&path, b"first-test-value").expect("fixture file is writable");
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("private credential fixture");
    let source = FileCredentialAccess::new(
        path.clone(),
        CredentialReference::new(ANTHROPIC_CREDENTIAL_REFERENCE),
    );
    let reference = source
        .credential_reference()
        .expect("the fixture source has one reference");
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

/// a historical session pin can resolve any declared file
/// profile, not only the member currently preferred by a pool.
#[tokio::test]
async fn file_credential_catalog_resolves_each_declared_profile() {
    let directory = tempfile::tempdir().expect("fixture directory is available");
    let primary_path = directory.path().join("primary");
    let historical_path = directory.path().join("historical");
    let primary_value = b"primary-test-value";
    let historical_value = b"historical-test-value";
    std::fs::write(&primary_path, primary_value).expect("primary fixture is writable");
    std::fs::set_permissions(
        &primary_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .expect("private credential fixture");
    std::fs::write(&historical_path, historical_value).expect("historical fixture is writable");
    std::fs::set_permissions(
        &historical_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .expect("private credential fixture");
    let primary = CredentialReference::new("primary-profile");
    let historical = CredentialReference::new("historical-profile");
    let source = FileCredentialAccess::from_files([
        (primary.clone(), primary_path),
        (historical.clone(), historical_path),
    ]);

    assert_eq!(
        source
            .resolve(&primary)
            .await
            .expect("primary profile resolves")
            .expose_bytes(),
        primary_value
    );
    assert_eq!(
        source
            .resolve(&historical)
            .await
            .expect("historical profile resolves")
            .expose_bytes(),
        historical_value
    );
}

/// A credential-printing tool terminates the line it writes, so the
/// terminator is how the file ends rather than part of the secret.
#[test]
fn credential_file_trailing_line_feed_is_not_part_of_the_value() {
    assert_eq!(
        credential_bytes(b"synthetic-token-value\n"),
        b"synthetic-token-value"
    );
}

/// A file written with CRLF line endings ends the same way, so both
/// terminator bytes fall outside the value.
#[test]
fn credential_file_trailing_carriage_return_line_feed_is_not_part_of_the_value() {
    assert_eq!(
        credential_bytes(b"synthetic-token-value\r\n"),
        b"synthetic-token-value"
    );
}

/// Only trailing termination is dropped: a value carrying interior line
/// termination is still delivered whole, so narrowing can never truncate a
/// credential at its first line.
#[test]
fn credential_file_interior_line_termination_is_retained() {
    assert_eq!(
        credential_bytes(b"synthetic\ntoken\nvalue\n"),
        b"synthetic\ntoken\nvalue"
    );
}

/// A file holding nothing but termination narrows to an empty value, which
/// the adapter boundary refuses exactly as it already refuses an empty
/// file — narrowing never invents a credential.
#[test]
fn credential_file_of_only_line_termination_narrows_to_an_empty_value() {
    assert_eq!(credential_bytes(b"\r\n\n"), b"");
}

/// The narrowing is wired into the file read itself, so every adapter that
/// resolves a reference receives the bare secret rather than the bytes the
/// writing tool happened to leave behind.
#[tokio::test]
async fn file_credentials_resolve_a_terminated_file_to_the_bare_value() {
    let path = std::env::temp_dir().join(format!("signalbox-credential-{}", Uuid::now_v7()));
    std::fs::write(&path, b"synthetic-token-value\n").expect("fixture file is writable");
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("private credential fixture");
    let source = FileCredentialAccess::new(
        path.clone(),
        CredentialReference::new(ANTHROPIC_CREDENTIAL_REFERENCE),
    );

    let resolved = source
        .resolve(
            &source
                .credential_reference()
                .expect("the fixture source has one reference"),
        )
        .await
        .expect("fixture value resolves");

    assert_eq!(resolved.expose_bytes(), b"synthetic-token-value");
    std::fs::remove_file(path).expect("fixture file is removable");
}

#[test]
fn configuration_admits_codex_least_used_ties() {
    let pool = r#"[[credential_pools]]
name = "codex-main"
tie_break = "least_used"
on_pool_exhausted = "fail"
members = [{ profile = "codex-subscription-primary", priority = 1 }]"#;
    HubModelConfiguration::parse(&CONFIGURATION.replace(CODEX_POOL, pool))
        .expect("Codex capacity evidence admits least_used");
}

#[test]
fn configuration_admits_codex_pool_headroom_reserve() {
    let pool = r#"[[credential_pools]]
name = "codex-main"
tie_break = "first_listed"
on_pool_exhausted = "fail"
headroom_reserve_percent = 10
members = [{ profile = "codex-subscription-primary", priority = 1 }]"#;
    HubModelConfiguration::parse(&CONFIGURATION.replace(CODEX_POOL, pool))
        .expect("Codex capacity evidence admits the pool reserve");
}

#[test]
fn configuration_admits_codex_member_headroom_reserve() {
    let pool = r#"[[credential_pools]]
name = "codex-main"
tie_break = "first_listed"
on_pool_exhausted = "fail"
members = [{ profile = "codex-subscription-primary", priority = 1, headroom_reserve_percent = 23 }]"#;
    HubModelConfiguration::parse(&CONFIGURATION.replace(CODEX_POOL, pool))
        .expect("Codex capacity evidence admits the member reserve");
}

#[test]
fn configuration_admits_codex_headroom_actions_between_calls() {
    for action in [
        "stay",
        "switch_next_turn",
        "avoid_new_sessions",
        "quarantine",
    ] {
        let pool = format!("{CODEX_POOL}\non_headroom_low = \"{action}\"");
        assert!(
            HubModelConfiguration::parse(&CONFIGURATION.replace(CODEX_POOL, &pool)).is_ok(),
            "Codex capacity evidence admits {action}"
        );
    }
}

#[test]
fn configuration_rejects_codex_switch_now_on_low_headroom() {
    let pool = format!("{CODEX_POOL}\non_headroom_low = \"switch_now\"");
    assert_eq!(
        HubModelConfiguration::parse(&CONFIGURATION.replace(CODEX_POOL, &pool)).err(),
        Some(
            HubModelConfigurationError::InadmissibleCredentialPoolAction {
                trigger: Arc::from("on_headroom_low"),
            }
        ),
    );
}

#[test]
fn configuration_projects_codex_capacity_policy_into_runtime_catalog() {
    let pool = r#"[[credential_pools]]
name = "codex-main"
tie_break = "least_used"
on_pool_exhausted = "fail"
headroom_reserve_percent = 10
on_headroom_low = "switch_next_turn"
members = [{ profile = "codex-subscription-primary", priority = 2, headroom_reserve_percent = 23 }]"#;
    let directory = tempfile::tempdir().expect("fixture directory is available");
    let executable = std::env::current_exe().expect("test executable has a path");
    let source = configuration_with_codex_paths(&executable, directory.path());
    let source = format!(
        r#"{source}
[[models]]
selection_id = "10000000-0000-4000-8000-000000000002"
target_id = "20000000-0000-4000-8000-000000000002"
model_family = "codex"
provider_model = "gpt-example"
max_output_tokens = 256
context_window_tokens = 200000
"#
    );
    let configuration = HubModelConfiguration::parse(&source.replace(CODEX_POOL, pool))
        .expect("Codex admits capacity policy");
    let catalog = configuration.credential_pool_runtime_catalog();
    let projected = catalog
        .values()
        .find(|policy| policy.name() == "codex-main");
    let expected = CredentialPoolRuntimePolicy::new(
        "codex-main",
        vec![
            CredentialPoolRuntimeMember::new(
                "codex-subscription-primary",
                std::num::NonZeroU32::new(2).expect("configured priority is nonzero"),
            )
            .with_headroom_reserve(Some(23)),
        ],
        CredentialPoolRuntimeExhaustion::Fail,
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
    )
    .with_capacity_policy(
        CredentialPoolRuntimeTieBreak::LeastUsed,
        Some(10),
        CredentialPoolRuntimeAction::SwitchNextTurn,
    );
    assert_eq!(projected, Some(&expected));
}

#[test]
fn reasoning_replay_family_changes_only_request_capabilities() {
    let source = format!("{CONFIGURATION}{OPENAI_MAPPING_AND_MODEL}");
    let untagged = HubModelConfiguration::parse(&source).expect("untagged targets are valid");
    let target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
        Uuid::parse_str("20000000-0000-4000-8000-00000000000e").expect("fixture target UUID"),
    ));
    assert_eq!(
        untagged
            .runtime_model_capability_catalog()
            .resolve(&signalbox_model_runtime::ResolvedTarget::new("gpt-example"))
            .expect("OpenAI capability")
            .reasoning_replay_family(),
        None
    );
    let tagged = HubModelConfiguration::parse(&source.replace(
        "provider_model = \"gpt-example\"",
        "provider_model = \"gpt-example\"\nreasoning_replay_family = \"shared\"",
    ))
    .expect("OpenAI family is valid");
    assert_eq!(
        untagged
            .runtime_model_catalog()
            .resolve(target)
            .expect("untagged OpenAI target"),
        tagged
            .runtime_model_catalog()
            .resolve(target)
            .expect("tagged OpenAI target"),
        "replay family does not change the producer definition"
    );
    assert_eq!(
        tagged
            .runtime_model_capability_catalog()
            .resolve(&signalbox_model_runtime::ResolvedTarget::new("gpt-example"))
            .expect("OpenAI capability")
            .reasoning_replay_family(),
        Some("shared")
    );
}

#[test]
fn reasoning_replay_family_rejects_malformed_or_non_openai_declarations() {
    for value in ["true", "\"\"", "\" padded \""] {
        let source = format!("{CONFIGURATION}{OPENAI_MAPPING_AND_MODEL}").replace(
            "provider_model = \"gpt-example\"",
            &format!("provider_model = \"gpt-example\"\nreasoning_replay_family = {value}"),
        );
        assert_eq!(
            HubModelConfiguration::parse(&source).err(),
            Some(HubModelConfigurationError::InvalidModelCapabilities),
            "{value}"
        );
    }
    let non_openai = CONFIGURATION.replace(
        "provider_model = \"claude-example\"",
        "provider_model = \"claude-example\"\nreasoning_replay_family = \"shared\"",
    );
    assert_eq!(
        HubModelConfiguration::parse(&non_openai).err(),
        Some(HubModelConfigurationError::InvalidModelCapabilities)
    );
}

#[test]
fn reasoning_replay_family_must_match_every_entry_for_one_provider_model() {
    let source = format!("{CONFIGURATION}{OPENAI_MAPPING_AND_MODEL}").replace(
        "provider_model = \"gpt-example\"",
        "provider_model = \"gpt-example\"\nreasoning_replay_family = \"shared\"",
    );
    for family_line in ["reasoning_replay_family = \"other\"", ""] {
        for (inventory, selection_line) in [
            (
                "models",
                "selection_id = \"10000000-0000-4000-8000-00000000000f\"\nreasoning_levels = [\"minimal\", \"medium\", \"xhigh\"]\nfast_mode = \"request_control\"\nservice_tiers = [\"flex\", \"priority\"]\n",
            ),
            ("serving_targets", ""),
        ] {
            let entry = format!(
                r#"
[[{inventory}]]
target_id = "20000000-0000-4000-8000-00000000000f"
model_family = "openai"
provider_model = "gpt-example"
max_output_tokens = 256
context_window_tokens = 200000
{family_line}
"#
            );
            assert_eq!(
                HubModelConfiguration::parse(&format!("{source}{entry}{selection_line}")).err(),
                Some(HubModelConfigurationError::InvalidModelCapabilities),
                "{inventory}: {family_line}"
            );
        }
    }
}

#[test]
fn reasoning_replay_family_is_retained_on_the_effective_serving_target() {
    let source = format!("{CONFIGURATION}{OPENAI_MAPPING_AND_MODEL}").replace(
        "fast_mode = \"request_control\"", "fast_mode = \"alternate_target\"\nfast_target_id = \"20000000-0000-4000-8000-00000000000f\"",
    );
    let source = format!(
        r#"{source}
[[serving_targets]]
target_id = "20000000-0000-4000-8000-00000000000f"
model_family = "openai"
provider_model = "gpt-fast"
max_output_tokens = 256
context_window_tokens = 200000
reasoning_replay_family = "shared"
"#
    );
    let configuration = HubModelConfiguration::parse(&source)
        .expect("a fast serving target declares its own replay family");
    let catalog = configuration.runtime_model_capability_catalog();
    let selected = signalbox_model_runtime::ResolvedTarget::new("gpt-example");
    let (effective, _) = catalog
        .resolve(&selected)
        .expect("selected target")
        .effective_target(&selected, signalbox_model_runtime::FastMode::Enabled, None)
        .expect("fast target");
    assert_eq!(
        catalog
            .resolve(effective)
            .expect("effective target")
            .reasoning_replay_family(),
        Some("shared")
    );
    let durable_target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
        Uuid::parse_str("20000000-0000-4000-8000-00000000000f").expect("fixture target UUID"),
    ));
    assert!(
        configuration
            .runtime_model_catalog()
            .resolve(durable_target)
            .is_some()
    );
}

#[test]
fn reasoning_replay_family_accepts_matching_declarations_for_one_provider_model() {
    let source = format!("{CONFIGURATION}{OPENAI_MAPPING_AND_MODEL}").replace(
        "provider_model = \"gpt-example\"",
        "provider_model = \"gpt-example\"\nreasoning_replay_family = \"shared\"",
    );
    let alias = OPENAI_MAPPING_AND_MODEL
        .split("[[models]]")
        .nth(1)
        .expect("fixture model table")
        .replace("00000000000e", "00000000000f")
        .replace(
            "provider_model = \"gpt-example\"",
            "provider_model = \"gpt-example\"\nreasoning_replay_family = \"shared\"",
        );
    let source = format!(
        r#"{source}
[[models]]
{alias}
[[serving_targets]]
target_id = "20000000-0000-4000-8000-000000000010"
model_family = "openai"
provider_model = "gpt-example"
max_output_tokens = 256
context_window_tokens = 200000
reasoning_replay_family = "shared"
"#
    );
    let configuration = HubModelConfiguration::parse(&source)
        .expect("matching family declarations share a provider spelling");
    assert_eq!(
        configuration
            .runtime_model_capability_catalog()
            .resolve(&signalbox_model_runtime::ResolvedTarget::new("gpt-example"))
            .expect("OpenAI capability")
            .reasoning_replay_family(),
        Some("shared")
    );
}

#[test]
fn review_findings_policy_cannot_exceed_domain_admission() {
    for replacement in ["33", "\"none\""] {
        let document = CONFIGURATION.replace(
            "max_review_findings_per_run = 32",
            &format!("max_review_findings_per_run = {replacement}"),
        );
        assert!(matches!(
            HubModelConfiguration::parse(&document),
            Err(HubModelConfigurationError::InvalidNumericBound {
                field: "max_review_findings_per_run"
            })
        ));
    }
}

#[test]
fn imported_title_projection_admits_zero_unbounded_and_above_domain_limits() {
    for (replacement, expected) in [("0", Some(0)), ("257", Some(257)), ("\"none\"", None)] {
        let document = CONFIGURATION.replace(
            "max_imported_conversation_display_title_scalars = 256",
            &format!("max_imported_conversation_display_title_scalars = {replacement}"),
        );
        let configuration =
            HubModelConfiguration::parse(&document).expect("valid projection limit");
        assert_eq!(
            configuration
                .numeric_bounds()
                .integer("max_imported_conversation_display_title_scalars"),
            Some(expected),
            "{replacement}",
        );
    }
}

#[test]
fn replica_read_policy_must_admit_the_catalog_write_bound() {
    let document =
        CONFIGURATION.replace("max_blob_replica_count = 32", "max_blob_replica_count = 31");
    assert!(matches!(
        HubModelConfiguration::parse(&document),
        Err(HubModelConfigurationError::InvalidNumericBound {
            field: "max_blob_replica_count"
        })
    ));
}

#[test]
fn disabling_reconciliation_requires_lossless_nudge_handoff() {
    let document = CONFIGURATION.replace(
        "reconciliation_sweep_interval = \"1s\"",
        "reconciliation_sweep_interval = \"none\"",
    );
    assert!(matches!(
        HubModelConfiguration::parse(&document),
        Err(HubModelConfigurationError::InvalidNumericBound {
            field: "nudge_buffer_capacity"
        })
    ));
    let lossless = document.replace(
        "nudge_buffer_capacity = 1024",
        "nudge_buffer_capacity = \"none\"",
    );
    assert!(HubModelConfiguration::parse(&lossless).is_ok());
}

#[test]
fn repository_git_push_requires_an_absolute_file_reference_without_exposing_it() {
    let push_path = "/unused/push-only-token";
    let configured = configuration_with_repository_watch().replace(
        &format!("credential_file = \"{WATCH_CREDENTIAL_FILE}\""),
        &format!(
            "credential_file = \"{WATCH_CREDENTIAL_FILE}\"\npush_credential_file = \"{push_path}\""
        ),
    );
    let parsed = HubModelConfiguration::parse(&configured).expect("optional push credential");
    let repositories = parsed.repository_watch().expect("watch").repositories();
    assert_eq!(
        repositories[0].push_credential_file(),
        Some(std::path::Path::new(push_path))
    );
    assert_eq!(repositories[1].push_credential_file(), None);
    assert!(!format!("{:?}", repositories[0]).contains(push_path));
    assert!(
        HubModelConfiguration::parse(&configured.replace(push_path, "relative-token")).is_err()
    );
    assert!(
        HubModelConfiguration::parse(&configured.replace(push_path, "/unused/../token")).is_err()
    );
}

#[test]
fn repository_git_push_rejects_credentials_shared_across_roles_or_repositories() {
    // Distinct from both polling credentials and the webhook secret in the fixture.
    const PUSH_CREDENTIAL: &str = "/unused/push-only-token";
    const OTHER_PUSH_CREDENTIAL: &str = "/unused/other-push-only-token";
    struct Collision {
        name: &'static str,
        first_push: &'static str,
        second_push: &'static str,
    }
    for case in [
        Collision {
            name: "shared push secret",
            first_push: PUSH_CREDENTIAL,
            second_push: PUSH_CREDENTIAL,
        },
        Collision {
            name: "same repository polling secret",
            first_push: WATCH_CREDENTIAL_FILE,
            second_push: OTHER_PUSH_CREDENTIAL,
        },
        Collision {
            name: "later repository polling secret",
            first_push: SECOND_WATCH_CREDENTIAL_FILE,
            second_push: OTHER_PUSH_CREDENTIAL,
        },
        Collision {
            name: "earlier repository polling secret",
            first_push: PUSH_CREDENTIAL,
            second_push: WATCH_CREDENTIAL_FILE,
        },
        Collision {
            name: "same repository webhook secret",
            first_push: WATCH_WEBHOOK_SECRET_FILE,
            second_push: OTHER_PUSH_CREDENTIAL,
        },
        Collision {
            name: "earlier repository webhook secret",
            first_push: PUSH_CREDENTIAL,
            second_push: WATCH_WEBHOOK_SECRET_FILE,
        },
    ] {
        let mut configured = configuration_with_repository_watch_webhook()
            .parse::<toml_edit::DocumentMut>()
            .expect("watch fixture");
        let repositories = configured["repository_watch"]["repositories"]
            .as_array_of_tables_mut()
            .expect("watched repositories");
        for (repository, push) in repositories
            .iter_mut()
            .zip([case.first_push, case.second_push])
        {
            repository["push_credential_file"] = toml_edit::value(push);
        }
        assert_eq!(
            HubModelConfiguration::parse(&configured.to_string()).err(),
            Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile),
            "{}",
            case.name,
        );
    }
}

#[cfg(unix)]
#[test]
fn repository_git_push_rejects_a_dangling_symlink_to_a_polling_credential() {
    let directory = tempfile::tempdir().expect("credential directory");
    let credential = directory.path().join("pending-poll-token");
    let push_alias = directory.path().join("push-alias");
    std::os::unix::fs::symlink(&credential, &push_alias).expect("dangling push alias");
    let configured = configuration_with_repository_watch().replace(
        &format!("credential_file = \"{WATCH_CREDENTIAL_FILE}\""),
        &format!(
            "credential_file = \"{}\"\npush_credential_file = \"{}\"",
            credential.display(),
            push_alias.display()
        ),
    );
    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile),
    );
}

#[cfg(unix)]
#[test]
fn repository_git_push_rejects_a_hard_link_to_a_webhook_secret() {
    let directory = tempfile::tempdir().expect("credential directory");
    let secret = directory.path().join("webhook-secret");
    std::fs::write(&secret, []).expect("webhook secret");
    let push_alias = directory.path().join("push-alias");
    std::fs::hard_link(&secret, &push_alias).expect("push hard link");
    let configured = configuration_with_repository_watch_webhook()
        .replace(WATCH_WEBHOOK_SECRET_FILE, &secret.display().to_string())
        .replace(
            &format!("credential_file = \"{SECOND_WATCH_CREDENTIAL_FILE}\""),
            &format!("credential_file = \"{SECOND_WATCH_CREDENTIAL_FILE}\"\npush_credential_file = \"{}\"", push_alias.display()),
        );
    assert_eq!(
        HubModelConfiguration::parse(&configured).err(),
        Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile),
    );
}

#[test]
fn daemon_sandbox_defaults_do_not_expose_a_host_runtime() {
    let configuration = HubModelConfiguration::parse(CONFIGURATION).expect("fixture parses");
    assert_eq!(
        configuration
            .daemon_tools()
            .expect("daemon tools")
            .sandbox(),
        &signalbox_tools_exec::SandboxConfiguration::default()
    );
}

#[test]
fn daemon_sandbox_settings_preserve_explicit_runtime_inputs() {
    let runtime = tempfile::tempdir().expect("runtime directory");
    let settings = format!(
        r#"
sandbox_network = "host"
sandbox_read_only_binds = ["{path}"]
sandbox_path_prepend = ["{path}"]
sandbox_rustup_home = "{path}"
sandbox_rustup_toolchain = "stable"
"#,
        path = runtime.path().display()
    );
    let configured =
        CONFIGURATION.replace("[daemon_tools]", &format!("[daemon_tools]\n{settings}"));
    let configuration = HubModelConfiguration::parse(&configured).expect("explicit runtime parses");
    assert_eq!(
        configuration
            .daemon_tools()
            .expect("daemon tools")
            .sandbox(),
        &signalbox_tools_exec::SandboxConfiguration {
            network: signalbox_tools_exec::SandboxNetwork::Host,
            read_only_binds: vec![runtime.path().to_owned()],
            path_prepend: vec![runtime.path().to_owned()],
            rustup_home: Some(runtime.path().to_owned()),
            rustup_toolchain: Some(String::from("stable")),
        }
    );
}

#[test]
fn daemon_sandbox_settings_reject_malformed_runtime_inputs() {
    for settings in [
        "sandbox_network = 'registry'",
        "sandbox_network = true",
        "sandbox_read_only_binds = ['relative']",
        "sandbox_read_only_binds = ['/definitely-missing-sandbox-runtime']",
        "sandbox_read_only_binds = 'not-an-array'",
        "sandbox_path_prepend = [42]",
        "sandbox_rustup_home = 'relative'",
        "sandbox_rustup_toolchain = ''",
    ] {
        let configured =
            CONFIGURATION.replace("[daemon_tools]", &format!("[daemon_tools]\n{settings}"));
        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidDaemonToolSettings),
            "{settings}"
        );
    }
}

#[test]
fn repository_watch_poll_budget_defaults_to_one_hundred_requests() {
    assert_eq!(
        HubModelConfiguration::parse(CONFIGURATION)
            .expect("configuration")
            .numeric_bounds()
            .integer("repository_watch_poll_request_budget"),
        Some(Some(100))
    );
}

#[test]
fn repository_watch_poll_budget_accepts_finite_attempts_with_room_for_preflight() {
    const FIELD: &str = "repository_watch_poll_request_budget";
    for budget in [2, 7, 1000] {
        let source = CONFIGURATION.replace(
            "[numeric_bounds]",
            &format!("[numeric_bounds]\n{FIELD} = {budget}"),
        );
        assert_eq!(
            HubModelConfiguration::parse(&source)
                .expect("bounded request budget")
                .numeric_bounds()
                .integer(FIELD),
            Some(Some(budget))
        );
    }
}

#[test]
fn repository_watch_poll_budget_rejects_attempts_outside_its_bounds() {
    const FIELD: &str = "repository_watch_poll_request_budget";
    for invalid in ["0", "1", "1001", "-1", "1.5", "\"none\""] {
        let source = CONFIGURATION.replace(
            "[numeric_bounds]",
            &format!("[numeric_bounds]\n{FIELD} = {invalid}"),
        );
        assert_eq!(
            HubModelConfiguration::parse(&source).expect_err("invalid request budget"),
            HubModelConfigurationError::InvalidNumericBound { field: FIELD }
        );
    }
}

#[test]
fn file_media_requires_blob_storage_before_worker_startup() {
    let enabled = format!("file_media = true\n{CONFIGURATION}");
    assert_eq!(
        HubModelConfiguration::parse(&enabled).err(),
        Some(HubModelConfigurationError::InvalidBlobStorageConfiguration)
    );
    let disabled = format!("file_media = false\n{CONFIGURATION}");
    assert!(
        !HubModelConfiguration::parse(&disabled)
            .expect("disabled file tools require no store")
            .file_media()
    );
}

#[test]
fn checked_in_example_parses_unbounded_git_object_content() {
    let configuration =
        super::checked_in_example_configuration().expect("checked-in example parses");
    assert_eq!(
        configuration
            .numeric_bounds()
            .integer("max_git_object_bytes"),
        Some(None)
    );
    let configured = CONFIGURATION.replace(
        "max_git_object_bytes = \"none\"",
        "max_git_object_bytes = 1048576",
    );
    let configuration =
        HubModelConfiguration::parse(&configured).expect("finite Git object policy parses");
    assert_eq!(
        configuration
            .numeric_bounds()
            .integer("max_git_object_bytes"),
        Some(Some(1048576))
    );
}

#[test]
fn checked_in_example_admits_unlisted_web_origins() {
    use signalbox_application::ToolCatalog;
    let configuration = super::checked_in_example_configuration().expect("example parses");
    let (catalog, _) = signalbox_tools_web::WebFetchTool::try_new_production(
        configuration.web_fetch_egress_policy(),
    )
    .expect("web transport constructs")
    .into_parts();
    let name =
        signalbox_domain::ToolName::try_new(String::from(WEB_FETCH_NAME)).expect("valid name");
    let arguments = signalbox_domain::NormalizedToolArguments::try_from_provider_text(
        String::from(r#"{"url":"https://unlisted.example/documentation"}"#),
    )
    .expect("valid arguments");
    assert_eq!(catalog.validate_arguments(&name, &arguments), Ok(()));
}

#[test]
fn human_approval_wait_accepts_a_duration_or_none() {
    let timed = HubModelConfiguration::parse(&format!(
        "{CONFIGURATION}\n[tool_settings]\napproval_wait_timeout = \"2m\"\n"
    ))
    .expect("duration is valid");
    assert_eq!(
        timed.approval_wait_timeout(),
        Some(Duration::from_secs(120))
    );
    let unbounded = HubModelConfiguration::parse(&format!(
        "{CONFIGURATION}\n[tool_settings]\napproval_wait_timeout = \"none\"\n"
    ))
    .expect("none is valid");
    assert_eq!(unbounded.approval_wait_timeout(), None);
}

#[test]
fn human_approval_wait_rejects_zero_and_invalid_durations() {
    for value in ["0s", "-1s", "forever"] {
        assert!(
            HubModelConfiguration::parse(&format!(
                "{CONFIGURATION}\n[tool_settings]\napproval_wait_timeout = \"{value}\"\n"
            ))
            .is_err(),
            "{value}"
        );
    }
}
