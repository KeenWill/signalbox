use signalbox_application::ToolExecutorEvidence;
use signalbox_model_runtime::CredentialValue;

use super::{egress::*, redaction::*, result::*};

pub(super) const SYNTHETIC_KEY: &str = "fixture-search-key";

pub(super) const FIXTURE_RESULT_TITLE: &str = "Synthetic result";

pub(super) const FIXTURE_RESULT_URL: &str = "https://example.com/result";

pub(super) const FIXTURE_IPV6_RESULT_URL: &str = "https://[2001:db8::1]/result";

pub(super) const FIXTURE_COMPRESSED_LOOPBACK_IPV6_RESULT_URL: &str = "https://[::1]/";

pub(super) const FIXTURE_MULTI_HEXTET_IPV6_RESULT_URL: &str = "https://[2001:db8:0:1::]/";

pub(super) const FIXTURE_IPV4_TAIL_IPV6_RESULT_URL: &str = "http://[::ffff:c0a8:1]/";

pub(super) const FIXTURE_LEGACY_IPV4_RESULT_URL: &str = "https://2130706433/result";

pub(super) const FIXTURE_CANONICAL_IPV4_COMPONENT_RESULT_URL: &str = "http://127.0.0.1/";

pub(super) const FIXTURE_HEX_AFFIX_IPV4_RESULT_URL: &str = "http://0x7f.0.0.1/";

pub(super) const FIXTURE_SEPARATOR_SPANNING_HEX_IPV4_RESULT_URL: &str = "http://1.0x7f.0.1/";

pub(super) const FIXTURE_PADDED_SEPARATOR_SPANNING_HEX_IPV4_RESULT_URL: &str =
    "http://1.0x007f.0.1/";

pub(super) const FIXTURE_PADDED_MULTI_COMPONENT_HEX_IPV4_RESULT_URL: &str = "http://0x007f.00.0.1/";

pub(super) const FIXTURE_TRAILING_DOT_IPV4_RESULT_URL: &str = "http://127.0.0.1./";

pub(super) const FIXTURE_OCTAL_AFFIX_IPV4_RESULT_URL: &str = "http://0177.0.0.1/";

pub(super) const FIXTURE_HEX_SUBSTRING_IPV4_RESULT_URL: &str = "http://0x7f000001/";

pub(super) const FIXTURE_OCTAL_SUBSTRING_IPV4_RESULT_URL: &str = "http://017700000001/";

pub(super) const FIXTURE_LEADING_ZERO_PORT_RESULT_URL: &str = "https://example.com:0800/";

pub(super) const FIXTURE_IPV6_LEADING_ZERO_PORT_RESULT_URL: &str = "https://[::1]:0800/";

pub(super) const FIXTURE_INTERNAL_ZERO_PORT_RESULT_URL: &str = "https://example.com:0400/";

pub(super) const FIXTURE_PORT_PATH_ZERO_RESULT_URL: &str = "https://example.com:0400/path";

pub(super) const FIXTURE_DEFAULT_PORT_PATH_RESULT_URL: &str = "http://example.com:80/path";

pub(super) const FIXTURE_DEFAULT_PORT_ONLY_RESULT_URL: &str = "http://example.com:80/";

pub(super) const FIXTURE_COMPOSED_PORT_PATH_RESULT_URL: &str = "https://example.com:0400/a/../path";

pub(super) const FIXTURE_EMPTY_PORT_RESULT_URL: &str = "http://example.com:/path";

pub(super) const FIXTURE_EMPTY_QUERY_RESULT_URL: &str = "https://example.com/xyzabc?";

pub(super) const FIXTURE_EMPTY_FRAGMENT_RESULT_URL: &str = "https://example.com/xyzabc#";

pub(super) const FIXTURE_USER_INFORMATION_BOUNDARY_RESULT_URL: &str = "http://usernam@example.com/";

pub(super) const FIXTURE_USER_INFORMATION_LEGACY_IPV4_RESULT_URL: &str = "http://nam@0x7f.0.0.1/";

pub(super) const FIXTURE_EMPTY_USER_INFORMATION_RESULT_URL: &str = "http://@example.com/path";

pub(super) const FIXTURE_SCHEME_RELATIVE_UNICODE_RESULT_URL: &str = "https:é";

pub(super) const FIXTURE_CANONICAL_UNICODE_RESULT_URL: &str = "https://xn--9ca/";

pub(super) const FIXTURE_MULTI_OCTET_IPV4_COMPONENT_RESULT_URL: &str = "http://127.0.1.0/";

pub(super) const FIXTURE_EMBEDDED_IPV6_HEXTET_RESULT_URL: &str = "https://[0db8::1]/";

