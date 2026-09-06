//! Compatibility smoke against the real, pinned Codex CLI.
//!
//! Ignored by default: it spawns the installed executable, spends one real
//! model exchange, and therefore needs credentials the ordinary Rust workflow
//! never has. `.github/workflows/codex-smoke.yml` is the only automated caller:
//! its unprivileged gate rejects changed fork pull requests before the
//! environment-backed smoke job can start.
//!
//! What it proves is protocol compatibility, which is what a CLI version bump
//! actually breaks: the `codex exec --json` event stream still starts a thread,
//! still reports usage on `turn.completed`, still accepts every flag the
//! adapter passes, and its final response envelope still decodes as a completed
//! or refused terminal outcome. It deliberately asserts nothing about answer
//! quality.
//!
//! The version check runs first and fails closed. Without it a drifted local
//! or CI executable could satisfy every assertion below and be recorded as
//! evidence for a version it never ran.
//!
//! Credential discipline: this test never reads, receives, or logs credential
//! material. The CLI resolves its own login from `CODEX_HOME`, exactly as in
//! production. The version probe discards the child's stderr rather than
//! reporting it, and every other failure message carries only evidence the
//! adapter has already run through its credential-shape redaction.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::process::Stdio;
use std::time::Duration;

use signalbox_model_runtime::{
    BoundaryLossEvidence, CancellationSignal, CompletionEvidence, CompletionFinish,
    ConversationMessage, CredentialReference, DeliveryMode, ExchangeFacts, LossCause,
    ModelOperation, ModelRuntime, ModelSettings, PreparationDefect, PreparationFailure,
    PreparationOutcome, ProviderRequestId, RefusalEvidence, RequestedTarget, ResolvedTarget,
    TerminalEvidence, TokenUsage, ToolCallsAtLoss,
};
use signalbox_model_runtime_codex_cli::{
    CodexCliConfig, CodexCliPreparedRequest, CodexCliRuntime,
    DISABLED_CODEX_CLI_CAPABILITY_FEATURES, SUPPORTED_CODEX_CLI_VERSION,
};

/// Overrides the executable under test. The default resolves through `PATH`;
/// CI points it at the binary `npm ci` unpacked from the pin manifest.
const EXECUTABLE_VARIABLE: &str = "SIGNALBOX_CODEX_SMOKE_EXECUTABLE";

/// Overrides the model. The default is the cheapest model this CLI advertises
/// and the smoke has no reason to buy a more capable one, but which models a
/// credential may address is account-scoped — an API key and a subscription
/// login do not offer the same set — so the caller can name another.
const MODEL_VARIABLE: &str = "SIGNALBOX_CODEX_SMOKE_MODEL";

const DEFAULT_EXECUTABLE: &str = "codex";
/// The cheapest model the pinned CLI's own catalog advertises — the sole `-mini`
/// entry, which `codex debug models` describes as "Small, fast, and
/// cost-efficient". A slug the catalog does not carry is not merely a stale
/// default: the CLI reports the missing metadata as an error item and falls back
/// to generic metadata, so the smoke would record compatibility evidence from a
/// degraded run.
const DEFAULT_MODEL: &str = "gpt-5.4-mini";
const CODEX_SMOKE_WORKFLOW: &str = include_str!("../../../.github/workflows/codex-smoke.yml");
const LOGIN_TIMEOUT_INVOCATION: &str =
    "env -u CODEX_SMOKE_API_KEY timeout --signal=TERM --kill-after=5s 30s";
const LOGIN_TERM_HOLD_COMMAND: &str = r#"trap "while :; do sleep 1; done" TERM; "$@""#;
#[cfg(target_os = "linux")]
const LOGIN_TIMEOUT_DESCENDANT_FIXTURE: &str = r#"#!/bin/sh
pid_file=$1
sh -c 'trap "" TERM; printf "%s\n" "$$" > "$1"; while :; do sleep 1; done' sh "$pid_file" &
trap 'exit 0' TERM
wait
"#;

/// Exact built-in feature inventory reported by the pinned CLI. Stage and
/// default are part of the snapshot: a newly enabled feature is classification
/// drift even when its name already existed.
const PINNED_CODEX_FEATURE_INVENTORY: &str = r#"apply_patch_freeform                     removed            false
apply_patch_preserve_line_endings        under development  false
apply_patch_streaming_events             under development  false
apps                                     stable             true
apps_mcp_path_override                   removed            false
artifact                                 under development  false
auth_elicitation                         stable             true
background_paginated_rollout_migration   under development  false
bedrock_setup_wizard                     under development  false
browser_use                              stable             true
browser_use_external                     stable             true
browser_use_full_cdp_access              stable             true
chronicle                                under development  false
code_mode                                under development  false
code_mode_buffered_exec                  removed            false
code_mode_host                           stable             true
code_mode_interrupt                      under development  false
code_mode_only                           under development  false
code_mode_prewarm                        under development  false
codex_git_commit                         removed            false
collaboration_modes                      removed            true
compaction_image_budget                  stable             true
computer_use                             stable             true
concurrent_reasoning_summaries           under development  false
content_item_kinds                       stable             true
context_management                       under development  false
current_time_reminder                    under development  false
cwd_relative_turn_diffs                  under development  false
default_mode_request_user_input          under development  false
deferred_executor                        under development  false
deferred_tool_world_state                under development  false
elevated_windows_sandbox                 removed            false
enable_fanout                            removed            false
enable_mcp_apps                          under development  false
enable_request_compression               stable             true
exec_permission_approvals                under development  false
executed_tool_call_metadata              under development  false
executor_capability_discovery            under development  false
experimental_windows_sandbox             removed            false
external_agent_memory_import             under development  false
external_migration                       removed            false
fast_mode                                stable             true
goals                                    stable             true
guardian_approval                        stable             true
guardian_enhanced_node_repl_transcripts  under development  false
guardian_ext                             under development  false
guardian_node_repl_transcript_images     under development  false
guardian_reuse_parent_compaction         under development  false
guardianv2                               under development  false
hooks                                    stable             true
image_detail_original                    removed            false
image_generation                         stable             true
image_resize_notice                      under development  false
in_app_browser                           stable             true
in_app_chat                              stable             true
in_app_dictation                         stable             true
in_app_local_automation                  stable             true
in_app_updates                           stable             true
item_ids                                 removed            true
js_repl                                  removed            false
js_repl_tools_only                       removed            false
local_thread_store_compression           under development  false
local_thread_store_shared_compression    removed            false
mcp_2026_07_28                           under development  false
mcp_oauth_refresh_coordination           under development  false
memories                                 stable             false
mentions_v2                              stable             true
multi_agent                              stable             true
multi_agent_mode                         removed            false
multi_agent_v2                           stable             false
network_proxy                            experimental       false
non_prefixed_mcp_tool_names              under development  false
omit_app_server_notification_media       under development  false
personality                              stable             true
plugin_hooks                             removed            false
plugin_sharing                           stable             true
plugins                                  stable             true
powershell_shell_version                 under development  false
prevent_idle_sleep                       experimental       false
psp                                      under development  false
realtime_conversation                    under development  false
recommended_plugins                      stable             false
remote_compaction_v2                     stable             true
remote_control                           removed            false
remote_models                            removed            false
remote_plugin                            stable             true
request_permissions_tool                 under development  false
request_rule                             removed            false
resize_all_images                        removed            true
respect_system_proxy                     under development  false
responses_websockets                     removed            false
responses_websockets_v2                  removed            false
retain_client_developer_messages         under development  false
rollout_budget                           under development  false
runtime_metrics                          under development  false
search_tool                              removed            false
secret_auth_storage                      stable             false
send_async_message                       removed            false
shell_snapshot                           stable             true
shell_snapshot_v2                        under development  false
shell_tool                               stable             true
shell_zsh_fork                           under development  false
skill_env_var_dependency_prompt          removed            false
skill_mcp_dependency_install             stable             true
skill_search                             stable             true
skip_host_skill_discovery                under development  false
sleep_tool                               stable             true
sqlite                                   removed            true
standalone_web_search                    under development  false
steer                                    removed            true
step_model_switching                     under development  false
terminal_resize_reflow                   removed            true
terminal_visualization_instructions      under development  false
token_budget                             under development  false
tool_call_mcp_elicitation                stable             true
tool_search                              removed            false
tool_search_always_defer_mcp_tools       removed            true
tool_suggest                             stable             true
transcript_v2                            under development  false
tui_app_server                           removed            true
unavailable_dummy_tools                  removed            false
unbounded_connection_retries             stable             true
undo                                     removed            false
unified_exec                             stable             true
unified_exec_zsh_fork                    removed            true
unified_image_budget                     under development  false
use_agent_identity                       under development  false
use_legacy_landlock                      deprecated         false
use_linux_sandbox_bwrap                  removed            false
view_image                               stable             true
web_search_cached                        deprecated         false
web_search_request                       deprecated         false
workspace_dependencies                   stable             true
workspace_owner_usage_nudge              removed            false
write_stdin_approval                     under development  false
"#;

