use signalbox_application::ToolExecutorEvidence;
use signalbox_model_runtime::CredentialValue;

use super::{egress::*, redaction::*, result::*};

pub(super) const SYNTHETIC_KEY: &str = "fixture-search-key";

pub(super) const FIXTURE_RESULT_TITLE: &str = "Synthetic result";

pub(super) const FIXTURE_RESULT_URL: &str = "https://example.com/result";

pub(super) const FIXTURE_SCHEME_RELATIVE_UNICODE_RESULT_URL: &str = "https:é";

pub(super) const FIXTURE_CANONICAL_UNICODE_RESULT_URL: &str = "https://xn--9ca/";

pub(super) const FIXTURE_RESULT_SNIPPET: &str = "Synthetic recorded snippet";

pub(super) const FIXTURE_UNKNOWN_NAMED_REFERENCE_TITLE: &str = "Synthetic R&D; result";

pub(super) const FIXTURE_UNSUPPORTED_VALID_NAMED_REFERENCE_SNIPPET: &str =
    "Synthetic &copy; snippet";

pub(super) const FIXTURE_COMMON_NAMED_REFERENCES_SNIPPET: &str =
    "Synthetic &copy; &mdash; &ndash; &hellip; &rsquo; snippet";

pub(super) const FIXTURE_ESCAPED_COMMON_NAMED_REFERENCES_SNIPPET: &str =
    "Synthetic &amp;copy; &amp;mdash; &amp;ndash; &amp;hellip; &amp;rsquo; snippet";

pub(super) const SUPPORTED_NAMED_NONBREAKING_SPACE_REFERENCE: &str = "nbsp";

pub(super) const SUPPORTED_NAMED_NONBREAKING_SPACE_VALUE: &str = "\u{a0}";

pub(super) const SIGNED_JSON_UNICODE_ESCAPE: &str = r"\u+12a";

pub(super) const SIGNED_HTML_NUMERIC_REFERENCE: &str = "#+65";

pub(super) const FIXTURE_WHITESPACE_TITLE: &str = " \t\n";

pub(super) const FIXTURE_NORMALIZED_RESULT_URL: &str = "https://exa\nmple.com/result";

pub(super) const FIXTURE_ORIGIN_ONLY_RESULT_URL: &str = "https://example.com";

pub(super) const FIXTURE_CANONICAL_ORIGIN_RESULT_URL: &str = "https://example.com/";

pub(super) const FIXTURE_USERINFO_RESULT_URL: &str =
    "https://fixture-user:fixture-password@example.com/result";

pub(super) const FIXTURE_QUERY_RESULT_URL: &str = "https://example.com/result?fixture=unapproved";

pub(super) const FIXTURE_FRAGMENT_RESULT_URL: &str = "https://example.com/result#fixture-fragment";

pub(super) const FIXTURE_MARKUP_SNIPPET: &str = "Synthetic <b>result & details</b>";

pub(super) const FIXTURE_ESCAPED_MARKUP_SNIPPET: &str =
    "Synthetic &lt;b&gt;result &amp; details&lt;/b&gt;";

pub(super) const FIXTURE_MARKUP_TITLE: &str = "Synthetic <result> & \"title\"";

pub(super) const FIXTURE_ESCAPED_MARKUP_TITLE: &str =
    "Synthetic &lt;result&gt; &amp; &quot;title&quot;";

pub(super) const FIXTURE_LITERAL_ENTITY_TITLE: &str = "&lt;script&gt;";

pub(super) const ENTITY_ESCAPE_COLLISION_KEY: &str = "amp;";

pub(super) const FIXTURE_PROVIDER_ERROR_DETAIL: &str = "Synthetic <rejection> & detail";

pub(super) const FIXTURE_ESCAPED_PROVIDER_ERROR_DETAIL: &str =
    "Synthetic &lt;rejection&gt; &amp; detail";

pub(super) const SUCCESS_PAYLOAD_COLLISION_KEY: &str = "results";

pub(super) const URL_SCHEME_COLLISION_KEY: &str = "https";

pub(super) const URL_SCHEME_CASE_COLLISION_KEY: &str = "HTTPS";

pub(super) const URL_ENCODED_COLLISION_KEY: &str = "secret/key";

pub(super) const URL_ENCODED_COLLISION_VALUE: &str = "secret%2Fkey";

pub(super) const URL_UNICODE_HOST_COLLISION_KEY: &str = "BÜCHER";

pub(super) const URL_DECOMPOSED_UNICODE_HOST_COLLISION_KEY: &str = "BU\u{0308}CHER";

