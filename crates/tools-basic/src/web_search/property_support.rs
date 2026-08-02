use proptest::{
    prelude::*,
    strategy::BoxedStrategy,
    test_runner::{Config, TestCaseResult, TestRunner},
};
use reqwest::Url;
use signalbox_model_runtime::CredentialValue;

use super::{diagnostic::*, evidence::*, redaction::*, result::*, test_support::*};

const PROPERTY_CASES: u32 = 512;
const GRAMMAR_CREDENTIAL_ATOM: &str = "fixturegrammar";
const GRAMMAR_PERCENT_REFLECTION: &str = "%66%69%78%74%75%72%65%67%72%61%6D%6D%61%72";
const GRAMMAR_JSON_REFLECTION: &str =
    r"\u0066\u0069\u0078\u0074\u0075\u0072\u0065\u0067\u0072\u0061\u006d\u006d\u0061\u0072";

#[derive(Clone, Copy, Debug)]
pub(super) enum CredentialGrammar {
    Exact,
    PercentEncodedReflection,
    PercentEncodedCredential,
    CaseFoldedReflection,
    JsonEscapedReflection,
    HtmlNumericReflection,
}

#[derive(Clone, Debug)]
pub(super) struct CredentialReflection {
    pub(super) credential: String,
    pub(super) reflection: String,
}

#[derive(serde::Deserialize)]
pub(super) struct IndependentlyDecodedEvidence {
    pub(super) results: Vec<IndependentlyDecodedResult>,
}

#[derive(serde::Deserialize)]
pub(super) struct IndependentlyDecodedResult {
    pub(super) title: String,
    pub(super) url: String,
    pub(super) snippet: String,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum StructuralUrlSlot {
    Username,
    Password,
    QueryName,
    QueryValue,
    Fragment,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum CredentialBearingSlot {
    HostLabel,
    PathSegment,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum ProviderTextSlot {
    Title,
    Snippet,
}

#[derive(Clone, Debug)]
pub(super) struct StructuralUrlCase {
    pub(super) source_url: String,
}

#[derive(Clone, Debug)]
pub(super) struct CredentialBearingUrlCase {
    pub(super) source_url: String,
    pub(super) credential: CredentialReflection,
}

#[derive(Clone, Debug)]
pub(super) struct ProviderTextCase {
    pub(super) fields: WebSearchResultFields,
    pub(super) credential: CredentialReflection,
}

pub(super) fn structural_url_strategy() -> BoxedStrategy<StructuralUrlCase> {
    (
        credential_reflection_strategy(),
        structural_url_slot_strategy(),
    )
        .prop_map(|(credential, slot)| StructuralUrlCase {
            source_url: structural_url(&credential.reflection, slot),
        })
        .boxed()
}

pub(super) fn credential_bearing_url_strategy() -> BoxedStrategy<CredentialBearingUrlCase> {
    prop_oneof![
        credential_reflection_strategy().prop_map(|credential| CredentialBearingUrlCase {
            source_url: credential_bearing_url(
                &credential.reflection,
                CredentialBearingSlot::PathSegment,
            ),
            credential,
        }),
        "[a-z][a-z0-9]{3,15}".prop_map(|reflection| CredentialBearingUrlCase {
            source_url: credential_bearing_url(&reflection, CredentialBearingSlot::HostLabel,),
            credential: CredentialReflection {
                credential: reflection.to_ascii_uppercase(),
                reflection,
            },
        }),
    ]
    .boxed()
}

pub(super) fn provider_text_strategy() -> BoxedStrategy<ProviderTextCase> {
    (
        credential_reflection_strategy(),
        provider_text_slot_strategy(),
    )
        .prop_map(|(credential, slot)| ProviderTextCase {
            fields: provider_text_fields(&credential.reflection, slot),
            credential,
        })
        .boxed()
}

pub(super) fn property_runner() -> TestRunner {
    TestRunner::new(Config {
        cases: PROPERTY_CASES,
        ..Config::default()
    })
}

pub(super) fn assert_structural_url_property() {
    property_runner()
        .run(&structural_url_strategy(), |case| {
            let result = WebSearchResult::try_new(WebSearchResultFields {
                title: String::from(FIXTURE_RESULT_TITLE),
                url: case.source_url,
                snippet: String::from(FIXTURE_RESULT_SNIPPET),
            })
            .ok_or_else(|| TestCaseError::fail("generated structural URL must parse"))?;

            prop_assert_eq!(result.url(), FIXTURE_RESULT_URL);
            Ok(())
        })
        .expect("structural URL grammar satisfies the output property");
}

pub(super) fn assert_credential_bearing_url_property() {
    property_runner()
        .run(&credential_bearing_url_strategy(), |case| {
            let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
                case.credential.credential.as_bytes().to_vec(),
            ))
            .ok_or_else(|| TestCaseError::fail("generated credential must be usable"))?;
            let result = WebSearchResult::try_new(WebSearchResultFields {
                title: String::from(FIXTURE_RESULT_TITLE),
                url: case.source_url,
                snippet: String::from(FIXTURE_RESULT_SNIPPET),
            })
            .ok_or_else(|| TestCaseError::fail("generated credential URL must parse"))?;
            let response =
                WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
                    .ok_or_else(|| TestCaseError::fail("generated response must be bounded"))?;

            assert_safe_evidence(success_evidence(response, &scrubber), &case.credential)?;
            Ok(())
        })
        .expect("credential-bearing URL grammar satisfies the output property");
}

pub(super) fn assert_provider_text_property() {
    property_runner()
        .run(&provider_text_strategy(), |case| {
            let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
                case.credential.credential.as_bytes().to_vec(),
            ))
            .ok_or_else(|| TestCaseError::fail("generated credential must be usable"))?;
            let result = WebSearchResult::try_new(case.fields)
                .ok_or_else(|| TestCaseError::fail("generated provider text must be bounded"))?;
            let response =
                WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
                    .ok_or_else(|| TestCaseError::fail("generated response must be bounded"))?;

            assert_safe_evidence(success_evidence(response, &scrubber), &case.credential)?;
            Ok(())
        })
        .expect("provider-text grammar satisfies the output property");
}