/// Inventory entries that do not add a model-visible tool, external
/// interaction, instruction source, or delegated execution surface. Every
/// other entry is in the runtime's exported hard-disable list above.
///
/// Entries that only select *how* a surface classified above behaves — which
/// shell an already-disabled executor forks, what a disabled reviewer is shown,
/// how the CLI encodes, budgets, stores, or cancels work it would do anyway —
/// belong here even when a same-prefix sibling is hard-disabled: enabling one
/// alone opens nothing. Only the entry that decides whether the surface exists
/// is the capability.
const NON_CAPABILITY_CODEX_FEATURES: &[&str] = &[
    "apply_patch_freeform",
    "apply_patch_preserve_line_endings",
    "apply_patch_streaming_events",
    "apps_mcp_path_override",
    "background_paginated_rollout_migration",
    "bedrock_setup_wizard",
    "chronicle",
    "code_mode_interrupt",
    "code_mode_prewarm",
    "codex_git_commit",
    "collaboration_modes",
    "compaction_image_budget",
    "concurrent_reasoning_summaries",
    "content_item_kinds",
    "cwd_relative_turn_diffs",
    "elevated_windows_sandbox",
    "enable_fanout",
    "enable_request_compression",
    "executed_tool_call_metadata",
    "experimental_windows_sandbox",
    "external_migration",
    "fast_mode",
    "guardian_enhanced_node_repl_transcripts",
    "guardian_node_repl_transcript_images",
    "guardian_reuse_parent_compaction",
    "image_detail_original",
    "image_resize_notice",
    "item_ids",
    "js_repl",
    "js_repl_tools_only",
    "local_thread_store_compression",
    "local_thread_store_shared_compression",
    "mentions_v2",
    "multi_agent_mode",
    "network_proxy",
    "non_prefixed_mcp_tool_names",
    "omit_app_server_notification_media",
    "personality",
    "plugin_hooks",
    "prevent_idle_sleep",
    "psp",
    "remote_compaction_v2",
    "remote_control",
    "remote_models",
    "request_rule",
    "resize_all_images",
    "respect_system_proxy",
    "responses_websockets",
    "responses_websockets_v2",
    "retain_client_developer_messages",
    "rollout_budget",
    "runtime_metrics",
    "search_tool",
    "secret_auth_storage",
    "send_async_message",
    "shell_zsh_fork",
    "skill_env_var_dependency_prompt",
    "skip_host_skill_discovery",
    "sqlite",
    "steer",
    "terminal_resize_reflow",
    "terminal_visualization_instructions",
    "tool_search",
    "tool_search_always_defer_mcp_tools",
    "transcript_v2",
    "tui_app_server",
    "unavailable_dummy_tools",
    "unbounded_connection_retries",
    "undo",
    "unified_exec_zsh_fork",
    "unified_image_budget",
    "use_agent_identity",
    "use_legacy_landlock",
    "use_linux_sandbox_bwrap",
    "web_search_cached",
    "web_search_request",
    "workspace_owner_usage_nudge",
    "write_stdin_approval",
];

/// A trivial prompt keeps the exchange to the smallest billable turn that
/// still exercises the whole event protocol.
const PROMPT: &str = "Reply with the single word: ready";

const SKILL_INSTRUCTIONS_CONFIG: &str = "skills.include_instructions=false";
const SKILL_INSTRUCTIONS_BLOCK: &str = "<skills_instructions>";
const SYNTHETIC_SKILL_NAME: &str = "ambient_probe";
const SYNTHETIC_SKILL_DESCRIPTION: &str = "ambient-skill-injection-description-probe-317";
const SYNTHETIC_SKILL_BODY: &str = "ambient-skill-injection-body-probe-317";

/// The synthetic `SKILL.md`, rendered from the three probe tokens the
/// assertions read, so no assertion compares against a literal this fixture
/// also states independently.
fn synthetic_skill_file() -> String {
    format!(
        "---\n\
         name: {SYNTHETIC_SKILL_NAME}\n\
         description: {SYNTHETIC_SKILL_DESCRIPTION}\n\
         ---\n\
         \n\
         # Ambient probe\n\
         \n\
         {SYNTHETIC_SKILL_BODY}\n"
    )
}

/// Arbitrary non-default facts that prove the shared response projection
/// preserves terminal evidence rather than manufacturing defaults.
const FIXTURE_THREAD_ID: &str = "fixture-thread";
const FIXTURE_INPUT_TOKENS: u64 = 3;
const FIXTURE_OUTPUT_TOKENS: u64 = 1;

#[tokio::test]
#[ignore = "spends one real Codex CLI exchange; run only from the gated compatibility smoke"]
async fn the_pinned_codex_cli_completes_one_exchange() {
    let executable = absolute_executable(&executable_override_or_default());
    let model = variable_or(MODEL_VARIABLE, DEFAULT_MODEL);

    assert_pre_spend_contract(&executable).await;

    let working_directory = tempfile::tempdir().expect("smoke working directory is created");
    let credential_reference = CredentialReference::new("codex-smoke");
    let mut config = CodexCliConfig::new(
        &executable,
        working_directory.path(),
        credential_reference.clone(),
        None,
    );
    config.exchange_timeout = Some(Duration::from_secs(4 * 60));
    let runtime = CodexCliRuntime::new(config).expect("smoke runtime configuration is valid");

    let mut operation = ModelOperation::new(
        "codex-smoke".to_string(),
        credential_reference,
        RequestedTarget::new(&model),
        ResolvedTarget::new(&model),
        vec![ConversationMessage::user_text(PROMPT)],
        ModelSettings::new(64),
    );
    operation.delivery = DeliveryMode::Buffered;

    let prepared = require_prepared(
        runtime
            .prepare(operation, CancellationSignal::never())
            .await,
    );
    let mut observations = Vec::new();
    let report = runtime
        .execute(prepared, &mut observations, CancellationSignal::never())
        .await;

    let decoded = require_decoded_response(report.evidence);
    assert!(
        decoded.exchange.provider_request_id.is_some(),
        "no thread id reached the exchange facts, so `thread.started` no longer \
         parses for model {model}"
    );
    assert!(
        decoded.usage.input_tokens.is_some_and(|tokens| tokens > 0)
            && decoded.usage.output_tokens.is_some(),
        "`turn.completed` no longer reports the usage counters the adapter reads"
    );
}

/// Credential-free entry point for the real-CLI controls that must pass before
/// the live smoke authenticates or spends. Install the pinned CLI, point
/// `SIGNALBOX_CODEX_SMOKE_EXECUTABLE` at it, and run this exact ignored test.
#[tokio::test]
#[ignore = "requires the installed pinned CLI; credential-free pre-spend probes only"]
async fn the_pinned_codex_cli_pre_spend_contract_holds() {
    let executable = absolute_executable(&executable_override_or_default());

    assert_pre_spend_contract(&executable).await;
}

async fn assert_pre_spend_contract(executable: &std::path::Path) {
    assert_pinned_version(executable).await;
    assert_pinned_feature_inventory(executable).await;
    assert_ambient_skill_instructions_disabled(executable).await;
}

/// `CodexCliPreparedRequest` deliberately implements no diagnostic formatting,
/// so each non-prepared outcome reports only its safe shared-runtime evidence.
#[track_caller]
fn require_prepared(
    outcome: PreparationOutcome<String, CodexCliPreparedRequest<String>>,
) -> CodexCliPreparedRequest<String> {
    match outcome {
        PreparationOutcome::Prepared(prepared) => prepared,
        PreparationOutcome::Cancelled { .. } => panic!("smoke preparation was not cancelled"),
        PreparationOutcome::Failed { failure, .. } => {
            panic!("smoke preparation failed: {failure:?}")
        }
        PreparationOutcome::Defect { defect, .. } => {
            panic!("smoke preparation found a defect: {defect:?}")
        }
    }
}

struct DecodedResponse {
    exchange: ExchangeFacts,
    usage: TokenUsage,
}

#[track_caller]
fn require_decoded_response(evidence: TerminalEvidence) -> DecodedResponse {
    match evidence {
        TerminalEvidence::Completed(completed) => DecodedResponse {
            exchange: completed.exchange,
            usage: completed.usage,
        },
        TerminalEvidence::Refused(refused) => DecodedResponse {
            exchange: refused.exchange,
            usage: refused.usage,
        },
        // Adapter-produced evidence is already credential-shape redacted, so
        // printing it here cannot surface credential material.
        other => panic!("the pinned Codex CLI returned no decoded response: {other:?}"),
    }
}