pub(super) const FIXTURE_SEPARATOR_SPANNING_IPV6_HEXTET_RESULT_URL: &str = "https://[1:0db8::1]/";

pub(super) const FIXTURE_COMPRESSED_ZERO_HEXTET_RESULT_URL: &str =
    "https://[1:0000:0000:2:3:4:5:6]/";

pub(super) const FIXTURE_DOTTED_IPV4_TAIL_IPV6_RESULT_URL: &str = "http://[::ffff:192.168.0.1]/";

pub(super) const FIXTURE_TAB_NORMALIZED_RESULT_URL: &str = "http://example.com/abcd";

pub(super) const FIXTURE_UPPERCASE_SCHEME_RESULT_URL: &str = "HTTPS://example.com/result";

pub(super) const FIXTURE_BACKSLASH_RESULT_URL: &str = "https://example.com/abc\\def";

pub(super) const FIXTURE_EMBEDDED_HOST_RESULT_URL: &str = "https://x-ABCDEF.example/result";

pub(super) const FIXTURE_UNICODE_EMBEDDED_HOST_RESULT_URL: &str = "https://x-bücher.example/result";

pub(super) const FIXTURE_IDNA_REMOVED_CODE_POINT_RESULT_URL: &str =
    "https://x-ab\u{00ad}cd.example/result";

pub(super) const FIXTURE_IDNA_COMPATIBILITY_RESULT_URL: &str = "https://x-a¼b-y.example/result";

pub(super) const FIXTURE_C0_PREPROCESSED_RESULT_URL: &str = "https://example.com/abc";

pub(super) const FIXTURE_INSERTED_AUTHORITY_SLASH_RESULT_URL: &str = "https:/example.com";

pub(super) const FIXTURE_INSERTED_TWO_AUTHORITY_SLASHES_RESULT_URL: &str = "https:example.com";

pub(super) const FIXTURE_RESULT_SNIPPET: &str = "Synthetic recorded snippet";

pub(super) const FIXTURE_UNKNOWN_NAMED_REFERENCE_TITLE: &str = "Synthetic R&D; result";

pub(super) const FIXTURE_UNSUPPORTED_VALID_NAMED_REFERENCE_SNIPPET: &str =
    "Synthetic &copy; snippet";

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

pub(super) const URL_DOT_SEGMENT_COLLISION_KEY: &str = "abc/./def";

pub(super) const URL_DOT_SEGMENT_NORMALIZED_VALUE: &str = "abc/def";

pub(super) const URL_PREPROCESSED_DOT_SEGMENT_COLLISION_KEY: &str = "x\t/a/../b";

pub(super) const URL_PREPROCESSED_AUTHORITY_PORT_COLLISION_KEY: &str = "m:\t04";

pub(super) const URL_PREPROCESSED_DECODED_DOT_COLLISION_KEY: &str = "%\t2e%2e/secret";

pub(super) const URL_EMPTY_SEGMENT_DOT_COLLISION_KEY: &str = "a//../b";

pub(super) const URL_SCHEME_CASE_COLLISION_KEY: &str = "HTTPS";

pub(super) const URL_ENCODED_COLLISION_KEY: &str = "secret/key";

pub(super) const URL_ENCODED_COLLISION_VALUE: &str = "secret%2Fkey";

pub(super) const URL_FORM_COLLISION_KEY: &str = "secret key";

pub(super) const URL_FORM_COLLISION_VALUE: &str = "secret+key";

pub(super) const URL_IDNA_COLLISION_KEY: &str = "bücher";

pub(super) const URL_IDNA_COLLISION_VALUE: &str = "https://bücher.example/";

pub(super) const URL_HOST_CASE_COLLISION_KEY: &str = "EXAMPLE.COM";

pub(super) const URL_IPV6_COLLISION_KEY: &str = "2001:0db8:0:0:0:0:0:1";

pub(super) const URL_LEGACY_IPV4_COLLISION_KEY: &str = "2130706433";

pub(super) const URL_OCTAL_IPV4_COMPONENT_COLLISION_KEY: &str = "0177";

pub(super) const URL_HEX_IPV4_COMPONENT_COLLISION_KEY: &str = "0x7f";

pub(super) const URL_EMBEDDED_HEX_IPV4_COLLISION_KEY: &str = "7f";

pub(super) const URL_EMBEDDED_OCTAL_IPV4_COLLISION_KEY: &str = "177";

pub(super) const URL_HEX_DIGIT_SUBSTRING_COLLISION_KEY: &str = "f00";

