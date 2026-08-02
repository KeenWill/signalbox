//! Live Brave Search smoke.
//!
//! Ignored by default and skipped with a printed reason when no credential is
//! present, so the shared report-only job stays quiet until the deployment
//! secret exists. One run performs exactly one provider request through the
//! production transport. The provider page size is fixed by the crate at
//! `MAX_PROVIDER_RESULTS` and is not a request parameter, so a run costs one
//! request and nothing else.
//!
//! What this proves beyond the fixture suite: a real Brave response, not a
//! synthetic one, still satisfies the bounded decoding the executor depends on.

use std::{env, future::Future, path::PathBuf};

use signalbox_model_runtime::CredentialValue;

use super::{egress::*, request::*, transport::*};

/// Credential source used by the automated caller.
const CREDENTIAL_ENVIRONMENT: &str = "BRAVE_API_KEY";

/// Deployment credential file, beside the `anthropic-api-key` and
/// `github-token` files this deployment already keeps.
const CREDENTIAL_FILE: &str = ".config/signalbox/brave-api-key";

/// A broad, stable query: the smoke proves decoding, not retrieval quality.
const SMOKE_QUERY: &str = "rust programming language";

/// Line termination a writing tool appends is how a credential file ends
/// rather than part of the secret, matching how the daemon narrows the same
/// files.
const CREDENTIAL_LINE_TERMINATORS: [u8; 2] = *b"\n\r";

/// Runs one smoke against the resolved credential, or reports which sources
/// were empty and skips.
///
/// The availability branch lives here rather than in the test body, mirroring
/// the `with_github_token` helper the GitHub live smokes use.
async fn with_smoke_credential<Smoke, SmokeFuture>(smoke: Smoke)
where
    Smoke: FnOnce(CredentialValue) -> SmokeFuture,
    SmokeFuture: Future<Output = ()>,
{
    run_with_resolved_credential(smoke_credential(), smoke).await;
}

/// Calls `smoke` when `credential` is present, otherwise reports which
/// sources were empty and returns without calling it.
///
/// Split from `with_smoke_credential` so the skip-versus-callback branch is
/// exercised by an ordinary test: `smoke_credential`'s real environment and
/// filesystem lookups stay untested here, but a regression in this branch
/// (for example, always skipping, or skipping when a credential is present)
/// is caught without one.
async fn run_with_resolved_credential<Smoke, SmokeFuture>(
    credential: Option<CredentialValue>,
    smoke: Smoke,
) where
    Smoke: FnOnce(CredentialValue) -> SmokeFuture,
    SmokeFuture: Future<Output = ()>,
{
    let Some(credential) = credential else {
        eprintln!(
            "skipping live web search smoke: neither {CREDENTIAL_ENVIRONMENT} nor ~/{CREDENTIAL_FILE} holds a credential"
        );
        return;
    };
    smoke(credential).await;
}

/// Resolves the smoke credential from the environment, then from the
/// deployment file, and returns nothing when neither source holds one.
fn smoke_credential() -> Option<CredentialValue> {
    let environment_value = env::var_os(CREDENTIAL_ENVIRONMENT)
        .and_then(|value| value.into_string().ok())
        .filter(|value| !value.is_empty());
    if let Some(value) = environment_value {
        return Some(CredentialValue::new(value.into_bytes()));
    }
    let file_bytes = std::fs::read(credential_file_path()?).ok()?;
    let value = credential_bytes(&file_bytes).to_vec();
    (!value.is_empty()).then(|| CredentialValue::new(value))
}

fn credential_file_path() -> Option<PathBuf> {
    let mut path = PathBuf::from(env::var_os("HOME")?);
    path.push(CREDENTIAL_FILE);
    Some(path)
}

fn credential_bytes(file_bytes: &[u8]) -> &[u8] {
    let end = file_bytes
        .iter()
        .rposition(|byte| !CREDENTIAL_LINE_TERMINATORS.contains(byte))
        .map_or(0, |last_value_byte| last_value_byte.saturating_add(1));
    &file_bytes[..end]
}

/// The terminator a writing tool appends is not part of the secret.
#[test]
fn credential_file_bytes_drop_trailing_line_termination() {
    assert_eq!(
        credential_bytes(b"fixture-search-key\r\n"),
        b"fixture-search-key"
    );
}

/// Every other byte is retained exactly, so a key that legitimately carries
/// interior or leading whitespace still reaches the provider unchanged.
#[test]
fn credential_file_bytes_retain_leading_and_interior_whitespace() {
    assert_eq!(
        credential_bytes(b" fixture search\tkey\n"),
        b" fixture search\tkey"
    );
}

/// A file holding nothing but terminators narrows to an empty value, which
/// resolution then treats as no credential at all.
#[test]
fn credential_file_bytes_narrow_a_terminator_only_file_to_empty() {
    assert_eq!(credential_bytes(b"\n\r\n"), b"");
}

/// No credential means the smoke never runs its callback: a regression that
/// always ran it would spend a live request from CI before any secret exists.
#[tokio::test]
async fn run_with_resolved_credential_skips_the_callback_when_absent() {
    let mut called = false;
    run_with_resolved_credential(None, |_credential| {
        called = true;
        async {}
    })
    .await;
    assert!(!called);
}

/// A present credential reaches the callback: a regression that always
/// skipped would report success while issuing no request.
#[tokio::test]
async fn run_with_resolved_credential_invokes_the_callback_when_present() {
    let mut called = false;
    run_with_resolved_credential(
        Some(CredentialValue::new(b"fixture-key".to_vec())),
        |_credential| {
            called = true;
            async {}
        },
    )
    .await;
    assert!(called);
}

/// One real Brave exchange decodes into a bounded page of results.
///
/// The credential is passed to the transport and never rendered, compared, or
/// reported: credential-safety of every diagnostic is covered exhaustively by
/// the offline suite, which needs no real secret to exercise it.
#[tokio::test]
#[ignore = "performs one real Brave Search request when a credential is present"]
async fn brave_search_decodes_a_bounded_live_page() {
    with_smoke_credential(|credential| async move {
        let mut transport = ReqwestWebSearchTransport::try_new(DEFAULT_EXCHANGE_TIMEOUT)
            .expect("production search client builds");
        let request = WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: String::from(SMOKE_QUERY),
        };

        transport
            .search(request, &credential)
            .await
            .into_result()
            .expect("Brave returns one complete bounded page");
    })
    .await;
}
