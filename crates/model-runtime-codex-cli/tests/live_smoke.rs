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
//! adapter passes, and its final agent message still decodes as the adapter's
//! response envelope. It deliberately asserts nothing about answer quality.
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
    AssistantPart, CancellationSignal, CompletionEvidence, ConversationMessage,
    CredentialReference, DeliveryMode, ModelOperation, ModelRuntime, ModelSettings,
    PreparationOutcome, RequestedTarget, ResolvedTarget, TerminalEvidence,
};
use signalbox_model_runtime_codex_cli::{
    CodexCliConfig, CodexCliRuntime, SUPPORTED_CODEX_CLI_VERSION,
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

    // `CodexCliPreparedRequest` implements no diagnostic formatting on
    // purpose, so each non-prepared outcome reports itself.
    let prepared = match runtime
        .prepare(operation, CancellationSignal::never())
        .await
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        PreparationOutcome::Cancelled { .. } => panic!("smoke preparation was not cancelled"),
        PreparationOutcome::Failed { failure, .. } => {
            panic!("smoke preparation failed: {failure:?}")
        }
        PreparationOutcome::Defect { defect, .. } => {
            panic!("smoke preparation found a defect: {defect:?}")
        }
    };
    let mut observations = Vec::new();
    let report = runtime
        .execute(prepared, &mut observations, CancellationSignal::never())
        .await;

    let completed = match report.evidence {
        TerminalEvidence::Completed(completed) => completed,
        // Adapter-produced evidence is already credential-shape redacted, so
        // printing it here cannot surface credential material.
        other => panic!("the pinned Codex CLI did not complete the exchange: {other:?}"),
    };
    assert_protocol_surfaces(&completed, &model);
}

/// Fails closed: an unreadable, unparsable, or mismatched version is a smoke
/// failure, never a skip. A skip here would quietly retire the only check that
/// binds this evidence to a specific executable.
async fn assert_pinned_version(executable: &str) {
    let output = tokio::process::Command::new(executable)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Discarded rather than reported: a version probe has no need to
        // surface provider-controlled diagnostics, and this text does not pass
        // through the adapter's redaction.
        .stderr(Stdio::null())
        .output()
        .await
        .unwrap_or_else(|error| panic!("`{executable} --version` could not be spawned: {error}"));
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

/// The event-protocol surfaces a CLI version bump can silently move.
fn assert_protocol_surfaces(completed: &CompletionEvidence, model: &str) {
    assert!(
        completed.exchange.provider_request_id.is_some(),
        "no thread id reached the exchange facts, so `thread.started` no longer \
         parses for model {model}"
    );
    assert!(
        completed
            .usage
            .input_tokens
            .is_some_and(|tokens| tokens > 0)
            && completed.usage.output_tokens.is_some(),
        "`turn.completed` no longer reports the usage counters the adapter reads"
    );
    let text = completed
        .content
        .iter()
        .filter_map(|part| match part {
            AssistantPart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        !text.trim().is_empty(),
        "the final agent message decoded as the response envelope but carried no \
         text; the envelope contract and the CLI's structured output have drifted"
    );
}

fn variable_or(name: &str, fallback: &str) -> String {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => fallback.to_string(),
    }
}

/// The adapter accepts only an absolute executable path, so the bare-command
/// local default is resolved through `PATH` exactly once, here. CI is
/// unaffected: the workflow always passes the absolute path of the binary
/// installed from the pin manifest.
fn absolute_executable(executable: &str) -> String {
    let path = std::path::Path::new(executable);
    if path.is_absolute() {
        return executable.to_string();
    }
    if path.components().count() > 1 {
        return std::env::current_dir()
            .expect("the smoke process has a working directory")
            .join(path)
            .to_string_lossy()
            .into_owned();
    }
    let search = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&search)
        .map(|directory| directory.join(executable))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| {
            panic!(
                "`{executable}` was not found on PATH; set {EXECUTABLE_VARIABLE} \
                 to an absolute executable path"
            )
        })
        .to_string_lossy()
        .into_owned()
}
