//! Exhaustive native error classification (`docs/spec/runtime-substrate.md`).
//!
//! The runtime-substrate spec requires each real adapter to define an
//! exhaustive, mutually exclusive mapping of its provider-native terminal
//! statuses and payloads. Chat Completions carries the specific condition in
//! `error.code` or `error.type` while the HTTP status carries the category,
//! so envelope classification treats HTTP 401 as credential rejection
//! outright, then consults a recognized code, then a recognized type, then the
//! status table. The mapping uses first-match arms, so
//! exhaustiveness and mutual exclusivity hold by construction. Unknown
//! combinations land in [`ProviderErrorKind::Unrecognized`] with the native
//! material retained on the evidence rather than being guessed at.

use signalbox_model_runtime::ProviderErrorKind;

/// Classifies a definitive error response from its native code and HTTP
/// status.
pub(crate) fn classify_error(status: u16, code: Option<&str>) -> ProviderErrorKind {
    if status == 401 {
        return ProviderErrorKind::CredentialRejected;
    }
    match code {
        Some("invalid_api_key") => ProviderErrorKind::CredentialRejected,
        Some("invalid_request_error") => ProviderErrorKind::InvalidRequest,
        Some("model_not_found") => ProviderErrorKind::TargetNotFound,
        Some("insufficient_quota") => ProviderErrorKind::QuotaExhausted,
        Some("context_length_exceeded") => ProviderErrorKind::RequestTooLarge,
        Some("rate_limit_exceeded" | "rate_limit_error") => ProviderErrorKind::RateLimited,
        Some("server_error" | "internal_server_error") => ProviderErrorKind::ProviderInternal,
        _ => match status {
            400 => ProviderErrorKind::InvalidRequest,
            401 => ProviderErrorKind::CredentialRejected,
            403 => ProviderErrorKind::PermissionDenied,
            404 => ProviderErrorKind::TargetNotFound,
            413 => ProviderErrorKind::RequestTooLarge,
            429 => ProviderErrorKind::RateLimited,
            500 => ProviderErrorKind::ProviderInternal,
            503 => ProviderErrorKind::Overloaded,
            _ => ProviderErrorKind::Unrecognized,
        },
    }
}

/// Classifies an envelope whose native code and type are distinct facts.
///
/// A recognized code takes precedence, then a recognized type, then the HTTP
/// status. An unknown non-null code must not hide a useful type token.
pub(crate) fn classify_error_envelope(
    status: u16,
    code: Option<&str>,
    error_type: Option<&str>,
) -> ProviderErrorKind {
    if status == 401 {
        return ProviderErrorKind::CredentialRejected;
    }
    let code_kind = classify_error(0, code);
    if code_kind != ProviderErrorKind::Unrecognized {
        return code_kind;
    }
    let type_kind = classify_error(0, error_type);
    if type_kind != ProviderErrorKind::Unrecognized {
        return type_kind;
    }
    classify_error(status, None)
}

#[cfg(test)]
mod tests {
    use expect_test::expect;
    use signalbox_expect_table::table;
    use signalbox_model_runtime::ProviderErrorKind;

    use super::{classify_error, classify_error_envelope};

    #[derive(Debug)]
    #[allow(
        dead_code,
        reason = "the table renderer reads every field through the Debug derive"
    )]
    struct ClassificationRow {
        status: u16,
        code: &'static str,
        kind: String,
    }

    /// Renders one classification row per `(status, code)` pair, in the
    /// given order; `"-"` means no native code was carried.
    fn classification_rows(cases: &[(u16, &'static str)]) -> Vec<ClassificationRow> {
        cases
            .iter()
            .map(|(status, code)| ClassificationRow {
                status: *status,
                code,
                kind: format!(
                    "{:?}",
                    classify_error(*status, (*code != "-").then_some(code))
                ),
            })
            .collect()
    }

    #[test]
    fn credential_rejection_is_typed_not_string_matched() {
        // `docs/spec/runtime-substrate.md`: provider-side credential
        // rejection must stay distinguishable without reading rendered
        // messages.
        assert_eq!(
            classify_error(401, Some("invalid_api_key")),
            ProviderErrorKind::CredentialRejected
        );
        assert_eq!(
            classify_error(401, None),
            ProviderErrorKind::CredentialRejected
        );
        assert_eq!(
            classify_error(401, Some("insufficient_quota")),
            ProviderErrorKind::CredentialRejected
        );
    }

    #[test]
    fn documented_codes_take_precedence_and_statuses_are_the_fallback() {
        let rows = classification_rows(&[
            (401, "invalid_api_key"),
            (0, "invalid_request_error"),
            (404, "model_not_found"),
            (429, "insufficient_quota"),
            (400, "context_length_exceeded"),
            (0, "rate_limit_exceeded"),
            (0, "rate_limit_error"),
            (0, "server_error"),
            (0, "internal_server_error"),
            (400, "-"),
            (401, "-"),
            (403, "-"),
            (404, "-"),
            (413, "-"),
            (429, "-"),
            (500, "-"),
            (503, "-"),
            (418, "-"),
            (429, "brand_new_code"),
        ]);

        expect![[r#"
            ┌────────┬─────────────────────────┬────────────────────┐
            │ status │ code                    │ kind               │
            ├────────┼─────────────────────────┼────────────────────┤
            │    401 │ invalid_api_key         │ CredentialRejected │
            │      0 │ invalid_request_error   │ InvalidRequest     │
            │    404 │ model_not_found         │ TargetNotFound     │
            │    429 │ insufficient_quota      │ QuotaExhausted     │
            │    400 │ context_length_exceeded │ RequestTooLarge    │
            │      0 │ rate_limit_exceeded     │ RateLimited        │
            │      0 │ rate_limit_error        │ RateLimited        │
            │      0 │ server_error            │ ProviderInternal   │
            │      0 │ internal_server_error   │ ProviderInternal   │
            │    400 │ -                       │ InvalidRequest     │
            │    401 │ -                       │ CredentialRejected │
            │    403 │ -                       │ PermissionDenied   │
            │    404 │ -                       │ TargetNotFound     │
            │    413 │ -                       │ RequestTooLarge    │
            │    429 │ -                       │ RateLimited        │
            │    500 │ -                       │ ProviderInternal   │
            │    503 │ -                       │ Overloaded         │
            │    418 │ -                       │ Unrecognized       │
            │    429 │ brand_new_code          │ RateLimited        │
            └────────┴─────────────────────────┴────────────────────┘
        "#]]
        .assert_eq(&table(rows));
    }

    #[test]
    fn unknown_code_falls_through_to_a_recognized_error_type() {
        assert_eq!(
            classify_error_envelope(429, Some("new_gateway_code"), Some("insufficient_quota")),
            ProviderErrorKind::QuotaExhausted
        );
    }

    #[test]
    fn statusless_invalid_request_type_is_typed() {
        assert_eq!(
            classify_error_envelope(0, None, Some("invalid_request_error")),
            ProviderErrorKind::InvalidRequest
        );
    }
}
