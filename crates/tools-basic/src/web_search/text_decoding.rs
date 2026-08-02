use icu_casemap::CaseMapper;
use unicode_normalization::UnicodeNormalization;
use url::form_urlencoded;

pub(super) const MAX_REVERSIBLE_DECODE_PASSES: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReversibleTextChange {
    Unchanged,
    Changed,
}

pub(super) struct DecodedText {
    pub(super) text: String,
    pub(super) change: ReversibleTextChange,
}

pub(super) fn decode_reversible_text_once(text: &str) -> Option<DecodedText> {
    let form_decoded = decode_form_component(text)?;
    let html_decoded = decode_html_character_references(&form_decoded.text);
    let json_decoded = decode_json_string_escapes(&html_decoded.text);
    let change = if form_decoded.change == ReversibleTextChange::Changed
        || html_decoded.change == ReversibleTextChange::Changed
        || json_decoded.change == ReversibleTextChange::Changed
    {
        ReversibleTextChange::Changed
    } else {
        ReversibleTextChange::Unchanged
    };
    Some(DecodedText {
        text: json_decoded.text,
        change,
    })
}

fn decode_form_component(text: &str) -> Option<DecodedText> {
    let encoded = format!("value={text}");
    let mut pairs = form_urlencoded::parse(encoded.as_bytes());
    let (name, value) = pairs.next()?;
    if name != "value" || pairs.next().is_some() {
        return Some(unchanged(text));
    }
    let decoded = value.into_owned();
    Some(changed_if_different(text, decoded))
}

pub(super) fn decode_html_character_references(text: &str) -> DecodedText {
    changed_if_different(text, html_escape::decode_html_entities(text).into_owned())
}

pub(super) fn decode_json_string_escapes(text: &str) -> DecodedText {
    let quoted = format!("\"{text}\"");
    serde_json::from_str::<String>(&quoted).map_or_else(
        |_| unchanged(text),
        |decoded| changed_if_different(text, decoded),
    )
}

fn unchanged(text: &str) -> DecodedText {
    DecodedText {
        text: text.to_owned(),
        change: ReversibleTextChange::Unchanged,
    }
}

fn changed_if_different(source: &str, decoded: String) -> DecodedText {
    let change = if decoded == source {
        ReversibleTextChange::Unchanged
    } else {
        ReversibleTextChange::Changed
    };
    DecodedText {
        text: decoded,
        change,
    }
}

pub(super) fn unicode_case_insensitive_contains(haystack: &str, needle: &str) -> bool {
    let normalized_needle = unicode_case_folded_nfd(needle);
    !normalized_needle.is_empty() && unicode_case_folded_nfd(haystack).contains(&normalized_needle)
}

pub(super) fn unicode_case_folded_nfd(text: &str) -> String {
    let decomposed = text.nfd().collect::<String>();
    let folded = CaseMapper::new().fold_string(&decomposed);
    folded.as_ref().nfd().collect()
}

pub(super) fn unicode_normalized_contains(haystack: &str, needle: &str) -> bool {
    let normalized_needle = needle.nfd().collect::<String>();
    !normalized_needle.is_empty()
        && haystack
            .nfd()
            .collect::<String>()
            .contains(&normalized_needle)
}
