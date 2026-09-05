//! Compatibility smoke against the real, pinned Claude Code CLI.
//!
//! Ignored by default: it spawns the installed executable, spends one real
//! model exchange, and therefore needs credentials the ordinary Rust workflow
//! never has. `.github/workflows/claude-smoke.yml` is the only automated
//! caller: its unprivileged gate rejects changed fork pull requests before the
//! environment-backed smoke job can start.
//!
//! What it proves is protocol compatibility, which is what a CLI version bump
//! actually breaks: `claude --print --verbose --output-format=stream-json`
//! still opens with a `system/init` event that names the pinned version,
//! reports the private MCP server connected, and exposes no ambient
//! instruction surface; the stream still ends in a typed terminal `result`;
//! and the adapter still decodes that envelope as a completed or refused
//! terminal outcome carrying correlation, reported-model, and usage evidence.
//! It deliberately asserts nothing about answer quality.
//!
//! The version check runs twice and fails closed both times. Before spending,
//! the credential-free `--version` probe refuses an executable that is not the
//! pinned one — without it a drifted local or CI executable could satisfy every
//! assertion below and be recorded as evidence for a version it never ran.
//! During the exchange the adapter's own `system/init` handshake re-checks the
//! version the running process reports, so a launcher whose banner disagrees
//! with its runtime is stream-protocol boundary loss rather than a pass.
//!
//! No prompt caching. The adapter requests none and this smoke adds none: a
//! cache write costs more than the uncached read it replaces, and at one
//! exchange per pin bump there is never a second read to amortize it against.
//! Caching would raise the price of the very run it is supposed to cheapen.
//!
//! Credential discipline: the live entry point receives only a non-secret path
//! and resolves that file through the same per-preparation access boundary as
//! production. The adapter writes the value to a private request-scoped Claude
//! settings store and forwards only that store's `CLAUDE_CONFIG_DIR`; it never
//! puts the key itself in the child's process environment. The version probe
//! discards the child's stderr rather than reporting it, and every other
//! failure message carries only evidence the adapter has already redacted with
//! the exact request credential.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::process::Stdio;
use std::time::Duration;

use signalbox_model_runtime::{
    BoundaryLossEvidence, CancellationSignal, CompletionEvidence, CompletionFinish,
    ConversationMessage, CredentialAccess, CredentialAccessError, CredentialAccessFailure,
    CredentialReference, CredentialValue, DeliveryMode, ExchangeFacts, LossCause, ModelOperation,
    ModelRuntime, ModelSettings, PreparationDefect, PreparationFailure, PreparationOutcome,
    ProviderReportedModel, ProviderRequestId, RefusalEvidence, RequestedTarget, ResolvedTarget,
    TerminalEvidence, TokenUsage, ToolCallsAtLoss,
};
use signalbox_model_runtime_claude_cli::{
    CLAUDE_CLI_FILE_CREDENTIAL_ENV_KEY, ClaudeCliConfig, ClaudeCliPreparedRequest,
    ClaudeCliRuntime, SUPPORTED_CLAUDE_CLI_VERSION,
};
use signalbox_test_bin::test_bin_path;

/// Overrides the executable under test. The default resolves through `PATH`;
/// CI points it at the binary `npm ci` unpacked from the pin manifest.
const EXECUTABLE_VARIABLE: &str = "SIGNALBOX_CLAUDE_SMOKE_EXECUTABLE";

/// Overrides the model. The default is the cheapest model this adapter's
/// provider offers and the smoke has no reason to buy a more capable one, but
/// which models a credential may address is account-scoped — an API key and a
/// subscription login do not offer the same set — so the caller can name
/// another.
const MODEL_VARIABLE: &str = "SIGNALBOX_CLAUDE_SMOKE_MODEL";

/// Deployment-owned file the gated workflow writes before the test process
/// starts. The value itself never enters this process's environment.
const CREDENTIAL_FILE_VARIABLE: &str = "SIGNALBOX_CLAUDE_SMOKE_CREDENTIAL_FILE";

const DEFAULT_EXECUTABLE: &str = "claude";

/// The cheapest model the provider catalog offers, and the selected default
/// for this smoke. A compatibility check buys protocol evidence, not answer
/// quality, so the cheapest model that exercises the whole event stream is the
/// right one.
const DEFAULT_MODEL: &str = "claude-haiku-4-5";

const CLAUDE_SMOKE_WORKFLOW: &str = include_str!("../../../.github/workflows/claude-smoke.yml");

/// The environment-scoped secret the smoke workflow supplies. It is named here
/// so the workflow assertions below reference one spelling rather than
/// restating it per assertion.
const SMOKE_CREDENTIAL_VARIABLE: &str = "ANTHROPIC_API_KEY";

/// A trivial prompt keeps the exchange to the smallest billable turn that
/// still exercises the whole event protocol.
const PROMPT: &str = "Reply with the single word: ready";

/// Matches the ceiling this adapter's offline process suite uses, which is the
/// smallest value already proven to carry a complete Claude Code turn through
/// the decoder rather than a value chosen fresh here.
const MAX_OUTPUT_TOKENS: u32 = 256;

/// Arbitrary non-default facts that prove the shared response projection
/// preserves terminal evidence rather than manufacturing defaults.
const FIXTURE_SESSION_ID: &str = "fixture-session";
const FIXTURE_MODEL: &str = "fixture-model";
const FIXTURE_INPUT_TOKENS: u64 = 3;
const FIXTURE_OUTPUT_TOKENS: u64 = 1;

