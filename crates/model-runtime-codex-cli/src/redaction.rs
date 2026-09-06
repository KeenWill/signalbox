//! Adapter-path enforcement for the shared CLI redactor.

use signalbox_model_runtime::{REDACTED, redact_text};

fn synthetic_credential_assignment() -> &'static str {
    "api_key=SYNTHETIC-SECRET-CODEX-ADAPTER"
}

/// the Codex adapter uses the shared CLI credential redactor.
#[test]
fn shared_cli_redactor_replaces_a_synthetic_credential() {
    assert_eq!(
        redact_text(synthetic_credential_assignment()),
        format!("api_key={REDACTED}")
    );
}