/// Spawns the version probe, retrying briefly on `ETXTBSY`. A freshly written
/// fixture executable can transiently report "text file busy" when a
/// concurrent test thread forks with the still-open write descriptor
/// inherited across the exec; the parent's own write handle is already closed,
/// so a short bounded retry clears the race without masking a real failure.
async fn spawn_probe(
    command: &mut tokio::process::Command,
    executable: &std::path::Path,
) -> tokio::process::Child {
    spawn_with_retry(executable.display(), || command.spawn()).await
}

const MAX_SPAWN_ATTEMPTS: usize = 50;
const SPAWN_RETRY_DELAY: Duration = Duration::from_millis(20);

/// Drives the bounded `ETXTBSY` retry over an injectable spawn, so the
/// retry-to-success, exhaustion, and non-retryable branches are all testable
/// without provoking a real text-file-busy race.
async fn spawn_with_retry<F>(
    executable: impl std::fmt::Display,
    mut spawn: F,
) -> tokio::process::Child
where
    F: FnMut() -> std::io::Result<tokio::process::Child>,
{
    for _ in 0..MAX_SPAWN_ATTEMPTS {
        match spawn() {
            Ok(child) => return child,
            Err(error) if spawn_error_is_retryable(&error) => {
                tokio::time::sleep(SPAWN_RETRY_DELAY).await;
            }
            Err(error) => panic!("`{executable} --version` could not be spawned: {error}"),
        }
    }
    panic!("`{executable} --version` stayed text-file-busy across retries")
}

/// A freshly written fixture executable can transiently return `ETXTBSY`; every
/// other spawn error is a real failure that must surface immediately.
fn spawn_error_is_retryable(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(26)
}

/// A `--version` banner is a line of tens of bytes; anything past this bound
/// is not a version and is never buffered.
const MAX_PROBE_STDOUT_BYTES: usize = 4096;

/// Awaits the probe under a timeout with its stdout read only up to
/// `MAX_PROBE_STDOUT_BYTES`; a hung executable or one flooding stdout fails
/// the gate promptly (killing the probe's process group) instead of holding
/// the smoke slot or buffering an unbounded stream into memory. Factored so a
/// short bound can be injected in tests.
async fn probe_output_bounded(
    child: tokio::process::Child,
    executable: &std::path::Path,
    bound: Duration,
) -> std::process::Output {
    command_output_bounded(
        child,
        executable,
        "--version",
        bound,
        MAX_PROBE_STDOUT_BYTES,
        "version banner",
    )
    .await
}

/// Shared bounded command collection for pre-spend CLI inspection. It owns
/// the child and kills the whole process group on timeout, read failure, or
/// overflow, matching the version probe's cleanup contract.
async fn command_output_bounded(
    mut child: tokio::process::Child,
    executable: &std::path::Path,
    invocation: &str,
    bound: Duration,
    stdout_limit: usize,
    output_name: &str,
) -> std::process::Output {
    let group = child.id();
    match tokio::time::timeout(
        bound,
        bounded_command_output(&mut child, stdout_limit, output_name),
    )
    .await
    {
        Ok(Ok(output)) => output,
        Ok(Err(failure)) => {
            // The child may still be producing when reading fails or
            // overflows; kill its whole group before failing the gate so a
            // flooding launcher (or its native subprocess) cannot linger.
            kill_probe_group(group);
            panic!("`{} {invocation}` {failure}", executable.display());
        }
        Err(_) => {
            // The timed-out future drops the child, whose kill-on-drop kills
            // and reaps the direct launcher; additionally signal its whole
            // process group so a native subprocess of a Node launcher cannot
            // survive the panic.
            kill_probe_group(group);
            panic!(
                "`{} {invocation}` did not exit within {bound:?}",
                executable.display()
            );
        }
    }
}

/// Reads the child's stdout to the supplied byte bound, then awaits its
/// exit. A producer that exceeds the bound is reported without buffering the
/// remainder — `wait_with_output` would collect the entire stream, letting a
/// fast producer consume unbounded memory before the timeout fires. Closing
/// the taken stdout handle after the bounded read denies an over-producing
/// child anywhere to write (a pipe with no reader), so it cannot grow the
/// buffer past the bound while the exit wait runs.
async fn bounded_command_output(
    child: &mut tokio::process::Child,
    stdout_limit: usize,
    output_name: &str,
) -> Result<std::process::Output, String> {
    use tokio::io::AsyncReadExt;

    let mut stdout = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        let mut buffer = vec![0_u8; stdout_limit + 1];
        let mut filled = 0_usize;
        while filled < buffer.len() {
            match pipe.read(&mut buffer[filled..]).await {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) => return Err(format!("stdout could not be read: {error}")),
            }
        }
        if filled > stdout_limit {
            return Err(format!(
                "printed more than {stdout_limit} bytes; refusing to buffer an \
                 unbounded {output_name}"
            ));
        }
        buffer.truncate(filled);
        stdout = buffer;
    }
    let status = child
        .wait()
        .await
        .map_err(|error| format!("could not be awaited: {error}"))?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}