#[tokio::test]
#[ignore = "spends one real Claude Code CLI exchange; run only from the gated compatibility smoke"]
async fn the_pinned_claude_cli_completes_one_exchange() {
    let executable = absolute_executable(&executable_override_or_default());
    let model = variable_or(MODEL_VARIABLE, DEFAULT_MODEL);

    assert_pre_spend_contract(&executable).await;

    let working_directory = tempfile::tempdir().expect("smoke working directory is created");
    let credential_reference = CredentialReference::new("claude-smoke");
    let credentials = SmokeFileCredentialAccess {
        path: std::path::PathBuf::from(
            std::env::var_os(CREDENTIAL_FILE_VARIABLE)
                .expect("the gated smoke supplies a credential file path"),
        ),
        reference: credential_reference.clone(),
    };
    let mut config = ClaudeCliConfig::new(
        &executable,
        mcp_bridge_executable(),
        working_directory.path(),
        credential_reference.clone(),
        None,
        None,
    );
    config.exchange_timeout = Some(Duration::from_secs(4 * 60));
    let runtime = ClaudeCliRuntime::new_with_file_delivery(
        config,
        credentials,
        CLAUDE_CLI_FILE_CREDENTIAL_ENV_KEY,
    )
    .expect("smoke runtime file delivery is valid");

    let mut operation = ModelOperation::new(
        "claude-smoke".to_string(),
        credential_reference,
        RequestedTarget::new(&model),
        ResolvedTarget::new(&model),
        vec![ConversationMessage::user_text(PROMPT)],
        ModelSettings::new(MAX_OUTPUT_TOKENS),
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

    // Reaching a decoded response is itself the version-handshake evidence: the
    // adapter turns a `system/init` whose reported Claude Code version differs
    // from the pinned one into stream-protocol boundary loss, which this
    // projection refuses.
    let decoded = require_decoded_response(report.evidence);
    assert!(
        decoded.exchange.provider_request_id.is_some(),
        "no session id reached the exchange facts, so `system/init` no longer \
         parses for model {model}"
    );
    assert!(
        decoded.reported_model.is_some(),
        "`system/init` no longer reports the model the adapter records as \
         provider-reported evidence"
    );
    assert!(
        decoded.usage.input_tokens.is_some_and(|tokens| tokens > 0)
            && decoded.usage.output_tokens.is_some(),
        "the terminal `result` event no longer reports the usage counters the adapter reads"
    );
}

struct SmokeFileCredentialAccess {
    path: std::path::PathBuf,
    reference: CredentialReference,
}

impl CredentialAccess for SmokeFileCredentialAccess {
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
        tokio::fs::read(&self.path)
            .await
            .map(CredentialValue::new)
            .map_err(|error| {
                CredentialAccessError::new(
                    reference.clone(),
                    if error.kind() == std::io::ErrorKind::NotFound {
                        CredentialAccessFailure::Unavailable
                    } else {
                        CredentialAccessFailure::Unreadable
                    },
                )
            })
    }
}

/// Credential-free entry point for the real-CLI controls that must pass before
/// the live smoke authenticates or spends. Install the pinned CLI, point
/// `SIGNALBOX_CLAUDE_SMOKE_EXECUTABLE` at it, and run this exact ignored test.
#[tokio::test]
#[ignore = "requires the installed pinned CLI; credential-free pre-spend probes only"]
async fn the_pinned_claude_cli_pre_spend_contract_holds() {
    let executable = absolute_executable(&executable_override_or_default());

    assert_pre_spend_contract(&executable).await;
}

/// The pre-spend contract is the version gate alone. The Codex adapter also
/// probes a built-in feature registry and an ambient-skill catalog before
/// spending; the Claude Code CLI exposes no credential-free equivalent of
/// either, so those surfaces are proven inside the exchange instead — the
/// adapter refuses a `system/init` that reports any slash command, skill, or
/// plugin, and refuses a tool inventory that differs from the declared MCP
/// surface. That is a weaker gate than Codex's: it fails after spend rather
/// than before it.
async fn assert_pre_spend_contract(executable: &std::path::Path) {
    assert_pinned_version(executable).await;
}

/// The path of the adapter-owned MCP bridge Cargo builds beside this test.
/// Production supplies this path from configuration; the smoke must exercise
/// the same bridge the adapter ships rather than a stand-in.
fn mcp_bridge_executable() -> std::path::PathBuf {
    test_bin_path!("signalbox-claude-mcp-bridge")
}

/// `ClaudeCliPreparedRequest` deliberately implements no diagnostic formatting,
/// so each non-prepared outcome reports only its safe shared-runtime evidence.
#[track_caller]
fn require_prepared(
    outcome: PreparationOutcome<String, ClaudeCliPreparedRequest<String>>,
) -> ClaudeCliPreparedRequest<String> {
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
    reported_model: Option<ProviderReportedModel>,
    usage: TokenUsage,
}

