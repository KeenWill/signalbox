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
    PreparationOutcome, ProviderRequestId, RefusalEvidence, RequestedTarget, ResolvedTarget,
    TerminalEvidence, TokenUsage,
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