pub(super) const URL_OCTAL_DIGIT_SUBSTRING_COLLISION_KEY: &str = "700";

pub(super) const URL_HEX_PREFIX_SPANNING_COLLISION_KEY: &str = "x7";

pub(super) const URL_IPV4_SEPARATOR_SPANNING_COLLISION_KEY: &str = ".0x7";

pub(super) const URL_IPV4_PADDED_SEPARATOR_SPANNING_COLLISION_KEY: &str = ".0x007";

pub(super) const URL_IPV4_PADDED_MULTI_COMPONENT_COLLISION_KEY: &str = "007f.00";

pub(super) const URL_IPV4_TRAILING_DOT_COLLISION_KEY: &str = ".1.";

pub(super) const URL_DISCARDED_PORT_ZERO_COLLISION_KEY: &str = ":0";

pub(super) const URL_IPV6_AUTHORITY_PORT_ZERO_COLLISION_KEY: &str = "]:0";

pub(super) const URL_INTERNAL_PORT_ZERO_COLLISION_KEY: &str = "m:04";

pub(super) const URL_PORT_PATH_ZERO_COLLISION_KEY: &str = "00/p";

pub(super) const URL_AUTHORITY_PORT_PATH_ZERO_COLLISION_KEY: &str = "m:0400/p";

pub(super) const URL_REMOVED_DEFAULT_PORT_PATH_COLLISION_KEY: &str = "m:80/p";

pub(super) const URL_BARE_REMOVED_DEFAULT_PORT_PATH_COLLISION_KEY: &str = "80/p";

pub(super) const URL_REMOVED_DEFAULT_PORT_COLLISION_KEY: &str = "m:80";

pub(super) const URL_AUTHORITY_DEFAULT_PORT_PREFIX_COLLISION_KEY: &str = "m:8";

pub(super) const URL_AUTHORITY_DEFAULT_PORT_COLLISION_KEY: &str = "example.com:443";

pub(super) const URL_DEFAULT_PORT_DIGIT_SUFFIX_COLLISION_KEY: &str = "0/p";

pub(super) const URL_COMPOSED_PORT_PATH_COLLISION_KEY: &str = "0400/a/../p";

pub(super) const URL_COMPLETE_EMPTY_QUERY_COLLISION_KEY: &str = "https://example.com/?";

pub(super) fn maximum_zero_prefixed_port_path_collision_key() -> String {
    let leading_zeroes = "0".repeat(MAX_CREDENTIAL_BYTES - "1/p".len());
    format!("{leading_zeroes}1/p")
}

pub(super) fn maximum_zero_prefixed_port_path_result_url() -> String {
    let leading_zeroes = "0".repeat(MAX_CREDENTIAL_BYTES - "1/p".len());
    format!("https://example.com:{leading_zeroes}1/path")
}

pub(super) const URL_EMPTY_PORT_COLLISION_KEY: &str = "m:/p";

pub(super) const URL_EMPTY_QUERY_DELIMITER_COLLISION_KEY: &str = "xyzabc?";

pub(super) const URL_EMPTY_FRAGMENT_DELIMITER_COLLISION_KEY: &str = "xyzabc#";

pub(super) const URL_USER_INFORMATION_BOUNDARY_COLLISION_KEY: &str = "m@example";

pub(super) const URL_USER_INFORMATION_LEGACY_IPV4_COLLISION_KEY: &str = "m@0x7";

pub(super) const URL_EMPTY_USER_INFORMATION_COLLISION_KEY: &str = "/@e";

pub(super) const URL_AUTHORITY_WITH_PORT_NON_COLLISION_KEY: &str = "com:1";

pub(super) const URL_MULTI_OCTET_IPV4_COMPONENT_COLLISION_KEY: &str = "0x100";

pub(super) const URL_IPV6_HEXTET_COLLISION_KEY: &str = "0db8";

pub(super) const URL_IPV6_SEPARATOR_SPANNING_COLLISION_KEY: &str = ":0db";

pub(super) const URL_IPV6_COMPRESSED_ZERO_SEPARATOR_COLLISION_KEY: &str = ":0000:";

pub(super) const URL_IPV6_COMPRESSED_ZERO_RUN_COLLISION_KEY: &str = ":0000:0000:";

pub(super) const URL_IPV6_COMPRESSED_ZERO_RUN_SUFFIX_COLLISION_KEY: &str = ":0000:0000:2:";

pub(super) const URL_IPV6_ZERO_RUN_BOUNDARY_COLLISION_KEY: &str = "0:";

pub(super) const URL_EMBEDDED_IPV6_HEXTET_COLLISION_KEY: &str = "0db";

