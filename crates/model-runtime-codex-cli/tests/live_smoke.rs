//! Compatibility smoke against the real, pinned Codex CLI.
//!
//! Ignored by default: it spawns the installed executable, spends one real
//! model exchange, and therefore needs credentials the ordinary Rust workflow
//! never has. `.github/workflows/codex-smoke.yml` is the only automated caller
//! — never on `pull_request`, so a fork can never reach the secret.
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
    CancellationSignal, CompletionEvidence, CompletionFinish, ConversationMessage,
    CredentialReference, DeliveryMode, ExchangeFacts, ModelOperation, ModelRuntime, ModelSettings,
    PreparationDefect, PreparationFailure, PreparationOutcome, ProviderRequestId, RefusalEvidence,
    RequestedTarget, ResolvedTarget, TerminalEvidence, TokenUsage,
};
use signalbox_model_runtime_codex_cli::{
    CodexCliConfig, CodexCliPreparedRequest, CodexCliRuntime, SUPPORTED_CODEX_CLI_VERSION,
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
const DEFAULT_MODEL: &str = "gpt-5.1-codex-mini";

/// A trivial prompt keeps the exchange to the smallest billable turn that
/// still exercises the whole event protocol.
const PROMPT: &str = "Reply with the single word: ready";

/// Arbitrary non-default facts that prove the shared response projection
/// preserves terminal evidence rather than manufacturing defaults.
const FIXTURE_THREAD_ID: &str = "fixture-thread";
const FIXTURE_INPUT_TOKENS: u64 = 3;
const FIXTURE_OUTPUT_TOKENS: u64 = 1;

#[tokio::test]
#[ignore = "spends one real Codex CLI exchange; run only from the gated compatibility smoke"]
async fn the_pinned_codex_cli_completes_one_exchange() {
    let executable = absolute_executable(&variable_or(EXECUTABLE_VARIABLE, DEFAULT_EXECUTABLE));
    let model = variable_or(MODEL_VARIABLE, DEFAULT_MODEL);

    assert_pinned_version(&executable).await;

    let working_directory = tempfile::tempdir().expect("smoke working directory is created");
    let credential_reference = CredentialReference::new("codex-smoke");
    let mut config = CodexCliConfig::new(
        &executable,
        working_directory.path(),
        credential_reference.clone(),
    );
    config.exchange_timeout = Duration::from_secs(4 * 60);
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

/// Fails closed: an unreadable, unparsable, or mismatched version is a smoke
/// failure, never a skip. A skip here would quietly retire the only check that
/// binds this evidence to a specific executable.
async fn assert_pinned_version(executable: &str) {
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
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Discarded rather than reported: a version probe has no need to
        // surface provider-controlled diagnostics, and this text does not pass
        // through the adapter's redaction.
        .stderr(Stdio::null())
        // Dropping the timed-out future drops the child, which kills and reaps
        // it, so an unbounded probe cannot linger.
        .kill_on_drop(true)
        .spawn()
        .unwrap_or_else(|error| panic!("`{executable} --version` could not be spawned: {error}"));
    let output = tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output())
        .await
        .unwrap_or_else(|_| {
            panic!("`{executable} --version` did not exit within {PROBE_TIMEOUT:?}")
        })
        .unwrap_or_else(|error| panic!("`{executable} --version` could not be awaited: {error}"));
    assert!(
        output.status.success(),
        "`{executable} --version` exited with {}",
        output.status
    );

    let reported = String::from_utf8(output.stdout)
        .unwrap_or_else(|_| panic!("`{executable} --version` printed non-UTF-8 output"));
    let version = reported
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().next_back())
        .unwrap_or_else(|| panic!("`{executable} --version` printed no version token"));

    assert_eq!(
        version, SUPPORTED_CODEX_CLI_VERSION,
        "the executable at `{executable}` reports {version}, but this smoke can \
         only produce compatibility evidence for the pinned \
         {SUPPORTED_CODEX_CLI_VERSION}; install the version pinned in \
         tooling/codex-cli/package.json"
    );
}