fn credential_reflection_strategy() -> BoxedStrategy<CredentialReflection> {
    (
        "[a-z][a-z0-9]{3,15}",
        prop_oneof![
            Just(CredentialGrammar::Exact),
            Just(CredentialGrammar::PercentEncodedReflection),
            Just(CredentialGrammar::PercentEncodedCredential),
            Just(CredentialGrammar::CaseFoldedReflection),
            Just(CredentialGrammar::JsonEscapedReflection),
            Just(CredentialGrammar::HtmlNumericReflection),
        ],
    )
        .prop_map(|(atom, grammar)| credential_reflection(&atom, grammar))
        .boxed()
}

fn structural_url_slot_strategy() -> BoxedStrategy<StructuralUrlSlot> {
    prop_oneof![
        Just(StructuralUrlSlot::Username),
        Just(StructuralUrlSlot::Password),
        Just(StructuralUrlSlot::QueryName),
        Just(StructuralUrlSlot::QueryValue),
        Just(StructuralUrlSlot::Fragment),
    ]
    .boxed()
}

fn provider_text_slot_strategy() -> BoxedStrategy<ProviderTextSlot> {
    prop_oneof![
        Just(ProviderTextSlot::Title),
        Just(ProviderTextSlot::Snippet),
    ]
    .boxed()
}

fn credential_reflection(atom: &str, grammar: CredentialGrammar) -> CredentialReflection {
    match grammar {
        CredentialGrammar::Exact => CredentialReflection {
            credential: atom.to_owned(),
            reflection: atom.to_owned(),
        },
        CredentialGrammar::PercentEncodedReflection => CredentialReflection {
            credential: atom.to_owned(),
            reflection: percent_encode(atom),
        },
        CredentialGrammar::PercentEncodedCredential => CredentialReflection {
            credential: percent_encode(atom),
            reflection: atom.to_owned(),
        },
        CredentialGrammar::CaseFoldedReflection => CredentialReflection {
            credential: atom.to_ascii_uppercase(),
            reflection: atom.to_owned(),
        },
        CredentialGrammar::JsonEscapedReflection => CredentialReflection {
            credential: atom.to_owned(),
            reflection: json_unicode_escape(atom),
        },
        CredentialGrammar::HtmlNumericReflection => CredentialReflection {
            credential: atom.to_owned(),
            reflection: html_numeric_escape(atom),
        },
    }
}

fn structural_url(reflection: &str, slot: StructuralUrlSlot) -> String {
    let mut url = Url::parse(FIXTURE_RESULT_URL).expect("fixture base URL is valid");
    match slot {
        StructuralUrlSlot::Username => {
            url.set_username(reflection)
                .expect("HTTP URL admits generated username");
        }
        StructuralUrlSlot::Password => {
            url.set_username("fixture-user")
                .expect("HTTP URL admits fixture username");
            url.set_password(Some(reflection))
                .expect("HTTP URL admits generated password");
        }
        StructuralUrlSlot::QueryName => {
            url.query_pairs_mut().append_pair(reflection, "fixture");
        }
        StructuralUrlSlot::QueryValue => {
            url.query_pairs_mut().append_pair("fixture", reflection);
        }
        StructuralUrlSlot::Fragment => url.set_fragment(Some(reflection)),
    }
    url.to_string()
}