pub(super) const URL_IPV6_MULTI_HEXTET_COLLISION_KEY: &str = "0db8:0000";

pub(super) const URL_IPV6_BRACKET_BOUND_COLLISION_KEY: &str = "[2001:0db8";

pub(super) const URL_IPV6_COMPRESSED_FRAGMENT_COLLISION_KEY: &str = "0db8::1";

pub(super) const URL_IPV4_TAIL_IPV6_COLLISION_KEY: &str = "192.168";

pub(super) const URL_IPV4_TAIL_DIGIT_SUBSTRING_COLLISION_KEY: &str = "68";

pub(super) const URL_IPV4_TAIL_SEPARATOR_SUBSTRING_COLLISION_KEY: &str = ".168";

pub(super) const URL_IPV4_TAIL_CROSS_COMPONENT_COLLISION_KEY: &str = "8.0";

pub(super) const URL_MIXED_COMPRESSED_IPV6_TAIL_COLLISION_KEY: &str = "::ffff:192.168";

pub(super) const URL_SEPARATOR_ONLY_IPV6_TAIL_COLLISION_KEY: &str = ":192.168";

pub(super) const URL_INTERNAL_TAB_COLLISION_KEY: &str = "ab\tcd";

pub(super) const URL_C0_PREPROCESSED_COLLISION_KEY: &str = "%00abc";

pub(super) const URL_BACKSLASH_COLLISION_KEY: &str = "abc\\def";

pub(super) const URL_INSERTED_AUTHORITY_SLASH_COLLISION_KEY: &str = "s:/e";

pub(super) const URL_INSERTED_TWO_AUTHORITY_SLASHES_COLLISION_KEY: &str = "s:e";

pub(super) const URL_DECODED_BACKSLASH_COLLISION_KEY: &str = "abc%5Cdef";

pub(super) const URL_DECODED_CASE_BACKSLASH_COLLISION_KEY: &str = "ABC%5CDEF";

pub(super) const URL_EMBEDDED_HOST_COLLISION_KEY: &str = "ABCDEF";

pub(super) const URL_UNICODE_HOST_COLLISION_KEY: &str = "BÜCHER";

pub(super) const URL_IDNA_REMOVED_CODE_POINT_COLLISION_KEY: &str = "ab\u{00ad}cd";

pub(super) const URL_IDNA_COMPATIBILITY_COLLISION_KEY: &str = "a¼b";

pub(super) const URL_DECOMPOSED_UNICODE_HOST_COLLISION_KEY: &str = "BU\u{0308}CHER";

pub(super) const URL_PORT_COLLISION_KEY: &str = ":08081";

pub(super) const URL_BARE_PORT_COLLISION_KEY: &str = "08081";

pub(super) const URL_PORT_COLLISION_VALUE: &str = "http://example.com:8081/";

pub(super) const URL_DEFAULT_PORT_FRAGMENT_COLLISION_KEY: &str = "080";

pub(super) const URL_EMBEDDED_PORT_COLLISION_VALUE: &str = "https://example.com:0800/";

pub(super) const URL_COMPLETE_DEFAULT_PORT_COLLISION_KEY: &str = "https://example.com:443/";

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

pub(super) const EXCESSIVE_FORM_ENCODING_VALUE: &str = "%252525252525252F";

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

pub(super) const ERROR_RESULT_BOUNDARY_DEBUG_COLLISION_KEY: &str = "Err(E";

pub(super) const KNOWN_FAILURE_RESULT_BOUNDARY_DEBUG_COLLISION_KEY: &str = "Ok(K";

pub(super) const POPULATED_RESPONSE_OPTION_DEBUG_COLLISION_KEY: &str = "Some(W";

pub(super) const POPULATED_EVIDENCE_OPTION_DEBUG_COLLISION_KEY: &str = "Some";

pub(super) const POPULATED_EVIDENCE_DETAIL_DEBUG_COLLISION_KEY: &str = "ToolExecutionErrorDetail";

pub(super) const POPULATED_SUCCESS_DEBUG_ESCAPE_COLLISION_KEY: &str = "\\";

pub(super) const REMOVED_DIAGNOSTIC_PROBE_WORD: &str = "diagnostic";

pub(super) const REMOVED_DETAIL_PROBE_WORD: &str = "probe";

pub(super) const DEBUG_RESULT_COUNT_COLLISION_COUNT: usize = 1;

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

pub(super) fn ascii_json_unicode_escape(value: &str) -> String {
    assert!(value.is_ascii(), "fixture must be ASCII");
    value
        .encode_utf16()
        .map(|code_unit| format!(r"\u{code_unit:04x}"))
        .collect()
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