#[track_caller]
fn require_decoded_response(evidence: TerminalEvidence) -> DecodedResponse {
    match evidence {
        TerminalEvidence::Completed(completed) => DecodedResponse {
            exchange: completed.exchange,
            reported_model: completed.reported_model,
            usage: completed.usage,
        },
        TerminalEvidence::Refused(refused) => DecodedResponse {
            exchange: refused.exchange,
            reported_model: refused.reported_model,
            usage: refused.usage,
        },
        // Adapter-produced evidence is already credential-shape redacted, so
        // printing it here cannot surface credential material. Enumerated
        // explicitly, per docs/style.md's owned-enum rule, so a future
        // `TerminalEvidence` variant fails to compile here instead of
        // silently inheriting this panic path — a new terminal classification
        // must be reviewed for whether it counts as decoded compatibility
        // evidence, not absorbed as an ordinary rejection.
        rejected @ (TerminalEvidence::CompletedWithProviderCompaction { .. }
        | TerminalEvidence::ProviderError(_)
        | TerminalEvidence::CancellationConfirmed(_)
        | TerminalEvidence::ProvenUnsent(_)
        | TerminalEvidence::BoundaryLoss(_)) => {
            panic!("the pinned Claude Code CLI returned no decoded response: {rejected:?}")
        }
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
/// binds this evidence to a specific executable before anything is spent.
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
    let version = reported_version(&reported).unwrap_or_else(|| {
        panic!(
            "`{} --version` printed no version token",
            executable.display()
        )
    });

    assert_eq!(
        version,
        SUPPORTED_CLAUDE_CLI_VERSION,
        "the executable at `{}` reports {version}, but this smoke can \
         only produce compatibility evidence for the pinned \
         {SUPPORTED_CLAUDE_CLI_VERSION}; install the version pinned in \
         crates/model-runtime-claude-cli/package.json",
        executable.display()
    );
}

/// The version token in a Claude Code `--version` banner, which leads with the
/// bare version and follows it with the product name (`2.1.220 (Claude Code)`).
/// Reading the leading token rather than the trailing one keeps the parenthesized
/// product name out of the comparison; a banner reduced to the version alone
/// still parses.
fn reported_version(banner: &str) -> Option<&str> {
    banner.lines().next()?.split_whitespace().next()
}

#[test]
fn reported_version_reads_the_leading_token_of_the_banner() {
    assert_eq!(reported_version("2.1.220 (Claude Code)\n"), Some("2.1.220"));
}

#[test]
fn reported_version_accepts_a_bare_version_banner() {
    assert_eq!(reported_version("2.1.220\n"), Some("2.1.220"));
}

#[test]
fn reported_version_reads_only_the_first_line() {
    assert_eq!(
        reported_version("2.1.220 (Claude Code)\ntrailing notice\n"),
        Some("2.1.220")
    );
}

#[test]
fn reported_version_rejects_empty_output() {
    assert_eq!(reported_version(""), None);
}

#[test]
fn reported_version_rejects_a_blank_first_line() {
    assert_eq!(reported_version("   \n2.1.220\n"), None);
}

/// Every place the smoke workflow is permitted to reach the credential's
/// *value*, in file order. The single environment binding scopes the secret to
/// the source-file materialization step, whose guard and shell-builtin write
/// read it without rendering it or placing it in an argument vector.
const PERMITTED_CREDENTIAL_SITES: &[&str] = &[
    "ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}",
    "test -n \"${ANTHROPIC_API_KEY}\" \\",
    "printf '%s' \"${ANTHROPIC_API_KEY}\" > \"${SIGNALBOX_CLAUDE_SMOKE_CREDENTIAL_FILE}\"",
];

/// The workflow's active lines, trimmed and in file order, each paired with
/// the indentation it was written at.
///
/// Blank lines and comments are dropped everywhere the assertions below look,
/// because a commented-out line is text GitHub never acts on. Treating it as
/// declared is the exact drift these assertions exist to catch: a commented-out
/// `cron` entry still reads as a scheduled firing to a substring search while
/// no firing happens. The indentation travels with each line so a lookup can be
/// scoped to the block that owns it rather than to the whole document.
fn active_workflow_lines(workflow: &str) -> Vec<(usize, &str)> {
    workflow
        .lines()
        .map(|line| (line.len() - line.trim_start().len(), line.trim()))
        .filter(|(_, content)| !content.is_empty() && !content.starts_with('#'))
        .collect()
}

/// The lines nested under `key` within `block`, or nothing when `block` does
/// not declare `key` at its own level.
///
/// The key must appear at the indentation `block` opens at, so a same-named key
/// belonging to a deeper structure is not mistaken for this one, and the value
/// runs until the first line that returns to that indentation or shallower.
fn mapping_value_block<'b, 's>(block: &'b [(usize, &'s str)], key: &str) -> &'b [(usize, &'s str)] {
    let Some(&(level, _)) = block.first() else {
        return &[];
    };
    let header = format!("{key}:");
    let Some(start) = block.iter().position(|(indent, content)| {
        *indent == level
            && content
                .strip_prefix(&header)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    }) else {
        return &[];
    };
    let body = &block[start + 1..];
    let extent = body
        .iter()
        .position(|(indent, _)| *indent <= level)
        .unwrap_or(body.len());
    &body[..extent]
}

/// The entries of the YAML sequence declared at `path`, in file order, with the
/// `- ` marker removed.
///
/// Each element of `path` names a mapping key matched only within the block the
/// previous element opened, so `["on", "push", "paths"]` reads the push
/// trigger's own filter and cannot be satisfied by the same entry written under
/// a different trigger. Comments are already gone, so a commented-out entry is
/// absent rather than declared.
///
/// Accepted limit: a sequence must be indented under its key, which is how this
/// workflow writes them. Reformatting one flush with its key would return no
/// entries and fail the assertion that asked for it — loud rather than vacuous,
/// which is the safe direction for a check whose whole job is to notice that a
/// trigger stopped existing.
fn workflow_sequence_at(workflow: &str, path: &[&str]) -> Vec<String> {
    let lines = active_workflow_lines(workflow);
    let mut block = lines.as_slice();
    for key in path {
        block = mapping_value_block(block, key);
    }
    block
        .iter()
        .filter_map(|(_, content)| content.strip_prefix("- "))
        .map(str::to_owned)
        .collect()
}

/// Whether `fragment` appears on a line the workflow actually acts on.
///
/// The plain `str::contains` these assertions used before was satisfied by a
/// commented-out occurrence, so a disabled step still read as present.
fn workflow_declares(workflow: &str, fragment: &str) -> bool {
    active_workflow_lines(workflow)
        .iter()
        .any(|(_, content)| content.contains(fragment))
}

/// Every active line of the workflow step introduced by `name`, in file order,
/// beginning with the step's own `- name:` marker.
///
/// The step runs from that marker to the first line back at its indentation,
/// which is the next step's marker or the end of the list, so the whole body —
/// the `env:` bindings and every line of the `run:` script — is returned
/// together.
fn workflow_step_lines(workflow: &str, name: &str) -> Vec<String> {
    let lines = active_workflow_lines(workflow);
    let marker = format!("- name: {name}");
    let Some(start) = lines.iter().position(|(_, content)| *content == marker) else {
        return Vec::new();
    };
    let Some(&(level, _)) = lines.get(start) else {
        return Vec::new();
    };
    let body = lines.get(start + 1..).unwrap_or_default();
    let extent = body
        .iter()
        .position(|(indent, _)| *indent <= level)
        .unwrap_or(body.len());
    std::iter::once(marker)
        .chain(
            body.get(..extent)
                .unwrap_or_default()
                .iter()
                .map(|(_, content)| (*content).to_owned()),
        )
        .collect()
}

/// The one step that carries the credential's value.
const CREDENTIALED_STEP: &str = "Materialize the file-delivered credential";

/// Every line that step executes, in file order.
///
/// Pinned whole, because naming the credential is not the only way to read it.
/// A command such as `env`, `export -p`, or `set` writes the whole environment
/// — the key with it — without the variable appearing on its line, so an
/// inventory keyed on the name cannot see it. Nothing else in this workflow
/// receives the value in its environment: this step writes the deployment-owned
/// source file and the test process later receives only its path. The
/// name-keyed inventory above remains the complement, catching a binding that
/// puts the credential into some other step.
const CREDENTIALED_STEP_LINES: &[&str] = &[
    "- name: Materialize the file-delivered credential",
    "env:",
    "ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}",
    "SIGNALBOX_CLAUDE_SMOKE_CREDENTIAL_FILE: >-",
    "${{ runner.temp }}/signalbox-claude-smoke-credential",
    "run: |",
    "set -euo pipefail",
    "test -n \"${ANTHROPIC_API_KEY}\" \\",
    "|| { echo \"the claude-smoke environment provides no API key\"; exit 1; }",
    "umask 077",
    "printf '%s' \"${ANTHROPIC_API_KEY}\" > \"${SIGNALBOX_CLAUDE_SMOKE_CREDENTIAL_FILE}\"",
];