fn credential_bearing_url(reflection: &str, slot: CredentialBearingSlot) -> String {
    match slot {
        CredentialBearingSlot::HostLabel => {
            format!("https://{reflection}.example/result")
        }
        CredentialBearingSlot::PathSegment => {
            let mut url = Url::parse(FIXTURE_RESULT_URL).expect("fixture base URL is valid");
            url.path_segments_mut()
                .expect("HTTP URL has path segments")
                .push(reflection);
            url.to_string()
        }
    }
}

fn provider_text_fields(reflection: &str, slot: ProviderTextSlot) -> WebSearchResultFields {
    match slot {
        ProviderTextSlot::Title => WebSearchResultFields {
            title: format!("Synthetic {reflection} result"),
            url: String::from(FIXTURE_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        },
        ProviderTextSlot::Snippet => WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: String::from(FIXTURE_RESULT_URL),
            snippet: format!("Synthetic {reflection} snippet"),
        },
    }
}

fn assert_safe_evidence(
    evidence: Result<signalbox_application::ToolExecutorEvidence, WebSearchExecutorError>,
    credential: &CredentialReflection,
) -> TestCaseResult {
    match evidence {
        Ok(signalbox_application::ToolExecutorEvidence::CompletedText(content)) => {
            let decoded: IndependentlyDecodedEvidence = serde_json::from_str(&content)
                .map_err(|_| TestCaseError::fail("completed evidence must remain typed JSON"))?;
            prop_assert_eq!(decoded.results.len(), 1);
            let result = decoded
                .results
                .first()
                .ok_or_else(|| TestCaseError::fail("generated evidence must retain one result"))?;
            assert_independent_component_absence(&result.title, credential)?;
            assert_independent_component_absence(&result.url, credential)?;
            assert_independent_component_absence(&result.snippet, credential)?;
            Ok(())
        }
        Ok(signalbox_application::ToolExecutorEvidence::KnownFailed { .. })
        | Ok(signalbox_application::ToolExecutorEvidence::Ambiguous) => Err(TestCaseError::fail(
            "success evidence changed terminal kind",
        )),
        Err(WebSearchExecutorError::EvidenceEncoding) => Ok(()),
        Err(error) => Err(TestCaseError::fail(format!(
            "unexpected evidence failure: {error:?}"
        ))),
    }
}

fn assert_independent_component_absence(
    component: &str,
    credential: &CredentialReflection,
) -> TestCaseResult {
    let component = component.to_ascii_lowercase();
    let credential_spelling = credential.credential.to_ascii_lowercase();
    let reflected_spelling = credential.reflection.to_ascii_lowercase();
    prop_assert!(!component.contains(&credential_spelling));
    prop_assert!(!component.contains(&reflected_spelling));
    Ok(())
}

fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| format!("%{byte:02X}"))
        .collect::<String>()
}

fn json_unicode_escape(value: &str) -> String {
    value
        .encode_utf16()
        .map(|code_unit| format!(r"\u{code_unit:04x}"))
        .collect::<String>()
}

fn html_numeric_escape(value: &str) -> String {
    value
        .chars()
        .map(|character| format!("&#{};", character as u32))
        .collect::<String>()
}

/// The credential grammar's percent branch preserves a known reversible
/// relationship instead of generating unrelated strings.
#[test]
fn credential_grammar_percent_branch_is_reversible() {
    let case = credential_reflection(
        GRAMMAR_CREDENTIAL_ATOM,
        CredentialGrammar::PercentEncodedReflection,
    );

    assert_eq!(case.credential, GRAMMAR_CREDENTIAL_ATOM);
    assert_eq!(case.reflection, GRAMMAR_PERCENT_REFLECTION);
}

/// The credential grammar's JSON branch emits four-digit escapes that match
/// the scrubber's JSON-escaped credential variant.
#[test]
fn credential_grammar_json_branch_matches_json_escape_form() {
    let case = credential_reflection(
        GRAMMAR_CREDENTIAL_ATOM,
        CredentialGrammar::JsonEscapedReflection,
    );

    assert_eq!(case.credential, GRAMMAR_CREDENTIAL_ATOM);
    assert_eq!(case.reflection, GRAMMAR_JSON_REFLECTION);
}

/// The structural URL grammar places generated text in user information while
/// preserving an otherwise canonical result URL.
#[test]
fn structural_url_grammar_places_username() {
    let source = structural_url(GRAMMAR_CREDENTIAL_ATOM, StructuralUrlSlot::Username);
    let parsed = Url::parse(&source).expect("generated username URL is valid");

    assert_eq!(parsed.username(), GRAMMAR_CREDENTIAL_ATOM);
}