pub(super) const HTML_ENTITY_COLLISION_KEY: &str = "abc&def";

pub(super) const HTML_ENTITY_COLLISION_VALUE: &str = "abc&amp;def";

pub(super) const UNSUPPORTED_NAMED_ENTITY_COLLISION_KEY: &str = "*";

pub(super) const UNSUPPORTED_NAMED_ENTITY_COLLISION_VALUE: &str = "&ast;";

pub(super) const UNSUPPORTED_NAMED_ENTITY_ESCAPED_VALUE: &str = "&amp;ast;";

pub(super) const HTML_NUMERIC_C1_COLLISION_KEY: &str = "€";

pub(super) const HTML_NUMERIC_C1_COLLISION_VALUE: &str = "&#x80;";

pub(super) const SEMICOLONLESS_NUMERIC_HTML_COLLISION_KEY: &str = ">";

pub(super) const SEMICOLONLESS_NUMERIC_HTML_COLLISION_VALUE: &str = "&#62";

pub(super) const SEMICOLONLESS_NAMED_HTML_COLLISION_KEY: &str = "<";

pub(super) const SEMICOLONLESS_NAMED_HTML_COLLISION_VALUE: &str = "&lt";

pub(super) const PREFIXED_LEGACY_NAMED_HTML_COLLISION_VALUE: &str = "&GTfoo;";

pub(super) const PREFIXED_LEGACY_NAMED_HTML_COLLISION_KEY: &str = ">";

pub(super) const NESTED_NAMED_HTML_COLLISION_VALUE: &str = "&junk&lt;";

pub(super) const RUST_DEBUG_UNICODE_COLLISION_KEY: &str = r"\u{85}";

pub(super) const RUST_DEBUG_UNICODE_COLLISION_VALUE: &str = "\u{85}";

pub(super) const OVER_WINDOW_NUMERIC_HTML_COLLISION_KEY: &str = "ZXQ";

pub(super) const HTML_NESTED_ENTITY_COLLISION_VALUE: &str = "abc&amp;amp;def";

pub(super) const FORM_HTML_COLLISION_VALUE: &str = "abc%26amp%3Bdef";

pub(super) const REVERSE_ENCODED_COLLISION_KEY: &str = "abc%26def";

pub(super) const REVERSE_ENCODED_COLLISION_VALUE: &str = "abc&def";

pub(super) const JSON_UNICODE_COLLISION_KEY: &str = "abc";

pub(super) const JSON_UNICODE_COLLISION_VALUE: &str = r"\u0061\u0062\u0063";

pub(super) const JSON_SOLIDUS_COLLISION_KEY: &str = r"abc\/def";

pub(super) const JSON_SOLIDUS_COLLISION_VALUE: &str = "abc/def";

pub(super) const UNICODE_FULL_FOLD_COLLISION_KEY: &str = "STRASSE";

pub(super) const UNICODE_FULL_FOLD_COLLISION_VALUE: &str = "Straße";

pub(super) const UNICODE_COMBINING_MARK_COLLISION_KEY: &str = "\u{0301}x";

pub(super) const UNICODE_COMBINING_MARK_COLLISION_VALUE: &str = "éx";

pub(super) const PROVIDER_REJECTION_STATUS: u16 = 429;

pub(super) const PROVIDER_ERROR_DEBUG_COLLISION_KEY: &str = "WebSearchProviderError";

pub(super) const PROVIDER_VARIANT_DEBUG_COLLISION_KEY: &str = "Brave";

pub(super) const EGRESS_POLICY_DEBUG_COLLISION_KEY: &str = "WebSearchEgressPolicy";

pub(super) const CONFIGURATION_DEBUG_COLLISION_KEY: &str = "WebSearchConfiguration";

pub(super) const PROVIDER_PLACEHOLDER_DEBUG_COLLISION_KEY: &str = "[provider-controlled]";

pub(super) const COMPLETED_EVIDENCE_DEBUG_COLLISION_KEY: &str = "CompletedText";

pub(super) const KNOWN_FAILURE_EVIDENCE_DEBUG_COLLISION_KEY: &str = "KnownFailed";

pub(super) const SUCCESS_RESULT_DEBUG_COLLISION_KEY: &str = "Ok";

pub(super) const SUCCESS_RESULT_BOUNDARY_DEBUG_COLLISION_KEY: &str = "Ok(C";

pub(super) const POPULATED_SUCCESS_RESULT_SUFFIX_COLLISION_KEY: &str = "}\"))";