/// Any line added to, removed from, or changed in the credentialed step fails
/// here and must be reviewed for what it does with the key in scope.
#[test]
fn the_credentialed_step_is_pinned_in_full() {
    assert_eq!(
        workflow_step_lines(CLAUDE_SMOKE_WORKFLOW, CREDENTIALED_STEP),
        CREDENTIALED_STEP_LINES,
        "the step that carries {SMOKE_CREDENTIAL_VARIABLE} changed; every line that runs \
         with the key in scope must be reviewed before it is permitted"
    );
}

/// The workflow text the step fixtures extend, so each differs from the pinned
/// step by exactly the one line under test.
fn credentialed_step_workflow() -> String {
    CREDENTIALED_STEP_LINES.join("\n        ")
}

#[test]
fn credentialed_step_scan_accepts_the_pinned_step() {
    assert_eq!(
        workflow_step_lines(&credentialed_step_workflow(), CREDENTIALED_STEP),
        CREDENTIALED_STEP_LINES
    );
}

/// The read the name-keyed inventory cannot see: `env` writes the credential to
/// the job log without the variable appearing on its line.
#[test]
fn credentialed_step_scan_detects_an_environment_dump() {
    let workflow = format!("{}\n        env", credentialed_step_workflow());

    assert_eq!(
        credential_reading_lines(&workflow),
        PERMITTED_CREDENTIAL_SITES
    );
    assert_ne!(
        workflow_step_lines(&workflow, CREDENTIALED_STEP),
        CREDENTIALED_STEP_LINES
    );
}

/// `export -p` and `set` dump the environment just as `env` does, and name the
/// credential no more than it does.
#[test]
fn credentialed_step_scan_detects_an_export_dump() {
    let workflow = format!("{}\n        export -p", credentialed_step_workflow());

    assert_ne!(
        workflow_step_lines(&workflow, CREDENTIALED_STEP),
        CREDENTIALED_STEP_LINES
    );
}

/// A step whose marker is absent yields nothing, so a renamed or deleted step
/// fails the pin rather than matching an empty expectation.
#[test]
fn credentialed_step_scan_is_empty_for_a_missing_step() {
    assert_eq!(
        workflow_step_lines(SCOPING_FIXTURE_WORKFLOW, CREDENTIALED_STEP),
        Vec::<String>::new()
    );
}

/// Every line that reaches the credential's value, trimmed and in file order.
///
/// A line qualifies when it names the credential anywhere outside a comment.
/// Keying on the name rather than on an expansion sigil is deliberate: `$`
/// covers the GitHub expression form, a braced shell expansion, and a bare
/// one, but not a read that never expands anything — `printenv
/// ANTHROPIC_API_KEY` writes the value to the job log without a `$` in sight.
/// Only comments are excluded, because prose that names the variable reads
/// nothing; every executable mention is inventoried whether it expands or not.
/// Factored out of the assertion so synthetic workflows can prove the scan
/// catches each form.
fn credential_reading_lines(workflow: &str) -> Vec<&str> {
    active_workflow_lines(workflow)
        .into_iter()
        .filter(|(_, content)| content.contains(SMOKE_CREDENTIAL_VARIABLE))
        .map(|(_, content)| content)
        .collect()
}

/// Pins the complete inventory rather than counting one spelling. Asserting a
/// count of the `${{ secrets.… }}` form alone would let a later
/// `echo "$ANTHROPIC_API_KEY"`, or the variable handed to another command,
/// place the key in the job log or a process listing that the environment's
/// access controls do not cover — while the assertion still passed. Any added
/// reference in any expansion form changes this inventory and fails here.
#[test]
fn the_smoke_workflow_reaches_the_credential_only_where_permitted() {
    assert_eq!(
        credential_reading_lines(CLAUDE_SMOKE_WORKFLOW),
        PERMITTED_CREDENTIAL_SITES,
        "the smoke workflow's credential references changed; every site that reads \
         {SMOKE_CREDENTIAL_VARIABLE} must be reviewed before it is permitted"
    );
}

#[test]
fn the_smoke_workflow_supplies_and_cleans_one_nonsecret_credential_file_path() {
    let path_binding = "${{ runner.temp }}/signalbox-claude-smoke-credential".to_string();
    assert!(workflow_declares(
        CLAUDE_SMOKE_WORKFLOW,
        "SIGNALBOX_CLAUDE_SMOKE_CREDENTIAL_FILE: >-"
    ));
    assert!(workflow_declares(
        CLAUDE_SMOKE_WORKFLOW,
        "${{ runner.temp }}/signalbox-claude-smoke-credential"
    ));
    assert!(
        workflow_step_lines(
            CLAUDE_SMOKE_WORKFLOW,
            "Run one live exchange through the adapter"
        )
        .contains(&path_binding)
    );
    assert_eq!(
        workflow_step_lines(
            CLAUDE_SMOKE_WORKFLOW,
            "Remove the file-delivered credential"
        ),
        [
            "- name: Remove the file-delivered credential",
            "if: ${{ always() }}",
            "env:",
            "SIGNALBOX_CLAUDE_SMOKE_CREDENTIAL_FILE: >-",
            "${{ runner.temp }}/signalbox-claude-smoke-credential",
            "run: rm -f \"${SIGNALBOX_CLAUDE_SMOKE_CREDENTIAL_FILE}\"",
        ]
    );
}

/// The workflow text the scan fixtures extend, so each fixture differs from the
/// permitted inventory by exactly the one line under test.
fn permitted_credential_workflow() -> String {
    PERMITTED_CREDENTIAL_SITES.join("\n")
}

#[test]
fn credential_scan_accepts_the_permitted_inventory() {
    assert_eq!(
        credential_reading_lines(&permitted_credential_workflow()),
        PERMITTED_CREDENTIAL_SITES
    );
}

/// A quoted braced expansion echoed into the job log is the leak this scan
/// exists to catch, and it is not the `${{ secrets.… }}` spelling.
#[test]
fn credential_scan_detects_a_braced_shell_expansion() {
    let workflow = format!(
        "{}\necho \"${{{SMOKE_CREDENTIAL_VARIABLE}}}\"",
        permitted_credential_workflow()
    );

    assert_ne!(
        credential_reading_lines(&workflow),
        PERMITTED_CREDENTIAL_SITES
    );
}