/// SIGKILLs the probe's process group so a launcher's native subprocess is
/// terminated with it. Harmless when the child is not its own group leader
/// (the signal target group does not exist).
#[cfg(unix)]
fn kill_probe_group(group: Option<u32>) {
    if let Some(raw) = group
        && let Some(pid) = rustix::process::Pid::from_raw(raw as i32)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

#[cfg(not(unix))]
fn kill_probe_group(_group: Option<u32>) {}

/// Fails closed: an unreadable, unparsable, or mismatched version is a smoke
/// failure, never a skip. A skip here would quietly retire the only check that
/// binds this evidence to a specific executable.
async fn assert_pinned_version(executable: &std::path::Path) {
    // A hung or slow probe must not hold the sole gated-smoke concurrency slot
    // until the job timeout; it fails the version gate promptly instead.
    const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

    let mut command = tokio::process::Command::new(executable);
    command.arg("--version").env_clear();
    // The probe clears the parent environment so unrelated shell credentials
    // never reach the external CLI, mirroring the adapter spawn; only `PATH`
    // is restored, the minimum a bare `--version` launch needs.
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Discarded rather than reported: a version probe has no need to
        // surface provider-controlled diagnostics, and this text does not pass
        // through the adapter's redaction.
        .stderr(Stdio::null())
        // Dropping the timed-out future kill-on-drops the direct launcher;
        // its own process group lets the timeout path signal a native
        // subprocess too, so an unbounded probe cannot linger.
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let child = spawn_probe(&mut command, executable).await;
    let output = probe_output_bounded(child, executable, PROBE_TIMEOUT).await;
    assert!(
        output.status.success(),
        "`{} --version` exited with {}",
        executable.display(),
        output.status
    );

    let reported = String::from_utf8(output.stdout).unwrap_or_else(|_| {
        panic!(
            "`{} --version` printed non-UTF-8 output",
            executable.display()
        )
    });
    let version = reported
        .lines()
        .next()
        .and_then(|line| {
            line.split_whitespace()
                .find_map(|token| semver::Version::parse(token).ok())
        })
        .unwrap_or_else(|| {
            panic!(
                "`{} --version` printed no version token",
                executable.display()
            )
        });

    assert_eq!(
        version,
        semver::Version::parse(SUPPORTED_CODEX_CLI_VERSION)
            .expect("the build-validated pin is SemVer"),
        "the executable at `{}` reports {version}, but this smoke can \
         only produce compatibility evidence for the pinned \
         {SUPPORTED_CODEX_CLI_VERSION}; install the version pinned in \
         tooling/codex-cli/package.json",
        executable.display()
    );
}

/// Fails before spend when the pinned CLI's complete built-in feature registry
/// differs from the reviewed classification. A fresh home keeps this command
/// independent of — and unable to read — any ambient CLI login or config.
async fn assert_pinned_feature_inventory(executable: &std::path::Path) {
    const FEATURE_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
    const MAX_FEATURE_STDOUT_BYTES: usize = 16 * 1024;

    assert_complete_feature_classification();
    let isolated_home = tempfile::tempdir().expect("feature probe home is created");
    let mut command = tokio::process::Command::new(executable);
    command.args(["features", "list"]).env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .env("HOME", isolated_home.path())
        .env("CODEX_HOME", isolated_home.path())
        .env("XDG_CONFIG_HOME", isolated_home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let child = spawn_probe(&mut command, executable).await;
    let output = command_output_bounded(
        child,
        executable,
        "features list",
        FEATURE_PROBE_TIMEOUT,
        MAX_FEATURE_STDOUT_BYTES,
        "feature inventory",
    )
    .await;
    assert!(
        output.status.success(),
        "`{} features list` exited with {}",
        executable.display(),
        output.status
    );
    let reported = String::from_utf8(output.stdout).unwrap_or_else(|_| {
        panic!(
            "`{} features list` printed non-UTF-8 output",
            executable.display()
        )
    });
    assert_eq!(
        reported, PINNED_CODEX_FEATURE_INVENTORY,
        "the pinned CLI feature inventory changed; classify every added or \
         changed entry before moving the supported-version contract"
    );
}

#[derive(Clone, Copy)]
enum SkillInstructionDisposition {
    InjectedBaseline,
    HardDisabled,
}

/// Fails before spend unless the pinned CLI both discovers the isolated synthetic
/// ambient skill under its ordinary home and removes all model-visible skill
/// instructions under the exact config value the production invocation passes.
///
/// What the pinned CLI injects is a *catalog*: the block states its own contract
/// — each entry carries a name, a description, and a source locator — and a
/// skill's body is read on demand when the skill is used, never up front. Both
/// halves of that are pinned here. Requiring the body in the baseline would fail
/// against every release that behaves this way, and would also make the
/// disabled-side body clause vacuously true, retiring a third of this control
/// without saying so.
async fn assert_ambient_skill_instructions_disabled(executable: &std::path::Path) {
    let isolated_home = tempfile::tempdir().expect("skill probe home is created");
    let skill_directory = isolated_home
        .path()
        .join("skills")
        .join(SYNTHETIC_SKILL_NAME);
    std::fs::create_dir_all(&skill_directory).expect("synthetic skill directory is created");
    std::fs::write(skill_directory.join("SKILL.md"), synthetic_skill_file())
        .expect("synthetic skill is written");

    let baseline = skill_prompt_input(
        executable,
        isolated_home.path(),
        SkillInstructionDisposition::InjectedBaseline,
    )
    .await;
    let disabled = skill_prompt_input(
        executable,
        isolated_home.path(),
        SkillInstructionDisposition::HardDisabled,
    )
    .await;

    assert!(
        baseline.contains(SKILL_INSTRUCTIONS_BLOCK),
        "the pinned CLI injected no `{SKILL_INSTRUCTIONS_BLOCK}` block, so this \
         probe's baseline no longer establishes that ambient skills reach the model"
    );
    assert!(
        baseline.contains(SYNTHETIC_SKILL_NAME),
        "the pinned CLI did not catalog the isolated synthetic control skill by name"
    );
    assert!(
        baseline.contains(SYNTHETIC_SKILL_DESCRIPTION),
        "the pinned CLI did not catalog the isolated synthetic control skill's description"
    );
    // Positive half of the catalog contract: the entry is injected and the
    // skill's contents are not. A release that started injecting bodies would
    // widen the ambient instruction surface this probe exists to bound, so it
    // fails the gate rather than passing unnoticed.
    assert!(
        !baseline.contains(SYNTHETIC_SKILL_BODY),
        "the pinned CLI injected the synthetic control skill's body, not just its \
         catalog entry; the ambient instruction surface is wider than this probe assumes"
    );
    assert!(
        !disabled.contains(SKILL_INSTRUCTIONS_BLOCK),
        "`{SKILL_INSTRUCTIONS_CONFIG}` left the `{SKILL_INSTRUCTIONS_BLOCK}` block \
         in model-visible input"
    );
    assert!(
        !disabled.contains(SYNTHETIC_SKILL_NAME),
        "`{SKILL_INSTRUCTIONS_CONFIG}` left the ambient skill's name in model-visible input"
    );
    assert!(
        !disabled.contains(SYNTHETIC_SKILL_DESCRIPTION),
        "`{SKILL_INSTRUCTIONS_CONFIG}` left the ambient skill's description in \
         model-visible input"
    );
    // No body clause here: the baseline proves the body is absent in *both*
    // dispositions, so asserting its absence again could not fail.
}

/// Captures only the bounded JSON prompt description from an isolated synthetic
/// home; stderr is discarded and no ambient login or configuration is reachable.
async fn skill_prompt_input(
    executable: &std::path::Path,
    isolated_home: &std::path::Path,
    disposition: SkillInstructionDisposition,
) -> String {
    const PROMPT_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
    const MAX_PROMPT_INPUT_STDOUT_BYTES: usize = 1024 * 1024;

    let mut command = tokio::process::Command::new(executable);
    command.args(["debug", "prompt-input"]);
    match disposition {
        SkillInstructionDisposition::InjectedBaseline => {}
        SkillInstructionDisposition::HardDisabled => {
            command.args(["--config", SKILL_INSTRUCTIONS_CONFIG]);
        }
    }
    command.arg("synthetic prompt input probe").env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .env("HOME", isolated_home)
        .env("CODEX_HOME", isolated_home)
        .env("XDG_CONFIG_HOME", isolated_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let child = spawn_probe(&mut command, executable).await;
    let output = command_output_bounded(
        child,
        executable,
        "debug prompt-input",
        PROMPT_PROBE_TIMEOUT,
        MAX_PROMPT_INPUT_STDOUT_BYTES,
        "prompt input",
    )
    .await;
    assert!(
        output.status.success(),
        "`{} debug prompt-input` exited with {}",
        executable.display(),
        output.status
    );
    String::from_utf8(output.stdout).unwrap_or_else(|_| {
        panic!(
            "`{} debug prompt-input` printed non-UTF-8 output",
            executable.display()
        )
    })
}

/// Proves that every pinned entry has exactly one disposition and that every
/// capability disposition is the feature the production invocation disables.
fn assert_complete_feature_classification() {
    use std::collections::BTreeSet;

    let inventory: BTreeSet<&str> = PINNED_CODEX_FEATURE_INVENTORY
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    let disabled: BTreeSet<&str> = DISABLED_CODEX_CLI_CAPABILITY_FEATURES
        .iter()
        .copied()
        .collect();
    let non_capability: BTreeSet<&str> = NON_CAPABILITY_CODEX_FEATURES.iter().copied().collect();
    assert_eq!(
        inventory.len(),
        PINNED_CODEX_FEATURE_INVENTORY.lines().count(),
        "the pinned inventory repeats a feature name"
    );
    assert_eq!(
        disabled.len(),
        DISABLED_CODEX_CLI_CAPABILITY_FEATURES.len(),
        "the production hard-disable list repeats a feature name"
    );
    assert_eq!(
        non_capability.len(),
        NON_CAPABILITY_CODEX_FEATURES.len(),
        "the non-capability classification repeats a feature name"
    );
    assert!(
        disabled.is_disjoint(&non_capability),
        "a pinned feature has two classifications"
    );
    let classified: BTreeSet<&str> = disabled.union(&non_capability).copied().collect();
    assert_eq!(
        classified, inventory,
        "every pinned feature must be classified exactly once"
    );
}

#[test]
fn pinned_feature_classification_matches_the_runtime_hard_disables() {
    assert_complete_feature_classification();
}

/// The credentialed login is independently bounded and removes the secret
/// from the timeout and CLI environments; the ignored model exchange and the
/// job-level timeout cannot substitute for this local cleanup boundary.
#[test]
fn compatibility_smoke_login_has_a_process_group_timeout() {
    assert!(
        CODEX_SMOKE_WORKFLOW.contains(LOGIN_TIMEOUT_INVOCATION),
        "the smoke login no longer carries the thirty-second TERM and five-second KILL bound"
    );
    assert!(
        CODEX_SMOKE_WORKFLOW.contains("| env -u CODEX_SMOKE_API_KEY timeout"),
        "the smoke login no longer removes the key from the timeout environment"
    );
    assert!(
        CODEX_SMOKE_WORKFLOW.contains(LOGIN_TERM_HOLD_COMMAND),
        "the timeout command no longer keeps its managed supervisor alive through the KILL grace"
    );
}

/// If the direct login launcher exits on TERM while its native descendant ignores
/// TERM, the managed shell stays alive until GNU timeout sends KILL to the
/// entire command group. Without that hold-open trap, timeout cancels its KILL
/// timer as soon as the launcher exits and strands the recorded descendant.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn login_timeout_kills_descendant_when_launcher_exits_on_term() {
    let directory = tempfile::tempdir().expect("login-timeout fixture directory is created");
    let pid_file = directory.path().join("descendant-pid");
    let launcher = version_probe_fixture(directory.path(), LOGIN_TIMEOUT_DESCENDANT_FIXTURE);
    let mut command = tokio::process::Command::new("timeout");
    command
        .arg("--signal=TERM")
        .arg("--kill-after=1s")
        .arg("0.5s")
        .arg("sh")
        .arg("-c")
        .arg(LOGIN_TERM_HOLD_COMMAND)
        .arg("sh")
        .arg("sh")
        .arg(launcher)
        .arg(&pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let status = command
        .status()
        .await
        .expect("the bounded synthetic login launcher runs");
    let descendant = read_recorded_descendant(&pid_file).await;

    assert!(!status.success(), "the synthetic login must time out");
    assert_process_exits(descendant).await;
}

/// Writes an executable version-probe script and returns its path.
#[cfg(unix)]
fn version_probe_fixture(directory: &std::path::Path, script: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = directory.join("codex-version-probe");
    std::fs::write(&path, script).expect("the version-probe script is written");
    let mut permissions = std::fs::metadata(&path)
        .expect("the version-probe script has metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&path, permissions).expect("the version-probe script is runnable");
    path
}

#[test]
fn spawn_error_etxtbsy_is_retryable() {
    assert!(spawn_error_is_retryable(
        &std::io::Error::from_raw_os_error(26)
    ));
}

#[test]
fn spawn_error_enoent_is_not_retryable() {
    assert!(!spawn_error_is_retryable(
        &std::io::Error::from_raw_os_error(2)
    ));
}

/// Builds a spawn returning `ETXTBSY` for its first `failures` calls, then a
/// trivially-succeeding child, keeping the branching out of the test body.
fn busy_then_success(failures: usize) -> impl FnMut() -> std::io::Result<tokio::process::Child> {
    let mut remaining = failures;
    move || match remaining {
        0 => trivial_success_command().spawn(),
        _ => {
            remaining -= 1;
            Err(std::io::Error::from_raw_os_error(26))
        }
    }
}

/// A no-op command that exits successfully on the running platform, so the
/// retry-loop tests do not depend on the Unix-only `true` executable and stay
/// runnable on the ordinary Windows workspace suite.
fn trivial_success_command() -> tokio::process::Command {
    #[cfg(windows)]
    {
        let mut command = tokio::process::Command::new("cmd");
        command.args(["/C", "exit", "0"]);
        command
    }
    #[cfg(not(windows))]
    {
        tokio::process::Command::new("true")
    }
}

/// The scripted retry fixture returns exactly the requested run of `ETXTBSY`
/// failures before succeeding.
#[tokio::test]
async fn busy_then_success_fails_the_requested_count_then_spawns() {
    let mut spawn = busy_then_success(2);

    assert_eq!(spawn().unwrap_err().raw_os_error(), Some(26));
    assert_eq!(spawn().unwrap_err().raw_os_error(), Some(26));
    let child = spawn().expect("the third attempt spawns");
    let status = child
        .wait_with_output()
        .await
        .expect("the spawned child is awaited");
    assert!(status.status.success());
}

/// A hung executable trips the probe timeout and fails the gate promptly; the
/// kill-on-drop child is terminated when the timed-out future is dropped.
#[cfg(unix)]
#[tokio::test]
#[should_panic(expected = "did not exit within")]
async fn version_probe_times_out_on_a_hanging_child() {
    let mut command = tokio::process::Command::new("sleep");
    command
        .arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let child = command.spawn().expect("the hanging child spawns");

    let _ = probe_output_bounded(
        child,
        std::path::Path::new("hanging-probe"),
        Duration::from_millis(50),
    )
    .await;
}

/// The timeout path kills the probe's whole process group, not just the
/// direct launcher: a launcher that spawned a native descendant (as the Node
/// CLI shim does) leaves that descendant in the group, and only the group
/// signal reaches it — kill-on-drop alone would strand it. The descendant's
/// pid is recorded by the launcher and observed to die after the timeout
/// panics, so `kill_probe_group` regressing to a no-op fails this test.
#[cfg(unix)]
#[tokio::test]
async fn version_probe_timeout_kills_probe_descendants() {
    let directory = tempfile::tempdir().expect("descendant fixture directory is created");
    let pid_file = directory.path().join("descendant-pid");
    let script = format!(
        "#!/bin/sh\nsleep 60 &\nprintf '%s\\n' \"$!\" > '{}'\nwait\n",
        pid_file.display()
    );
    let executable = version_probe_fixture(directory.path(), &script);
    let mut command = tokio::process::Command::new(&executable);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    // Route through the shared retry: a freshly written fixture can be
    // transiently `ETXTBSY` while a concurrent test thread holds an inherited
    // write descriptor across its fork/exec, so a direct spawn would flake.
    let child = spawn_probe(&mut command, &executable).await;
    let descendant = read_recorded_descendant(&pid_file).await;

    let probe = tokio::spawn(probe_output_bounded_owned(
        child,
        std::path::PathBuf::from("descendant-probe"),
        Duration::from_millis(50),
    ))
    .await;

    let panic_message = probe
        .expect_err("the timed-out probe panics")
        .into_panic()
        .downcast::<String>()
        .expect("the probe panic carries its message");
    assert!(panic_message.contains("did not exit within"));
    assert_process_exits(descendant).await;
}

/// `probe_output_bounded` with owned arguments, so the probe future can be
/// spawned as a task whose panic is observed rather than aborting the test.
#[cfg(unix)]
async fn probe_output_bounded_owned(
    child: tokio::process::Child,
    executable: std::path::PathBuf,
    bound: Duration,
) -> std::process::Output {
    probe_output_bounded(child, &executable, bound).await
}

/// Polls the launcher's pid record until it is written, bounded.
#[cfg(unix)]
async fn read_recorded_descendant(pid_file: &std::path::Path) -> rustix::process::Pid {
    const RECORD_TIMEOUT: Duration = Duration::from_secs(5);

    let deadline = std::time::Instant::now() + RECORD_TIMEOUT;
    loop {
        if let Ok(record) = std::fs::read_to_string(pid_file)
            && let Ok(raw) = record.trim().parse::<i32>()
        {
            return rustix::process::Pid::from_raw(raw).expect("the recorded pid is nonzero");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the launcher never recorded its descendant pid"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Asserts the process exits within a bounded observation window. A killed
/// descendant can linger as an unreaped zombie in containers whose PID 1 does
/// not promptly reap orphans, and `test_kill_process` keeps succeeding for that
/// zombie; treat a zombie (via `/proc` where available) as exited so cleanup is
/// not falsely reported as a live process.
#[cfg(unix)]
async fn assert_process_exits(pid: rustix::process::Pid) {
    const PROCESS_EXIT_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(5);

    let deadline = std::time::Instant::now() + PROCESS_EXIT_OBSERVATION_TIMEOUT;
    while process_is_live(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "the probe descendant remains alive after the timeout cleanup"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Whether `pid` names a still-running process, taking the two operating-system
/// observations the classification needs and handing them to
/// [`classify_process_liveness`], so every arm is pinned by a focused test
/// rather than only by whichever outcome the host running the descendant
/// integration test happens to produce.
#[cfg(unix)]
fn process_is_live(pid: rustix::process::Pid) -> bool {
    let signalable = rustix::process::test_kill_process(pid).is_ok();
    // Read the stat line only for a pid still worth classifying; an
    // unsignalable one is already gone.
    let proc_stat = signalable.then(|| proc_stat_line(pid)).flatten();
    classify_process_liveness(signalable, proc_stat.as_deref())
}

/// The process's `/proc/<pid>/stat` line, or `None` where `/proc` is
/// unavailable (macOS) or the entry vanished between the two observations.
#[cfg(unix)]
fn proc_stat_line(pid: rustix::process::Pid) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{}/stat", pid.as_raw_nonzero().get())).ok()
}

/// Classifies liveness from those observations. An unsignalable pid is gone. A
/// signalable pid that `/proc` reports as a zombie has already exited — its slot
/// only awaits reaping, and `test_kill_process` keeps succeeding for it — so
/// treating it as live would make cleanup time out on a dead descendant. Where
/// `/proc` is unavailable (macOS), a signalable pid is treated as live, the best
/// signal available.
#[cfg(unix)]
fn classify_process_liveness(signalable: bool, proc_stat: Option<&str>) -> bool {
    if !signalable {
        return false;
    }
    match proc_stat {
        Some(stat) => !proc_stat_is_zombie(stat),
        None => true,
    }
}

/// An unsignalable pid names no process, so it is not live.
#[cfg(unix)]
#[test]
fn unsignalable_process_is_not_live() {
    assert!(!classify_process_liveness(false, None));
}

/// Signalability decides first: a stale stat line for a pid that can no longer
/// be signalled cannot revive it.
#[cfg(unix)]
#[test]
fn unsignalable_process_with_a_running_stat_line_is_not_live() {
    assert!(!classify_process_liveness(
        false,
        Some("4321 (codex) R 1 4321 4321 0 -1")
    ));
}

/// Where `/proc` is unavailable, a signalable pid is live — the fallback that
/// keeps the macOS host from reporting every descendant as already exited.
#[cfg(unix)]
#[test]
fn signalable_process_without_proc_is_live() {
    assert!(classify_process_liveness(true, None));
}

/// A signalable pid whose stat line reports a zombie has exited; reporting it
/// live would make the cleanup observation time out on a dead descendant.
#[cfg(unix)]
#[test]
fn signalable_zombie_is_not_live() {
    assert!(!classify_process_liveness(
        true,
        Some("4321 (codex) Z 1 4321 4321 0 -1")
    ));
}

/// A signalable pid whose stat line reports a running process is live;
/// reporting it exited would let cleanup pass with a surviving descendant.
#[cfg(unix)]
#[test]
fn signalable_running_process_is_live() {
    assert!(classify_process_liveness(
        true,
        Some("4321 (codex) R 1 4321 4321 0 -1")
    ));
}

/// A `/proc/<pid>/stat` line reports a zombie as state `Z` in the field after
/// the parenthesized comm (which itself may contain spaces or `)`), so the
/// state is read after the last `") "`.
#[cfg(unix)]
fn proc_stat_is_zombie(stat: &str) -> bool {
    stat.rsplit_once(") ")
        .is_some_and(|(_, fields)| fields.starts_with("Z "))
}

/// The zombie state `Z` after a plain comm is detected.
#[cfg(unix)]
#[test]
fn proc_stat_detects_a_zombie() {
    assert!(proc_stat_is_zombie("4321 (codex) Z 1 4321 4321 0 -1"));
}

/// A running (`R`) process is not a zombie.
#[cfg(unix)]
#[test]
fn proc_stat_running_process_is_not_a_zombie() {
    assert!(!proc_stat_is_zombie("4321 (codex) R 1 4321 4321 0 -1"));
}

/// A comm containing `) ` (the exact split token) does not fool the parser: the
/// state is read after the *last* `") "`, so a zombie is still detected.
#[cfg(unix)]
#[test]
fn proc_stat_detects_a_zombie_with_embedded_paren_in_comm() {
    assert!(proc_stat_is_zombie("4321 (od) d ) name) Z 1 4321 4321"));
}

/// The same embedded-`) ` comm on a running process is still not a zombie.
#[cfg(unix)]
#[test]
fn proc_stat_embedded_paren_running_is_not_a_zombie() {
    assert!(!proc_stat_is_zombie("4321 (od) d ) name) S 1 4321 4321"));
}

/// A malformed line without the `") "` boundary is treated as not-a-zombie
/// (the caller then falls back to the signalable-is-live check).
#[cfg(unix)]
#[test]
fn proc_stat_without_boundary_is_not_a_zombie() {
    assert!(!proc_stat_is_zombie("garbage-without-the-boundary"));
}

/// An *unset* `PATH` cannot locate a bare command and fails, rather than being
/// silently searched in the current directory as `unwrap_or_default` would.
#[test]
#[should_panic(expected = "PATH is unset")]
fn bare_command_with_unset_path_fails() {
    let directory = tempfile::tempdir().expect("fixture directory is created");
    let _ = resolved_executable(std::ffi::OsStr::new("codex"), directory.path(), None);
}

/// A probe that floods stdout is stopped at the byte bound and fails the gate
/// with the overflow diagnostic — not buffered whole until the timeout, which
/// let a fast producer consume unbounded memory before failing.
#[cfg(unix)]
#[tokio::test]
async fn version_probe_overflow_kills_flooding_probe_descendants() {
    let directory = tempfile::tempdir().expect("flood fixture directory is created");
    let pid_file = directory.path().join("descendant-pid");
    let script = format!(
        "#!/bin/sh\nsleep 60 &\nprintf '%s\\n' \"$!\" > '{}'\nexec yes codex-flood\n",
        pid_file.display()
    );
    let executable = version_probe_fixture(directory.path(), &script);
    let mut command = tokio::process::Command::new(&executable);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    // Shared retry, as above: tolerate a transient `ETXTBSY` on the
    // freshly written fixture under parallel test execution.
    let child = spawn_probe(&mut command, &executable).await;
    let descendant = read_recorded_descendant(&pid_file).await;

    let probe = tokio::spawn(probe_output_bounded_owned(
        child,
        std::path::PathBuf::from("flooding-probe"),
        Duration::from_secs(30),
    ))
    .await;

    let panic_message = probe
        .expect_err("the overflowing probe panics")
        .into_panic()
        .downcast::<String>()
        .expect("the probe panic carries its message");
    assert!(panic_message.contains("printed more than"));
    assert_process_exits(descendant).await;
}

/// The retry loop keeps trying through transient `ETXTBSY` and returns the
/// child once a later attempt succeeds.
#[tokio::test]
async fn spawn_with_retry_succeeds_after_transient_busy() {
    let child = spawn_with_retry("fixture", busy_then_success(2)).await;

    let status = child
        .wait_with_output()
        .await
        .expect("the child is awaited");
    assert!(status.status.success());
}

/// Persistent `ETXTBSY` exhausts the bound and fails rather than looping.
#[tokio::test]
#[should_panic(expected = "stayed text-file-busy")]
async fn spawn_with_retry_gives_up_when_always_busy() {
    let _ = spawn_with_retry("fixture", || Err(std::io::Error::from_raw_os_error(26))).await;
}

/// A non-`ETXTBSY` spawn error is not masked by the retry.
#[tokio::test]
#[should_panic(expected = "could not be spawned")]
async fn spawn_with_retry_surfaces_a_non_busy_error() {
    let _ = spawn_with_retry("fixture", || Err(std::io::Error::from_raw_os_error(2))).await;
}

/// The gate accepts an executable that reports exactly the pinned version.
#[cfg(unix)]
#[tokio::test]
async fn version_gate_accepts_the_pinned_version() {
    let directory = tempfile::tempdir().expect("version fixture directory is created");
    let script = format!("#!/bin/sh\nprintf 'codex-cli %s\\n' '{SUPPORTED_CODEX_CLI_VERSION}'\n");
    let executable = version_probe_fixture(directory.path(), &script);

    assert_pinned_version(&executable).await;
}

#[cfg(unix)]
#[tokio::test]
#[should_panic(expected = "only produce compatibility evidence")]
async fn version_gate_rejects_a_mismatched_version() {
    let directory = tempfile::tempdir().expect("version fixture directory is created");
    let executable = version_probe_fixture(
        directory.path(),
        "#!/bin/sh\nprintf 'codex-cli %s\\n' '0.0.1-drifted'\n",
    );

    assert_pinned_version(&executable).await;
}

#[cfg(unix)]
#[tokio::test]
#[should_panic(expected = "printed no version token")]
async fn version_gate_rejects_empty_version_output() {
    let directory = tempfile::tempdir().expect("version fixture directory is created");
    let executable = version_probe_fixture(directory.path(), "#!/bin/sh\n");

    assert_pinned_version(&executable).await;
}

#[cfg(unix)]
#[tokio::test]
#[should_panic(expected = "printed non-UTF-8 output")]
async fn version_gate_rejects_non_utf8_version_output() {
    let directory = tempfile::tempdir().expect("version fixture directory is created");
    let executable =
        version_probe_fixture(directory.path(), "#!/bin/sh\nprintf '\\377\\376\\375'\n");

    assert_pinned_version(&executable).await;
}

#[cfg(unix)]
#[tokio::test]
#[should_panic(expected = "exited with")]
async fn version_gate_rejects_a_failing_probe() {
    let directory = tempfile::tempdir().expect("version fixture directory is created");
    let executable = version_probe_fixture(directory.path(), "#!/bin/sh\nexit 3\n");

    assert_pinned_version(&executable).await;
}

/// The projection returns the capability for a prepared outcome; preparation
/// is offline here — validation, translation, and request construction only,
/// with no process spawn.
// Gated to the exact hosts the runtime supports: this exercises the
// preparation projection through a real `CodexCliRuntime`, whose construction
// rejects platforms without process-group supervision
// (`CodexCliConstructionError::UnsupportedPlatform`, confirmed by
// `unsupported_platform_is_rejected_at_construction`). The gate mirrors
// `PROCESS_GROUP_SUPERVISION_SUPPORTED` — `#[cfg(unix)]` alone is broader and
// would still panic at construction on Unix targets the runtime rejects
// (OpenBSD, Redox, …), so the case is skipped there rather than failing.
#[cfg(all(
    unix,
    not(any(
        target_os = "cygwin",
        target_os = "horizon",
        target_os = "openbsd",
        target_os = "redox",
        target_os = "wasi"
    ))
))]
#[tokio::test]
async fn preparation_projection_returns_a_prepared_capability() {
    let working_directory = tempfile::tempdir().expect("smoke working directory is created");
    let credential_reference = CredentialReference::new("codex-smoke");
    let config = CodexCliConfig::new(
        working_directory.path().join("codex-fixture"),
        working_directory.path(),
        credential_reference.clone(),
        None,
    );
    let runtime = CodexCliRuntime::new(config).expect("smoke runtime configuration is valid");
    let operation = ModelOperation::new(
        "codex-smoke".to_string(),
        credential_reference,
        RequestedTarget::new(DEFAULT_MODEL),
        ResolvedTarget::new(DEFAULT_MODEL),
        vec![ConversationMessage::user_text(PROMPT)],
        ModelSettings::new(64),
    );

    let _prepared = require_prepared(
        runtime
            .prepare(operation, CancellationSignal::never())
            .await,
    );
}

#[test]
#[should_panic(expected = "smoke preparation was not cancelled")]
fn preparation_projection_rejects_a_cancelled_outcome() {
    let _ = require_prepared(PreparationOutcome::Cancelled {
        correlation: "codex-smoke".to_string(),
    });
}

#[test]
#[should_panic(expected = "smoke preparation failed")]
fn preparation_projection_rejects_a_failed_outcome() {
    let _ = require_prepared(PreparationOutcome::Failed {
        correlation: "codex-smoke".to_string(),
        failure: PreparationFailure::UnsupportedOperation {
            detail: "fixture failure".to_string(),
        },
    });
}

#[test]
#[should_panic(expected = "smoke preparation found a defect")]
fn preparation_projection_rejects_a_defect_outcome() {
    let _ = require_prepared(PreparationOutcome::Defect {
        correlation: "codex-smoke".to_string(),
        defect: PreparationDefect::RequestConstructionFailed {
            detail: "fixture defect".to_string(),
        },
    });
}

#[test]
fn decoded_response_accepts_completion() {
    let exchange = ExchangeFacts {
        provider_request_id: Some(ProviderRequestId::new(FIXTURE_THREAD_ID)),
        http_status: None,
        retry_after: None,
    };
    let usage = TokenUsage {
        input_tokens: Some(FIXTURE_INPUT_TOKENS),
        output_tokens: Some(FIXTURE_OUTPUT_TOKENS),
        ..TokenUsage::default()
    };
    let evidence = TerminalEvidence::Completed(CompletionEvidence {
        exchange: exchange.clone(),
        message_id: None,
        reported_model: None,
        finish: CompletionFinish::EndTurn,
        content: Vec::new(),
        usage,
    });

    let decoded = require_decoded_response(evidence);
    assert_eq!(decoded.exchange, exchange);
    assert_eq!(decoded.usage, usage);
}

#[test]
fn decoded_response_accepts_refusal_without_completion_material() {
    let exchange = ExchangeFacts {
        provider_request_id: Some(ProviderRequestId::new(FIXTURE_THREAD_ID)),
        http_status: None,
        retry_after: None,
    };
    let usage = TokenUsage {
        input_tokens: Some(FIXTURE_INPUT_TOKENS),
        output_tokens: Some(FIXTURE_OUTPUT_TOKENS),
        ..TokenUsage::default()
    };
    let evidence = TerminalEvidence::Refused(RefusalEvidence {
        exchange: exchange.clone(),
        message_id: None,
        reported_model: None,
        content: Vec::new(),
        usage,
    });

    let decoded = require_decoded_response(evidence);
    assert_eq!(decoded.exchange, exchange);
    assert_eq!(decoded.usage, usage);
}

/// The smoke accepts only a decoded response: every other terminal variant —
/// cancellation, failure, defect, boundary loss — is rejected rather than
/// reported as a successful exchange, so a compatibility break cannot pass the
/// gate as a completed turn.
#[test]
#[should_panic(expected = "the pinned Codex CLI returned no decoded response")]
fn decoded_response_rejects_an_unexpected_terminal_variant() {
    let evidence = TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
        cause: LossCause::ResponseUnintelligible {
            detail: "fixture terminal variant".to_string(),
        },
        exchange: ExchangeFacts {
            provider_request_id: Some(ProviderRequestId::new(FIXTURE_THREAD_ID)),
            http_status: None,
            retry_after: None,
        },
        reported_model: None,
        finish_reported: None,
        tool_calls: ToolCallsAtLoss::Unobserved,
        usage: TokenUsage {
            input_tokens: Some(FIXTURE_INPUT_TOKENS),
            output_tokens: Some(FIXTURE_OUTPUT_TOKENS),
            ..TokenUsage::default()
        },
    });

    let _ = require_decoded_response(evidence);
}

/// The executable override keeps raw OS bytes: a valid Unix path that is not
/// UTF-8 names the executable the operator asked for, rather than being read as
/// absent and silently resolving the bare-command default.
#[cfg(unix)]
#[test]
fn non_utf8_executable_override_is_resolved_verbatim() {
    use std::os::unix::ffi::OsStringExt;

    let root = tempfile::tempdir().expect("fixture directory is created");
    let executable = root.path().join(std::ffi::OsString::from_vec(vec![
        b'c', b'o', b'd', b'e', b'x', 0xff,
    ]));

    let resolved = resolved_executable(executable.as_os_str(), root.path(), None);

    assert_eq!(resolved, executable);
}

/// Selection preserves a populated override as raw OS bytes before any path
/// resolution, including a valid Unix value that is not UTF-8.
#[cfg(unix)]
#[test]
fn executable_selector_preserves_a_non_utf8_override() {
    use std::os::unix::ffi::OsStringExt;

    let override_value =
        std::ffi::OsString::from_vec(vec![b'c', b'o', b'd', b'e', b'x', b'-', 0xff]);

    assert_eq!(
        selected_executable_or_default(Some(override_value.clone())),
        override_value
    );
}

/// An absent override selects the documented bare-command default.
#[test]
fn executable_selector_defaults_when_override_is_absent() {
    assert_eq!(
        selected_executable_or_default(None),
        std::ffi::OsString::from(DEFAULT_EXECUTABLE)
    );
}

/// An explicitly empty override has the same defaulting semantics as absence.
#[test]
fn executable_selector_defaults_when_override_is_empty() {
    assert_eq!(
        selected_executable_or_default(Some(std::ffi::OsString::new())),
        std::ffi::OsString::from(DEFAULT_EXECUTABLE)
    );
}

fn variable_or(name: &str, fallback: &str) -> String {
    selected_or(std::env::var(name).ok(), fallback)
}

/// Pure selection behind [`variable_or`]: the caller supplies the variable's
/// value, so every branch is testable without mutating process-global state.
/// An unset or non-Unicode variable arrives as `None` and falls back exactly
/// as a blank one does.
fn selected_or(value: Option<String>, fallback: &str) -> String {
    match value {
        Some(value) if !value.trim().is_empty() => value,
        _ => fallback.to_string(),
    }
}

#[test]
fn smoke_variable_selection_prefers_a_populated_override() {
    let override_value = "codex-override".to_string();

    assert_eq!(
        selected_or(Some(override_value.clone()), DEFAULT_EXECUTABLE),
        override_value
    );
}

#[test]
fn smoke_variable_selection_falls_back_for_an_absent_variable() {
    assert_eq!(selected_or(None, DEFAULT_MODEL), DEFAULT_MODEL);
}

#[test]
fn smoke_variable_selection_falls_back_for_a_blank_variable() {
    assert_eq!(
        selected_or(Some(String::new()), DEFAULT_MODEL),
        DEFAULT_MODEL
    );
}

#[test]
fn smoke_variable_selection_falls_back_for_a_whitespace_variable() {
    assert_eq!(
        selected_or(Some("   ".to_string()), DEFAULT_EXECUTABLE),
        DEFAULT_EXECUTABLE
    );
}

/// The adapter accepts only an absolute executable path, so the bare-command
/// local default is resolved through `PATH` exactly once, here. CI is
/// unaffected: the workflow always passes the absolute path of the binary
/// installed from the pin manifest.
/// The executable override as raw OS bytes, or the bare-command default. Read
/// with `var_os` rather than `var`: a valid Unix path need not be UTF-8, and
/// `var(..).ok()` turns such a path's `NotUnicode` into absence, silently
/// resolving the default instead of the executable the operator named. The
/// model override stays a `String` — it is protocol text, not a path.
fn executable_override_or_default() -> std::ffi::OsString {
    selected_executable_or_default(std::env::var_os(EXECUTABLE_VARIABLE))
}

/// Pure selection behind [`executable_override_or_default`], keeping raw OS
/// path bytes injectable without mutating process-global environment state.
fn selected_executable_or_default(selected: Option<std::ffi::OsString>) -> std::ffi::OsString {
    match selected {
        Some(value) if !value.is_empty() => value,
        _ => std::ffi::OsString::from(DEFAULT_EXECUTABLE),
    }
}

fn absolute_executable(executable: &std::ffi::OsStr) -> std::path::PathBuf {
    resolved_executable(
        executable,
        &std::env::current_dir().expect("the smoke process has a working directory"),
        std::env::var_os("PATH").as_deref(),
    )
}

/// Pure resolution behind [`absolute_executable`]: the caller supplies the
/// working directory and search path, so every branch is testable without
/// mutating process-global state.
fn resolved_executable(
    executable: &std::ffi::OsStr,
    current_directory: &std::path::Path,
    // `None` distinguishes an *unset* `PATH` from a present-but-empty one: an
    // unset PATH offers no search directories, so a bare command name cannot be
    // located and must fail — not be silently searched in the current directory
    // as `unwrap_or_default()` would. A present-but-empty entry (`Some("")`)
    // keeps its POSIX meaning of the current directory.
    search: Option<&std::ffi::OsStr>,
) -> std::path::PathBuf {
    let path = std::path::Path::new(executable);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    if path.components().count() > 1 {
        return current_directory.join(path);
    }
    let search = search.unwrap_or_else(|| {
        panic!(
            "PATH is unset, so the bare command `{}` cannot be located; \
             set {EXECUTABLE_VARIABLE} to an absolute executable path",
            path.display()
        )
    });
    // The resolved candidate is kept as a `PathBuf`, never lossily converted to
    // a `String`, so a match in a non-UTF-8 `PATH` directory still names the
    // real executable.
    std::env::split_paths(search)
        .map(|directory| {
            // A relative or empty PATH element denotes a directory under the
            // caller's working directory; anchor it so the returned path
            // satisfies the adapter's absolute-executable requirement.
            if directory.is_absolute() {
                directory.join(executable)
            } else {
                current_directory.join(directory).join(executable)
            }
        })
        .find(|candidate| executable_file(candidate))
        .unwrap_or_else(|| {
            panic!(
                "`{}` was not found on PATH; set {EXECUTABLE_VARIABLE} \
                 to an absolute executable path",
                path.display()
            )
        })
}

/// Mirrors shell `PATH` lookup: a regular file the current process cannot
/// execute — checked with effective credentials, not merely any execute bit —
/// is skipped, so a non-executable shadow cannot hide the real executable in
/// a later directory.
#[cfg(unix)]
fn executable_file(candidate: &std::path::Path) -> bool {
    candidate.is_file()
        && rustix::fs::accessat(
            rustix::fs::CWD,
            candidate,
            rustix::fs::Access::EXEC_OK,
            rustix::fs::AtFlags::EACCESS,
        )
        .is_ok()
}

#[cfg(not(unix))]
fn executable_file(candidate: &std::path::Path) -> bool {
    candidate.is_file()
}

#[test]
fn executable_resolution_passes_an_absolute_path_through() {
    let directory = tempfile::tempdir().expect("resolution fixture directory is created");
    let absolute = directory.path().join("codex-absolute");

    let resolved = resolved_executable(
        absolute.as_os_str(),
        directory.path(),
        Some(std::ffi::OsStr::new("")),
    );

    assert_eq!(resolved, absolute);
}

#[test]
fn executable_resolution_anchors_a_relative_path_to_the_working_directory() {
    let directory = tempfile::tempdir().expect("resolution fixture directory is created");
    let relative = "tooling/codex-relative";

    let resolved = resolved_executable(
        std::ffi::OsStr::new(relative),
        directory.path(),
        Some(std::ffi::OsStr::new("")),
    );

    assert_eq!(resolved, directory.path().join(relative));
}

/// Writes an executable fixture file and returns its path.
#[cfg(unix)]
fn executable_fixture(directory: &std::path::Path, name: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = directory.join(name);
    std::fs::write(&path, "#!/bin/sh\n").expect("the executable fixture file is written");
    let mut permissions = std::fs::metadata(&path)
        .expect("the executable fixture file has metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&path, permissions).expect("the executable fixture is made runnable");
    path
}

#[cfg(unix)]
#[test]
fn executable_resolution_finds_a_bare_command_on_the_search_path() {
    let empty = tempfile::tempdir().expect("resolution fixture directory is created");
    let populated = tempfile::tempdir().expect("resolution fixture directory is created");
    let on_path = executable_fixture(populated.path(), "codex-on-path");
    let search = std::env::join_paths([empty.path(), populated.path()])
        .expect("the fixture search path joins");

    let resolved = resolved_executable(
        std::ffi::OsStr::new("codex-on-path"),
        empty.path(),
        Some(&search),
    );

    assert_eq!(resolved, on_path);
}

/// A file whose only execute bit belongs to "other" is not executable by
/// its owner, so it cannot shadow the runnable executable in a later
/// directory, exactly as normal PATH lookup skips it.
#[cfg(unix)]
#[test]
fn executable_resolution_skips_an_other_only_execute_bit() {
    verify_other_only_execute_bit_is_skipped();
}

/// Body of the other-only-execute-bit case. The effective-uid gate lives here
/// rather than in the `#[test]` function so that body stays straight-line: an
/// effective root uid may execute a regular file for any execute bit, so the
/// owner-vs-other distinction the case relies on does not hold and the check
/// returns early, keeping the workspace validation portable in root
/// containers.
#[cfg(unix)]
fn verify_other_only_execute_bit_is_skipped() {
    use std::os::unix::fs::PermissionsExt;

    if rustix::process::geteuid().is_root() {
        return;
    }
    let shadowing = tempfile::tempdir().expect("resolution fixture directory is created");
    let populated = tempfile::tempdir().expect("resolution fixture directory is created");
    let shadow = shadowing.path().join("codex-other-only");
    std::fs::write(&shadow, "#!/bin/sh\n").expect("the other-only shadow file is written");
    let mut permissions = std::fs::metadata(&shadow)
        .expect("the other-only shadow file has metadata")
        .permissions();
    permissions.set_mode(0o004 | 0o001);
    std::fs::set_permissions(&shadow, permissions).expect("the shadow keeps only the other bits");
    let on_path = executable_fixture(populated.path(), "codex-other-only");
    let search = std::env::join_paths([shadowing.path(), populated.path()])
        .expect("the fixture search path joins");

    let resolved = resolved_executable(
        std::ffi::OsStr::new("codex-other-only"),
        shadowing.path(),
        Some(&search),
    );

    assert_eq!(resolved, on_path);
}

/// A match in a `PATH` directory whose name is not valid UTF-8 is returned as
/// the real path, not a lossy conversion that would name a nonexistent file.
/// Gated to Linux, whose filesystems accept arbitrary filename bytes; macOS
/// and Windows reject a non-UTF-8 name at creation.
#[cfg(target_os = "linux")]
#[test]
fn executable_resolution_preserves_a_non_utf8_path_directory() {
    use std::os::unix::ffi::OsStrExt;

    let root = tempfile::tempdir().expect("resolution fixture directory is created");
    let directory = root
        .path()
        .join(std::ffi::OsStr::from_bytes(b"codex-\xff-dir"));
    std::fs::create_dir(&directory).expect("the non-UTF-8 PATH directory is created");
    let on_path = executable_fixture(&directory, "codex-nonutf8");
    let search = std::env::join_paths([&directory]).expect("the fixture search path joins");

    let resolved = resolved_executable(
        std::ffi::OsStr::new("codex-nonutf8"),
        root.path(),
        Some(&search),
    );

    assert_eq!(resolved, on_path);
}

/// A relative PATH element is anchored to the working directory, so the
/// returned candidate satisfies the adapter's absolute-path requirement.
#[cfg(unix)]
#[test]
fn executable_resolution_anchors_a_relative_search_entry() {
    let working = tempfile::tempdir().expect("resolution fixture directory is created");
    std::fs::create_dir(working.path().join("bin")).expect("the relative bin directory is created");
    let on_path = executable_fixture(&working.path().join("bin"), "codex-rel");

    let resolved = resolved_executable(
        std::ffi::OsStr::new("codex-rel"),
        working.path(),
        Some(std::ffi::OsStr::new("bin")),
    );

    assert_eq!(resolved, on_path);
}

/// A regular but non-executable file earlier on the search path cannot shadow
/// the real executable in a later directory, matching shell `PATH` lookup.
#[cfg(unix)]
#[test]
fn executable_resolution_skips_a_non_executable_shadow() {
    let shadowing = tempfile::tempdir().expect("resolution fixture directory is created");
    let populated = tempfile::tempdir().expect("resolution fixture directory is created");
    std::fs::write(shadowing.path().join("codex-shadowed"), "not runnable\n")
        .expect("the non-executable shadow file is written");
    let on_path = executable_fixture(populated.path(), "codex-shadowed");
    let search = std::env::join_paths([shadowing.path(), populated.path()])
        .expect("the fixture search path joins");

    let resolved = resolved_executable(
        std::ffi::OsStr::new("codex-shadowed"),
        shadowing.path(),
        Some(&search),
    );

    assert_eq!(resolved, on_path);
}

#[test]
#[should_panic(expected = "was not found on PATH")]
fn executable_resolution_panics_for_a_missing_bare_command() {
    let empty = tempfile::tempdir().expect("resolution fixture directory is created");
    let search = std::env::join_paths([empty.path()]).expect("the fixture search path joins");

    let _ = resolved_executable(
        std::ffi::OsStr::new("codex-missing"),
        empty.path(),
        Some(&search),
    );
}
