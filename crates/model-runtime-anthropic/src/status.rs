//! Exhaustive native error classification (`docs/spec/runtime-substrate.md`).
//!
//! The runtime-substrate spec requires each real adapter to define an
//! exhaustive, mutually exclusive mapping of its provider-native terminal
//! statuses and payloads. Both mappings below are single `match` expressions,
//! so exhaustiveness and mutual exclusivity hold by construction; an unknown
//! token or status lands in [`ProviderErrorKind::Unrecognized`] with the
//! native material retained on the evidence rather than being guessed at.

use signalbox_model_runtime::ProviderErrorKind;

/// Classifies the error envelope's native `error.type` token — the primary
/// mapping, used whenever the error body parses.
pub(crate) fn classify_error_token(token: &str) -> ProviderErrorKind {
    match token {
        "authentication_error" => ProviderErrorKind::CredentialRejected,
        "permission_error" => ProviderErrorKind::PermissionDenied,
        "invalid_request_error" => ProviderErrorKind::InvalidRequest,
        "not_found_error" => ProviderErrorKind::TargetNotFound,
        "request_too_large" => ProviderErrorKind::RequestTooLarge,
        "rate_limit_error" => ProviderErrorKind::RateLimited,
        "overloaded_error" => ProviderErrorKind::Overloaded,
        "api_error" => ProviderErrorKind::ProviderInternal,
        _ => ProviderErrorKind::Unrecognized,
    }
}

/// Classifies a definitive error response by HTTP status alone — the
/// fallback when the error body does not parse as the documented envelope.
/// The response is still a complete terminal error status, so it stays
/// definitive known-failure evidence per the runtime-substrate spec, with
/// the raw body retained.
pub(crate) fn classify_error_status(status: u16) -> ProviderErrorKind {
    match status {
        400 => ProviderErrorKind::InvalidRequest,
        401 => ProviderErrorKind::CredentialRejected,
        403 => ProviderErrorKind::PermissionDenied,
        404 => ProviderErrorKind::TargetNotFound,
        413 => ProviderErrorKind::RequestTooLarge,
        429 => ProviderErrorKind::RateLimited,
        500 => ProviderErrorKind::ProviderInternal,
        529 => ProviderErrorKind::Overloaded,
        _ => ProviderErrorKind::Unrecognized,
    }
}

/// Combines the native token with the authoritative HTTP status.
///
/// A credential-rejection status has precedence over a contradictory body so
/// credential remediation cannot be displaced by provider-controlled text.
/// Otherwise a recognized native token refines the status classification.
pub(crate) fn classify_error(status: u16, token: Option<&str>) -> ProviderErrorKind {
    let status_kind = classify_error_status(status);
    if status_kind == ProviderErrorKind::CredentialRejected {
        return status_kind;
    }
    match token.map(classify_error_token) {
        Some(ProviderErrorKind::Unrecognized) | None => status_kind,
        Some(kind) => kind,
    }
}

#[cfg(test)]
mod tests {
    use expect_test::expect;
    use signalbox_expect_table::table;

    use super::{classify_error, classify_error_status, classify_error_token};

    #[derive(Debug)]
    #[allow(
        dead_code,
        reason = "the table renderer reads every field through the Debug derive"
    )]
    struct TokenRow {
        token: &'static str,
        kind: String,
    }

    #[derive(Debug)]
    #[allow(
        dead_code,
        reason = "the table renderer reads every field through the Debug derive"
    )]
    struct StatusRow {
        status: u16,
        kind: String,
    }

    /// Renders one classification row per token, in the given order.
    fn token_rows(tokens: &[&'static str]) -> Vec<TokenRow> {
        tokens
            .iter()
            .map(|token| TokenRow {
                token,
                kind: format!("{:?}", classify_error_token(token)),
            })
            .collect()
    }

    /// Renders one classification row per status, in the given order.
    fn status_rows(statuses: &[u16]) -> Vec<StatusRow> {
        statuses
            .iter()
            .map(|status| StatusRow {
                status: *status,
                kind: format!("{:?}", classify_error_status(*status)),
            })
            .collect()
    }

    #[test]
    fn credential_rejection_is_typed_not_string_matched() {
        // `docs/spec/runtime-substrate.md`: provider-side credential
        // rejection must stay distinguishable without reading rendered
        // messages.
        assert_eq!(
            classify_error_token("authentication_error"),
            signalbox_model_runtime::ProviderErrorKind::CredentialRejected
        );
        assert_eq!(
            classify_error_status(401),
            signalbox_model_runtime::ProviderErrorKind::CredentialRejected
        );
        assert_eq!(
            classify_error(401, Some("rate_limit_error")),
            signalbox_model_runtime::ProviderErrorKind::CredentialRejected
        );
    }

    #[test]
    fn every_documented_error_token_classifies_and_unknown_stays_unrecognized() {
        let rows = token_rows(&[
            "authentication_error",
            "permission_error",
            "invalid_request_error",
            "not_found_error",
            "request_too_large",
            "rate_limit_error",
            "overloaded_error",
            "api_error",
            "billing_error_from_the_future",
        ]);

        expect![[r#"
            ┌───────────────────────────────┬────────────────────┐
            │ token                         │ kind               │
            ├───────────────────────────────┼────────────────────┤
            │ authentication_error          │ CredentialRejected │
            │ permission_error              │ PermissionDenied   │
            │ invalid_request_error         │ InvalidRequest     │
            │ not_found_error               │ TargetNotFound     │
            │ request_too_large             │ RequestTooLarge    │
            │ rate_limit_error              │ RateLimited        │
            │ overloaded_error              │ Overloaded         │
            │ api_error                     │ ProviderInternal   │
            │ billing_error_from_the_future │ Unrecognized       │
            └───────────────────────────────┴────────────────────┘
        "#]]
        .assert_eq(&table(rows));
    }

    #[test]
    fn every_documented_error_status_classifies_and_unknown_stays_unrecognized() {
        let rows = status_rows(&[400, 401, 403, 404, 413, 429, 500, 529, 503]);

        expect![[r#"
            ┌────────┬────────────────────┐
            │ status │ kind               │
            ├────────┼────────────────────┤
            │    400 │ InvalidRequest     │
            │    401 │ CredentialRejected │
            │    403 │ PermissionDenied   │
            │    404 │ TargetNotFound     │
            │    413 │ RequestTooLarge    │
            │    429 │ RateLimited        │
            │    500 │ ProviderInternal   │
            │    529 │ Overloaded         │
            │    503 │ Unrecognized       │
            └────────┴────────────────────┘
        "#]]
        .assert_eq(&table(rows));
    }
}