/// The unbraced spelling leaks identically and must not slip past the scan.
#[test]
fn credential_scan_detects_a_bare_shell_expansion() {
    let workflow = format!(
        "{}\necho ${SMOKE_CREDENTIAL_VARIABLE}",
        permitted_credential_workflow()
    );

    assert_ne!(
        credential_reading_lines(&workflow),
        PERMITTED_CREDENTIAL_SITES
    );
}

/// Handing the value to another command puts it in that process's argv, which
/// a process listing exposes without any `echo` at all.
#[test]
fn credential_scan_detects_an_argv_expansion() {
    let workflow = format!(
        "{}\nsome-tool --token \"${{{SMOKE_CREDENTIAL_VARIABLE}}}\"",
        permitted_credential_workflow()
    );

    assert_ne!(
        credential_reading_lines(&workflow),
        PERMITTED_CREDENTIAL_SITES
    );
}

/// A second environment binding forwards the value to a step that was never
/// reviewed for it, so it is a change to the inventory even though nothing is
/// rendered.
#[test]
fn credential_scan_detects_a_second_environment_binding() {
    let workflow = format!(
        "{}\nOTHER_TOKEN: ${{{{ secrets.{SMOKE_CREDENTIAL_VARIABLE} }}}}",
        permitted_credential_workflow()
    );

    assert_ne!(
        credential_reading_lines(&workflow),
        PERMITTED_CREDENTIAL_SITES
    );
}

/// A read that never expands anything still writes the value to the job log,
/// so keying the scan on a `$` sigil would have missed it entirely.
#[test]
fn credential_scan_detects_a_read_without_expansion() {
    let workflow = format!(
        "{}\nprintenv {SMOKE_CREDENTIAL_VARIABLE}",
        permitted_credential_workflow()
    );

    assert_ne!(
        credential_reading_lines(&workflow),
        PERMITTED_CREDENTIAL_SITES
    );
}

/// Naming the variable in prose reads nothing, so the header comment's
/// mentions must not inflate the inventory — otherwise the assertion would
/// have to be relaxed to tolerate documentation.
#[test]
fn credential_scan_ignores_a_prose_mention() {
    let workflow = format!(
        "{}\n# the {SMOKE_CREDENTIAL_VARIABLE} secret is supplied by the environment",
        permitted_credential_workflow()
    );

    assert_eq!(
        credential_reading_lines(&workflow),
        PERMITTED_CREDENTIAL_SITES
    );
}

/// A pinned `npm ci` install is what binds the workflow's executable to the
/// manifest this crate's build script reads; disabling lifecycle scripts keeps
/// package-authored code from running as an implicit side effect of install.
#[test]
fn the_smoke_workflow_installs_the_pinned_cli_without_lifecycle_scripts() {
    assert!(
        workflow_declares(
            CLAUDE_SMOKE_WORKFLOW,
            "npm ci --ignore-scripts --prefix crates/model-runtime-claude-cli"
        ),
        "the smoke workflow no longer installs the pinned CLI from the lockfile with \
         lifecycle scripts disabled"
    );
}

/// The pinned package's launcher is a stub until its own installer places the
/// platform-native binary, so the scripts-disabled install above must be
/// followed by that installer as an explicit step. Without it the workflow
/// would probe an executable that refuses to run, and the version gate would
/// fail for a reason unrelated to the pin under test.
#[test]
fn the_smoke_workflow_places_the_native_binary_explicitly() {
    assert!(
        workflow_declares(
            CLAUDE_SMOKE_WORKFLOW,
            "node node_modules/@anthropic-ai/claude-code/install.cjs"
        ),
        "the smoke workflow no longer runs the pinned package's installer, so the \
         launcher it probes has no native binary behind it"
    );
}

/// The wrapped CLI and the model behind it move on their own cadence, so an
/// unchanged pin can stop working between adapter changes. The twice-daily
/// canary is what finds that without waiting for the next pull request;
/// dropping either firing would halve the detection window silently.
///
/// Read from `on.schedule` itself rather than searched for as text, and pinned
/// as the complete list rather than counted. A count over the raw document was
/// satisfied by a firing that had been commented out — the text stays, the
/// schedule does not — and said nothing about a third firing being added.
#[test]
fn the_smoke_workflow_runs_a_twice_daily_drift_canary() {
    assert_eq!(
        workflow_sequence_at(CLAUDE_SMOKE_WORKFLOW, &["on", "schedule"]),
        ["cron: \"0 13 * * *\"", "cron: \"0 1 * * *\""],
        "the smoke workflow no longer schedules exactly the two daily drift-canary firings"
    );
}

/// A change to the workflow is a change to the gate itself, so a push to
/// `main` touching only this file must run it. Read from the `on.push.paths`
/// list specifically: searching every line of the document would stay satisfied
/// if this trigger were deleted while the identical entry appeared under some
/// other trigger, and a commented-out entry would count as declared.
#[test]
fn the_smoke_workflow_push_trigger_names_its_own_definition() {
    assert!(
        workflow_sequence_at(CLAUDE_SMOKE_WORKFLOW, &["on", "push", "paths"])
            .iter()
            .any(|entry| entry == "\".github/workflows/claude-smoke.yml\""),
        "the smoke workflow's push path filter no longer lists its own definition, \
         so a push that changes only this file would not run it"
    );
}

/// The pull-request side is gated by the eligibility job rather than a `paths`
/// filter, so the same self-change parity has to be asserted in that predicate
/// too — and separately from the push trigger above, since either can be
/// dropped without changing the other. The predicate is a shell fragment rather
/// than a YAML node, so it is matched as a line the workflow acts on; that is
/// what keeps a commented-out clause from reading as an active one.
#[test]
fn the_smoke_gate_predicate_names_the_workflow_definition() {
    assert!(
        workflow_declares(
            CLAUDE_SMOKE_WORKFLOW,
            "|| \"${path}\" == \".github/workflows/claude-smoke.yml\" ]]; then"
        ),
        "the eligibility gate's changed-path predicate no longer names this workflow, \
         so a pull request that changes only this file would report not-applicable"
    );
}

/// A workflow whose triggers deliberately repeat one another's entries and
/// carry disabled ones, so the fixtures below prove a lookup reads the context
/// it asked for rather than anywhere the same text happens to appear.
const SCOPING_FIXTURE_WORKFLOW: &str = r#"
name: fixture
on:
  pull_request:
    paths:
      - "fixture/decoy.yml"
  push:
    branches: [main]
    paths:
      - "fixture/wanted.yml"
      # - "fixture/retired.yml"
  schedule:
    - cron: "0 13 * * *"
    # - cron: "0 1 * * *"
