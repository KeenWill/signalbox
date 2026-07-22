//! Exhaustive native error classification (ADR-0043).
//!
//! ADR-0043 requires each real adapter to define an exhaustive, mutually
//! exclusive mapping of its provider-native terminal statuses and payloads.
//! Chat Completions carries the specific condition in `error.code` while the
//! HTTP status carries the category, so classification consults the
//! documented specific codes first and falls back to the status table; the
//! whole mapping is one function of first-match arms, so exhaustiveness and
//! mutual exclusivity hold by construction. Unknown combinations land in
//! [`ProviderErrorKind::Unrecognized`] with the native material retained on
//! the evidence rather than being guessed at.

use signalbox_model_runtime::ProviderErrorKind;

/// Classifies a definitive error response from its native code and HTTP
/// status.
pub(crate) fn classify_error(status: u16, code: Option<&str>) -> ProviderErrorKind {
    if status == 401 {
        return ProviderErrorKind::CredentialRejected;
    }
    match code {
        Some("invalid_api_key") => ProviderErrorKind::CredentialRejected,
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

#[cfg(test)]
mod tests {
    use expect_test::expect;
    use signalbox_expect_table::table;
    use signalbox_model_runtime::ProviderErrorKind;

    use super::classify_error;

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
        // ADR-0017/ADR-0043: provider-side credential rejection must stay
        // distinguishable without reading rendered messages.
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
}