pub(super) const FIXTURE_POPULATED_FAILURE_DETAIL: &str = "synthetic failure evidence";

pub(super) const POPULATED_FAILURE_RESULT_SUFFIX_COLLISION_KEY: &str = "e\")) })";

pub(super) const ERROR_RESULT_BOUNDARY_DEBUG_COLLISION_KEY: &str = "Err(E";

pub(super) const KNOWN_FAILURE_RESULT_BOUNDARY_DEBUG_COLLISION_KEY: &str = "Ok(K";

pub(super) const POPULATED_RESPONSE_OPTION_DEBUG_COLLISION_KEY: &str = "Some(W";

pub(super) const POPULATED_PARTIAL_RESPONSE_OPTION_DEBUG_COLLISION_KEY: &str =
    "Some(WebSearchResponse { completeness: M";

pub(super) const POPULATED_PROVIDER_ERROR_OPTION_DEBUG_COLLISION_KEY: &str = "Some(WebSearchP";

pub(super) const POPULATED_EVIDENCE_OPTION_DEBUG_COLLISION_KEY: &str = "Some";

pub(super) const POPULATED_EVIDENCE_DETAIL_DEBUG_COLLISION_KEY: &str = "ToolExecutionErrorDetail";

pub(super) const POPULATED_FAILURE_RESULT_DEBUG_COLLISION_KEY: &str = "Ok(KnownFailed { detail: S";

pub(super) const POPULATED_SUCCESS_DEBUG_ESCAPE_COLLISION_KEY: &str = "\\";

pub(super) const REMOVED_DIAGNOSTIC_PROBE_WORD: &str = "diagnostic";

pub(super) const REMOVED_DETAIL_PROBE_WORD: &str = "probe";

pub(super) const DEBUG_RESULT_COUNT_COLLISION_COUNT: usize = 1;

pub(super) const FIXTURE_LEGACY_PERCENT_ENCODED_RESULT_URL: &str = "https://example.com/caf%E9";

pub(super) const FIXTURE_UNPARSED_PROVIDER_ERROR: &str =
    "synthetic provider-private response bytes";

pub(super) fn configuration() -> WebSearchConfiguration {
    WebSearchConfiguration::new(WebSearchProvider::Brave)
}

pub(super) fn result(title: impl Into<String>) -> WebSearchResult {
    WebSearchResult::try_new(WebSearchResultFields {
        title: title.into(),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("fixture result is admitted")
}

pub(super) fn response_with_result_count(count: usize) -> WebSearchResponse {
    let results = (0..count)
        .map(|index| result(format!("recorded result {index}")))
        .collect();
    WebSearchResponse::new(results, WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted")
}

pub(super) fn scrubber() -> CredentialScrubber {
    CredentialScrubber::try_new(&CredentialValue::new(SYNTHETIC_KEY.as_bytes().to_vec()))
        .expect("fixture credential is usable")
}

pub(super) fn debug_result_count_collision_key() -> String {
    DEBUG_RESULT_COUNT_COLLISION_COUNT.to_string()
}

pub(super) fn fixture_result_url_with_path_segment(segment: &str) -> String {
    format!("{FIXTURE_ORIGIN_ONLY_RESULT_URL}/{segment}")
}

#[track_caller]
pub(super) fn completed_text(evidence: ToolExecutorEvidence) -> String {
    match evidence {
        ToolExecutorEvidence::CompletedText(content) => content,
        other => panic!("expected completed text, got {other:?}"),
    }
}

pub(super) fn html_multibyte_boundary_reflection() -> String {
    format!("{HTML_ENTITY_COLLISION_VALUE}{}é", "x".repeat(55))
}

pub(super) fn distant_html_reference_terminator() -> String {
    format!("&#{};", "1".repeat(MAX_PROVIDER_RESPONSE_BYTES - 3))
}

pub(super) fn over_window_numeric_html_reflection() -> String {
    let leading_zeroes = "0".repeat(64);
    format!("&#{leading_zeroes}90;&#{leading_zeroes}88;&#{leading_zeroes}81;")
}

pub(super) fn content_over_tool_result_bound() -> String {
    "x".repeat(
        MAX_RETURNED_RESULTS
            * (MAX_RESULT_TITLE_BYTES + MAX_RESULT_SNIPPET_BYTES)
            * "\\u0001".len(),
    )
}

pub(super) fn oversized_entity_escaped_error_detail() -> String {
    "&amp;é".repeat(MAX_PROVIDER_RESPONSE_BYTES / 2)
}