jobs:
  build:
    steps:
      - run: echo fixture
"#;

/// The failure the push-trigger assertion is meant to survive: the entry is
/// gone from `push.paths` but still present elsewhere in the document.
#[test]
fn workflow_sequence_excludes_an_entry_from_another_trigger() {
    assert!(
        SCOPING_FIXTURE_WORKFLOW.contains("fixture/decoy.yml"),
        "the fixture must contain the decoy for this to prove anything"
    );

    assert_eq!(
        workflow_sequence_at(SCOPING_FIXTURE_WORKFLOW, &["on", "push", "paths"]),
        ["\"fixture/wanted.yml\""]
    );
}

/// The failure the drift-canary assertion is meant to survive: the firing's
/// text remains but GitHub no longer schedules it.
#[test]
fn workflow_sequence_excludes_a_commented_out_entry() {
    assert!(
        SCOPING_FIXTURE_WORKFLOW.contains("cron: \"0 1 * * *\""),
        "the fixture must contain the disabled firing for this to prove anything"
    );

    assert_eq!(
        workflow_sequence_at(SCOPING_FIXTURE_WORKFLOW, &["on", "schedule"]),
        ["cron: \"0 13 * * *\""]
    );
}

/// An undeclared path yields nothing rather than falling back to a wider scope,
/// so a deleted trigger fails its assertion instead of matching a sibling.
#[test]
fn workflow_sequence_is_empty_for_an_undeclared_path() {
    assert_eq!(
        workflow_sequence_at(SCOPING_FIXTURE_WORKFLOW, &["on", "push", "tags"]),
        Vec::<String>::new()
    );
}