/// Writes an executable version-probe script and returns its path.
#[cfg(unix)]
fn version_probe_fixture(directory: &std::path::Path, script: &str) -> String {
    use std::os::unix::fs::PermissionsExt;

    let path = directory.join("codex-version-probe");
    std::fs::write(&path, script).expect("the version-probe script is written");
    let mut permissions = std::fs::metadata(&path)
        .expect("the version-probe script has metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&path, permissions).expect("the version-probe script is runnable");
    path.to_str()
        .expect("the version-probe path is UTF-8")
        .to_string()
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
#[tokio::test]
async fn preparation_projection_returns_a_prepared_capability() {
    let working_directory = tempfile::tempdir().expect("smoke working directory is created");
    let credential_reference = CredentialReference::new("codex-smoke");
    let config = CodexCliConfig::new(
        working_directory.path().join("codex-fixture"),
        working_directory.path(),
        credential_reference.clone(),
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
fn absolute_executable(executable: &str) -> String {
    resolved_executable(
        executable,
        &std::env::current_dir().expect("the smoke process has a working directory"),
        &std::env::var_os("PATH").unwrap_or_default(),
    )
}

/// Pure resolution behind [`absolute_executable`]: the caller supplies the
/// working directory and search path, so every branch is testable without
/// mutating process-global state.
fn resolved_executable(
    executable: &str,
    current_directory: &std::path::Path,
    search: &std::ffi::OsStr,
) -> String {
    let path = std::path::Path::new(executable);
    if path.is_absolute() {
        return executable.to_string();
    }
    if path.components().count() > 1 {
        return current_directory.join(path).to_string_lossy().into_owned();
    }
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
                "`{executable}` was not found on PATH; set {EXECUTABLE_VARIABLE} \
                 to an absolute executable path"
            )
        })
        .to_string_lossy()
        .into_owned()
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
        absolute.to_str().expect("the fixture path is UTF-8"),
        directory.path(),
        std::ffi::OsStr::new(""),
    );

    assert_eq!(resolved, absolute.to_string_lossy());
}

#[test]
fn executable_resolution_anchors_a_relative_path_to_the_working_directory() {
    let directory = tempfile::tempdir().expect("resolution fixture directory is created");
    let relative = "tooling/codex-relative";

    let resolved = resolved_executable(relative, directory.path(), std::ffi::OsStr::new(""));

    assert_eq!(resolved, directory.path().join(relative).to_string_lossy());
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

    let resolved = resolved_executable("codex-on-path", empty.path(), &search);

    assert_eq!(resolved, on_path.to_string_lossy());
}

/// A file whose only execute bit belongs to "other" is not executable by
/// its owner, so it cannot shadow the runnable executable in a later
/// directory, exactly as normal PATH lookup skips it.
#[cfg(unix)]
#[test]
fn executable_resolution_skips_an_other_only_execute_bit() {
    use std::os::unix::fs::PermissionsExt;

    if rustix::process::geteuid().is_root() {
        // Root may execute a regular file when any execute bit is set, so the
        // owner-vs-other distinction this case relies on does not hold; the
        // portable workspace validation can run as root in containers.
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

    let resolved = resolved_executable("codex-other-only", shadowing.path(), &search);

    assert_eq!(resolved, on_path.to_string_lossy());
}

/// A relative PATH element is anchored to the working directory, so the
/// returned candidate satisfies the adapter's absolute-path requirement.
#[cfg(unix)]
#[test]
fn executable_resolution_anchors_a_relative_search_entry() {
    let working = tempfile::tempdir().expect("resolution fixture directory is created");
    std::fs::create_dir(working.path().join("bin")).expect("the relative bin directory is created");
    let on_path = executable_fixture(&working.path().join("bin"), "codex-rel");

    let resolved = resolved_executable("codex-rel", working.path(), std::ffi::OsStr::new("bin"));

    assert_eq!(resolved, on_path.to_string_lossy());
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

    let resolved = resolved_executable("codex-shadowed", shadowing.path(), &search);

    assert_eq!(resolved, on_path.to_string_lossy());
}

#[test]
#[should_panic(expected = "was not found on PATH")]
fn executable_resolution_panics_for_a_missing_bare_command() {
    let empty = tempfile::tempdir().expect("resolution fixture directory is created");
    let search = std::env::join_paths([empty.path()]).expect("the fixture search path joins");

    let _ = resolved_executable("codex-missing", empty.path(), &search);
}