/// The line-level check carries the same rule, so a step that was commented out
/// no longer reads as one the workflow runs.
#[test]
fn workflow_declares_excludes_a_commented_out_line() {
    assert!(SCOPING_FIXTURE_WORKFLOW.contains("fixture/retired.yml"));
    assert!(!workflow_declares(
        SCOPING_FIXTURE_WORKFLOW,
        "fixture/retired.yml"
    ));
    assert!(workflow_declares(
        SCOPING_FIXTURE_WORKFLOW,
        "run: echo fixture"
    ));
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

/// Writes an executable version-probe script and returns its path.
#[cfg(unix)]
fn version_probe_fixture(directory: &std::path::Path, script: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = directory.join("claude-version-probe");
    std::fs::write(&path, script).expect("the version-probe script is written");
    let mut permissions = std::fs::metadata(&path)
        .expect("the version-probe script has metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&path, permissions).expect("the version-probe script is runnable");
    path
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

/// A probe that floods stdout is stopped at the byte bound and fails the gate
/// with the overflow diagnostic — not buffered whole until the timeout, which
/// let a fast producer consume unbounded memory before failing.
#[cfg(unix)]
#[tokio::test]
async fn version_probe_overflow_kills_flooding_probe_descendants() {
    let directory = tempfile::tempdir().expect("flood fixture directory is created");
    let pid_file = directory.path().join("descendant-pid");
    let script = format!(
        "#!/bin/sh\nsleep 60 &\nprintf '%s\\n' \"$!\" > '{}'\nexec yes claude-flood\n",
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
    let signalability = Signalability::observe(pid);
    // Read the stat line only for a pid still worth classifying; an
    // unsignalable one is already gone.
    let proc_stat = match signalability {
        Signalability::Signalable => proc_stat_line(pid),
        Signalability::Unsignalable => None,
    };
    classify_process_liveness(signalability, proc_stat.as_deref())
}

/// Whether a pid can still be signalled — the first of the two observations
/// liveness is classified from. A labeled pair rather than a `bool`, per
/// `docs/style.md`'s label discipline: at a call site `Unsignalable` says what
/// `false` would only imply, so a transposed or misread fixture stays visible.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Signalability {
    Signalable,
    Unsignalable,
}

#[cfg(unix)]
impl Signalability {
    fn observe(pid: rustix::process::Pid) -> Self {
        match rustix::process::test_kill_process(pid) {
            Ok(()) => Self::Signalable,
            Err(_) => Self::Unsignalable,
        }
    }
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
fn classify_process_liveness(signalability: Signalability, proc_stat: Option<&str>) -> bool {
    match signalability {
        Signalability::Unsignalable => false,
        Signalability::Signalable => match proc_stat {
            Some(stat) => !proc_stat_is_zombie(stat),
            None => true,
        },
    }
}

/// An unsignalable pid names no process, so it is not live.
#[cfg(unix)]
#[test]
fn unsignalable_process_is_not_live() {
    assert!(!classify_process_liveness(
        Signalability::Unsignalable,
        None
    ));
}

/// Signalability decides first: a stale stat line for a pid that can no longer
/// be signalled cannot revive it.
#[cfg(unix)]
#[test]
fn unsignalable_process_with_a_running_stat_line_is_not_live() {
    assert!(!classify_process_liveness(
        Signalability::Unsignalable,
        Some("4321 (claude) R 1 4321 4321 0 -1")
    ));
}

/// Where `/proc` is unavailable, a signalable pid is live — the fallback that
/// keeps the macOS host from reporting every descendant as already exited.
#[cfg(unix)]
#[test]
fn signalable_process_without_proc_is_live() {
    assert!(classify_process_liveness(Signalability::Signalable, None));
}

/// A signalable pid whose stat line reports a zombie has exited; reporting it
/// live would make the cleanup observation time out on a dead descendant.
#[cfg(unix)]
#[test]
fn signalable_zombie_is_not_live() {
    assert!(!classify_process_liveness(
        Signalability::Signalable,
        Some("4321 (claude) Z 1 4321 4321 0 -1")
    ));
}

/// A signalable pid whose stat line reports a running process is live;
/// reporting it exited would let cleanup pass with a surviving descendant.
#[cfg(unix)]
#[test]
fn signalable_running_process_is_live() {
    assert!(classify_process_liveness(
        Signalability::Signalable,
        Some("4321 (claude) R 1 4321 4321 0 -1")
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
    assert!(proc_stat_is_zombie("4321 (claude) Z 1 4321 4321 0 -1"));
}

/// A running (`R`) process is not a zombie.
#[cfg(unix)]
#[test]
fn proc_stat_running_process_is_not_a_zombie() {
    assert!(!proc_stat_is_zombie("4321 (claude) R 1 4321 4321 0 -1"));
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

/// The gate accepts an executable that reports exactly the pinned version.
#[cfg(unix)]
#[tokio::test]
async fn version_gate_accepts_the_pinned_version() {
    let directory = tempfile::tempdir().expect("version fixture directory is created");
    let script =
        format!("#!/bin/sh\nprintf '%s (Claude Code)\\n' '{SUPPORTED_CLAUDE_CLI_VERSION}'\n");
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
        "#!/bin/sh\nprintf '%s (Claude Code)\\n' '0.0.1-drifted'\n",
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
/// is offline here — validation, translation, and support-file construction
/// only, with no process spawn.
// Gated to the exact hosts the runtime supports: this exercises the
// preparation projection through a real `ClaudeCliRuntime`, whose construction
// rejects platforms without process-group supervision
// (`ClaudeCliConstructionError::UnsupportedPlatform`). The gate mirrors
// `CLI_PROCESS_GROUP_SUPERVISION_SUPPORTED` — `#[cfg(unix)]` alone is broader
// and would still panic at construction on Unix targets the runtime rejects
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
    let credential_reference = CredentialReference::new("claude-smoke");
    let config = ClaudeCliConfig::new(
        working_directory.path().join("claude-fixture"),
        working_directory.path().join("bridge-fixture"),
        working_directory.path(),
        credential_reference.clone(),
        None,
        None,
    );
    let runtime = ClaudeCliRuntime::new(config).expect("smoke runtime configuration is valid");
    let operation = ModelOperation::new(
        "claude-smoke".to_string(),
        credential_reference,
        RequestedTarget::new(DEFAULT_MODEL),
        ResolvedTarget::new(DEFAULT_MODEL),
        vec![ConversationMessage::user_text(PROMPT)],
        ModelSettings::new(MAX_OUTPUT_TOKENS),
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
        correlation: "claude-smoke".to_string(),
    });
}

#[test]
#[should_panic(expected = "smoke preparation failed")]
fn preparation_projection_rejects_a_failed_outcome() {
    let _ = require_prepared(PreparationOutcome::Failed {
        correlation: "claude-smoke".to_string(),
        failure: PreparationFailure::UnsupportedOperation {
            detail: "fixture failure".to_string(),
        },
    });
}

#[test]
#[should_panic(expected = "smoke preparation found a defect")]
fn preparation_projection_rejects_a_defect_outcome() {
    let _ = require_prepared(PreparationOutcome::Defect {
        correlation: "claude-smoke".to_string(),
        defect: PreparationDefect::RequestConstructionFailed {
            detail: "fixture defect".to_string(),
        },
    });
}

/// The exchange facts and reported model a fixture terminal outcome carries, so
/// the projection tests compare against one accessor rather than restating the
/// same literals per case.
fn fixture_exchange() -> ExchangeFacts {
    ExchangeFacts {
        provider_request_id: Some(ProviderRequestId::new(FIXTURE_SESSION_ID)),
        http_status: None,
        retry_after: None,
    }
}

fn fixture_reported_model() -> ProviderReportedModel {
    ProviderReportedModel::new(FIXTURE_MODEL)
}

fn fixture_usage() -> TokenUsage {
    TokenUsage {
        input_tokens: Some(FIXTURE_INPUT_TOKENS),
        output_tokens: Some(FIXTURE_OUTPUT_TOKENS),
        ..TokenUsage::default()
    }
}

#[test]
fn decoded_response_accepts_completion() {
    let evidence = TerminalEvidence::Completed(CompletionEvidence {
        exchange: fixture_exchange(),
        message_id: None,
        reported_model: Some(fixture_reported_model()),
        finish: CompletionFinish::EndTurn,
        content: Vec::new(),
        usage: fixture_usage(),
    });

    let decoded = require_decoded_response(evidence);
    assert_eq!(decoded.exchange, fixture_exchange());
    assert_eq!(decoded.reported_model, Some(fixture_reported_model()));
    assert_eq!(decoded.usage, fixture_usage());
}

#[test]
fn decoded_response_accepts_refusal_without_completion_material() {
    let evidence = TerminalEvidence::Refused(RefusalEvidence {
        exchange: fixture_exchange(),
        message_id: None,
        reported_model: Some(fixture_reported_model()),
        content: Vec::new(),
        usage: fixture_usage(),
    });

    let decoded = require_decoded_response(evidence);
    assert_eq!(decoded.exchange, fixture_exchange());
    assert_eq!(decoded.reported_model, Some(fixture_reported_model()));
    assert_eq!(decoded.usage, fixture_usage());
}

/// The smoke accepts only a decoded response: every other terminal variant —
/// cancellation, failure, defect, boundary loss — is rejected rather than
/// reported as a successful exchange, so a compatibility break cannot pass the
/// gate as a completed turn. A version handshake mismatch arrives as exactly
/// this variant.
#[test]
#[should_panic(expected = "the pinned Claude Code CLI returned no decoded response")]
fn decoded_response_rejects_an_unexpected_terminal_variant() {
    let evidence = TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
        cause: LossCause::ResponseUnintelligible {
            detail: "fixture terminal variant".to_string(),
        },
        exchange: fixture_exchange(),
        reported_model: None,
        finish_reported: None,
        tool_calls: ToolCallsAtLoss::Unobserved,
        usage: fixture_usage(),
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
        b'c', b'l', b'a', b'u', b'd', b'e', 0xff,
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
        std::ffi::OsString::from_vec(vec![b'c', b'l', b'a', b'u', b'd', b'e', b'-', 0xff]);

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
    let override_value = "claude-override".to_string();

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

/// The adapter accepts only an absolute executable path, so the bare-command
/// local default is resolved through `PATH` exactly once, here. CI is
/// unaffected: the workflow always passes the absolute path of the binary
/// installed from the pin manifest.
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

/// An *unset* `PATH` cannot locate a bare command and fails, rather than being
/// silently searched in the current directory as `unwrap_or_default` would.
#[test]
#[should_panic(expected = "PATH is unset")]
fn bare_command_with_unset_path_fails() {
    let directory = tempfile::tempdir().expect("fixture directory is created");
    let _ = resolved_executable(std::ffi::OsStr::new("claude"), directory.path(), None);
}

#[test]
fn executable_resolution_passes_an_absolute_path_through() {
    let directory = tempfile::tempdir().expect("resolution fixture directory is created");
    let absolute = directory.path().join("claude-absolute");

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
    let relative = "node_modules/claude-relative";

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
    let on_path = executable_fixture(populated.path(), "claude-on-path");
    let search = std::env::join_paths([empty.path(), populated.path()])
        .expect("the fixture search path joins");

    let resolved = resolved_executable(
        std::ffi::OsStr::new("claude-on-path"),
        empty.path(),
        Some(&search),
    );

    assert_eq!(resolved, on_path);
}

/// Whether the running process's effective uid is the one the kernel exempts
/// from the discretionary execute check.
///
/// `executable_file` asks `accessat` with `EACCESS`, so the answer is the
/// effective uid's. Root is exempt in a specific way that matters here: it may
/// execute a regular file when *any* execute bit is set, so the owner-vs-other
/// distinction below does not exist for it.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EffectivePrivilege {
    Root,
    Unprivileged,
}

#[cfg(unix)]
impl EffectivePrivilege {
    fn observe() -> Self {
        match rustix::process::geteuid().is_root() {
            true => Self::Root,
            false => Self::Unprivileged,
        }
    }

    /// Which file a `PATH` search must return when its first candidate carries
    /// only the "other" execute bit.
    ///
    /// Unprivileged, the file owner holds no execute permission, so the search
    /// skips the shadow and finds the runnable file in the later directory. As
    /// root the shadow is genuinely executable, so returning it is correct rather
    /// than a defect — the case has a real answer in both environments, which
    /// is why the test asserts one instead of returning early.
    fn other_only_resolution(self, candidates: OtherOnlyCandidates) -> std::path::PathBuf {
        match self {
            Self::Root => candidates.shadow,
            Self::Unprivileged => candidates.on_path,
        }
    }
}

/// The two files a search sees when an earlier `PATH` directory holds an
/// other-only file and a later one holds a runnable file of the same name.
///
/// A struct rather than two `PathBuf` parameters: the paths are the same type
/// and the whole question is which one a given privilege selects, so passing
/// them by position would let a transposed fixture compile and quietly assert
/// the opposite of what it names.
#[cfg(unix)]
struct OtherOnlyCandidates {
    /// Earlier on the search path, not executable by its file owner.
    shadow: std::path::PathBuf,
    /// Later on the search path, executable by its file owner.
    on_path: std::path::PathBuf,
}

/// Writes a fixture file whose only permission bits belong to "other", so the
/// file owner may read it but not execute it.
#[cfg(unix)]
fn other_only_fixture(directory: &std::path::Path, name: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = directory.join(name);
    std::fs::write(&path, "#!/bin/sh\n").expect("the other-only shadow file is written");
    let mut permissions = std::fs::metadata(&path)
        .expect("the other-only shadow file has metadata")
        .permissions();
    permissions.set_mode(0o004 | 0o001);
    std::fs::set_permissions(&path, permissions).expect("the shadow keeps only the other bits");
    path
}

/// A file whose only execute bit belongs to "other" is not executable by
/// its owner, so it cannot shadow the runnable executable in a later
/// directory, exactly as normal PATH lookup skips it — and under an effective
/// root uid, which may execute a file carrying any execute bit, it legitimately
/// does shadow it. Both outcomes are asserted, so neither environment reports a
/// pass without having checked anything.
#[cfg(unix)]
#[test]
fn executable_resolution_skips_an_other_only_execute_bit() {
    let shadowing = tempfile::tempdir().expect("resolution fixture directory is created");
    let populated = tempfile::tempdir().expect("resolution fixture directory is created");
    let shadow = other_only_fixture(shadowing.path(), "claude-other-only");
    let on_path = executable_fixture(populated.path(), "claude-other-only");
    let search = std::env::join_paths([shadowing.path(), populated.path()])
        .expect("the fixture search path joins");

    let resolved = resolved_executable(
        std::ffi::OsStr::new("claude-other-only"),
        shadowing.path(),
        Some(&search),
    );

    assert_eq!(
        resolved,
        EffectivePrivilege::observe()
            .other_only_resolution(OtherOnlyCandidates { shadow, on_path })
    );
}

/// Distinct fixture candidates the privilege assertions below select between.
#[cfg(unix)]
fn other_only_candidates() -> OtherOnlyCandidates {
    OtherOnlyCandidates {
        shadow: std::path::PathBuf::from("/fixture/other-only-shadow"),
        on_path: std::path::PathBuf::from("/fixture/runnable-on-path"),
    }
}

/// The unprivileged expectation, stated directly so the arm the local run does
/// not take is still covered.
#[cfg(unix)]
#[test]
fn unprivileged_resolution_skips_the_other_only_shadow() {
    let candidates = other_only_candidates();
    let expected = candidates.on_path.clone();

    assert_eq!(
        EffectivePrivilege::Unprivileged.other_only_resolution(candidates),
        expected
    );
}

/// The root expectation, likewise — an execute bit anywhere makes the shadow
/// runnable, so it wins the search.
#[cfg(unix)]
#[test]
fn root_resolution_takes_the_other_only_shadow() {
    let candidates = other_only_candidates();
    let expected = candidates.shadow.clone();

    assert_eq!(
        EffectivePrivilege::Root.other_only_resolution(candidates),
        expected
    );
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
        .join(std::ffi::OsStr::from_bytes(b"claude-\xff-dir"));
    std::fs::create_dir(&directory).expect("the non-UTF-8 PATH directory is created");
    let on_path = executable_fixture(&directory, "claude-nonutf8");
    let search = std::env::join_paths([&directory]).expect("the fixture search path joins");

    let resolved = resolved_executable(
        std::ffi::OsStr::new("claude-nonutf8"),
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
    let on_path = executable_fixture(&working.path().join("bin"), "claude-rel");

    let resolved = resolved_executable(
        std::ffi::OsStr::new("claude-rel"),
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
    std::fs::write(shadowing.path().join("claude-shadowed"), "not runnable\n")
        .expect("the non-executable shadow file is written");
    let on_path = executable_fixture(populated.path(), "claude-shadowed");
    let search = std::env::join_paths([shadowing.path(), populated.path()])
        .expect("the fixture search path joins");

    let resolved = resolved_executable(
        std::ffi::OsStr::new("claude-shadowed"),
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
        std::ffi::OsStr::new("claude-missing"),
        empty.path(),
        Some(&search),
    );
}
