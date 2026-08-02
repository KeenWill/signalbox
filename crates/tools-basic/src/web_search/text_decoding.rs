use icu_casemap::CaseMapper;
use unicode_normalization::UnicodeNormalization;

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
    let form_decoded = String::from_utf8(form_decode_once(text.as_bytes())).ok()?;
    let form_changed = form_decoded != text;
    let html_decoded = decode_html_character_references(&form_decoded)?;
    let json_decoded = decode_json_string_escapes(&html_decoded.text);
    let change = if form_changed
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

// WHATWG's named-character-reference table marks these legacy names as
// accepting an omitted semicolon:
// https://html.spec.whatwg.org/multipage/named-characters.html
pub(super) const LEGACY_SEMICOLONLESS_HTML_REFERENCES: [&str; 106] = [
    "AElig", "AMP", "Aacute", "Acirc", "Agrave", "Aring", "Atilde", "Auml", "COPY", "Ccedil",
    "ETH", "Eacute", "Ecirc", "Egrave", "Euml", "GT", "Iacute", "Icirc", "Igrave", "Iuml", "LT",
    "Ntilde", "Oacute", "Ocirc", "Ograve", "Oslash", "Otilde", "Ouml", "QUOT", "REG", "THORN",
    "Uacute", "Ucirc", "Ugrave", "Uuml", "Yacute", "aacute", "acirc", "acute", "aelig", "agrave",
    "amp", "aring", "atilde", "auml", "brvbar", "ccedil", "cedil", "cent", "copy", "curren", "deg",
    "divide", "eacute", "ecirc", "egrave", "eth", "euml", "frac12", "frac14", "frac34", "gt",
    "iacute", "icirc", "iexcl", "igrave", "iquest", "iuml", "laquo", "lt", "macr", "micro",
    "middot", "nbsp", "not", "ntilde", "oacute", "ocirc", "ograve", "ordf", "ordm", "oslash",
    "otilde", "ouml", "para", "plusmn", "pound", "quot", "raquo", "reg", "sect", "shy", "sup1",
    "sup2", "sup3", "szlig", "thorn", "times", "uacute", "ucirc", "ugrave", "uml", "uuml",
    "yacute", "yen", "yuml",
];

pub(super) fn decode_html_character_references(text: &str) -> Option<DecodedText> {
    const MAX_CHARACTER_REFERENCE_BYTES: usize = 64;
    let mut decoded = String::with_capacity(text.len());
    let mut remaining = text;
    let mut changed = false;
    while let Some(reference_start) = remaining.find('&') {
        decoded.push_str(&remaining[..reference_start]);
        let reference = &remaining[reference_start..];
        let relative_end = reference
            .bytes()
            .take(MAX_CHARACTER_REFERENCE_BYTES)
            .position(|byte| byte == b';');
        let nested_reference = reference
            .bytes()
            .take(MAX_CHARACTER_REFERENCE_BYTES)
            .skip(1)
            .position(|byte| byte == b'&')
            .map(|index| index + 1);
        if let Some(nested) =
            nested_reference.filter(|nested| relative_end.is_none_or(|end| *nested < end))
        {
            let candidate = &reference[..nested];
            if numeric_character_reference_prefix(candidate)
                || legacy_named_character_reference_prefix(candidate)
            {
                return None;
            }
            decoded.push_str(candidate);
            remaining = &reference[nested..];
            continue;
        }
        let Some(relative_end) = relative_end else {
            if numeric_character_reference_prefix(reference)
                || legacy_named_character_reference_prefix(reference)
            {
                return None;
            }
            decoded.push('&');
            remaining = &reference[1..];
            continue;
        };
        let entity = &reference[1..relative_end];
        match decode_html_character_reference(entity) {
            Some(replacement) => {
                decoded.push_str(&replacement);
                changed = true;
            }
            None if entity.starts_with('#') => return None,
            None if plausible_named_character_reference(entity) => return None,
            None if prefixed_legacy_named_character_reference(&reference[..relative_end + 1]) => {
                return None;
            }
            None => decoded.push_str(&reference[..relative_end + 1]),
        }
        remaining = &reference[relative_end + 1..];
    }
    decoded.push_str(remaining);
    Some(DecodedText {
        text: decoded,
        change: if changed {
            ReversibleTextChange::Changed
        } else {
            ReversibleTextChange::Unchanged
        },
    })
}

fn plausible_named_character_reference(entity: &str) -> bool {
    entity
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic())
        && entity.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

pub(super) fn numeric_character_reference_prefix(reference: &str) -> bool {
    let Some(numeric) = reference.as_bytes().strip_prefix(b"&#") else {
        return false;
    };
    let (digits, radix) = if let Some(hexadecimal) = numeric
        .strip_prefix(b"x")
        .or_else(|| numeric.strip_prefix(b"X"))
    {
        (hexadecimal, 16)
    } else {
        (numeric, 10)
    };
    digits.first().is_some_and(|byte| match radix {
        16 => byte.is_ascii_hexdigit(),
        10 => byte.is_ascii_digit(),
        _ => false,
    })
}

pub(super) fn legacy_named_character_reference_prefix(reference: &str) -> bool {
    let Some(named) = reference.strip_prefix('&') else {
        return false;
    };
    LEGACY_SEMICOLONLESS_HTML_REFERENCES
        .iter()
        .any(|legacy| named.starts_with(legacy))
}

fn prefixed_legacy_named_character_reference(reference: &str) -> bool {
    let Some(named) = reference
        .strip_prefix('&')
        .and_then(|named| named.strip_suffix(';'))
    else {
        return false;
    };
    LEGACY_SEMICOLONLESS_HTML_REFERENCES
        .iter()
        .any(|legacy| named.starts_with(legacy) && named.len() > legacy.len())
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

pub(super) fn decode_json_string_escapes(text: &str) -> DecodedText {
    let mut decoded = String::with_capacity(text.len());
    let mut remaining = text;
    let mut changed = false;
    while let Some(relative_start) = remaining.find('\\') {
        decoded.push_str(&remaining[..relative_start]);
        let escape = &remaining[relative_start..];
        let decoded_escape = match escape.as_bytes().get(1) {
            Some(b'"') => Some(('"', 2)),
            Some(b'\\') => Some(('\\', 2)),
            Some(b'/') => Some(('/', 2)),
            Some(b'b') => Some(('\u{8}', 2)),
            Some(b'f') => Some(('\u{c}', 2)),
            Some(b'n') => Some(('\n', 2)),
            Some(b'r') => Some(('\r', 2)),
            Some(b't') => Some(('\t', 2)),
            Some(b'u') => decode_json_unicode_escape(escape)
                .or_else(|| decode_rust_debug_unicode_escape(escape)),
            _ => None,
        };
        let Some((character, consumed)) = decoded_escape else {
            decoded.push('\\');
            remaining = &escape[1..];
            continue;
        };
        decoded.push(character);
        remaining = &escape[consumed..];
        changed = true;
    }
    decoded.push_str(remaining);
    DecodedText {
        text: decoded,
        change: if changed {
            ReversibleTextChange::Changed
        } else {
            ReversibleTextChange::Unchanged
        },
    }
}

pub(super) fn decode_json_unicode_escape(escape: &str) -> Option<(char, usize)> {
    const CODE_UNIT_ESCAPE_BYTES: usize = 6;
    const SURROGATE_PAIR_ESCAPE_BYTES: usize = CODE_UNIT_ESCAPE_BYTES * 2;
    let first = decode_json_code_unit(escape)?;
    if (0xd800..=0xdbff).contains(&first) {
        let second = decode_json_code_unit(escape.get(CODE_UNIT_ESCAPE_BYTES..)?)?;
        if !(0xdc00..=0xdfff).contains(&second) {
            return None;
        }
        let scalar = 0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00);
        return char::from_u32(scalar).map(|character| (character, SURROGATE_PAIR_ESCAPE_BYTES));
    }
    if (0xdc00..=0xdfff).contains(&first) {
        return None;
    }
    char::from_u32(u32::from(first)).map(|character| (character, CODE_UNIT_ESCAPE_BYTES))
}

pub(super) fn decode_json_code_unit(escape: &str) -> Option<u16> {
    let digits = escape.strip_prefix("\\u")?.get(..4)?;
    if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(digits, 16).ok()
}

pub(super) fn decode_rust_debug_unicode_escape(escape: &str) -> Option<(char, usize)> {
    const MAX_SCALAR_HEX_DIGITS: usize = 6;
    let digits_and_suffix = escape.strip_prefix("\\u{")?;
    let closing_brace = digits_and_suffix.find('}')?;
    let digits = digits_and_suffix.get(..closing_brace)?;
    if digits.is_empty()
        || digits.len() > MAX_SCALAR_HEX_DIGITS
        || !digits.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let scalar = u32::from_str_radix(digits, 16).ok()?;
    char::from_u32(scalar).map(|character| (character, 3 + digits.len() + 1))
}

pub(super) fn decode_html_character_reference(entity: &str) -> Option<String> {
    let named = match entity {
        "amp" | "AMP" => Some("&"),
        "apos" => Some("'"),
        "ast" => Some("*"),
        "gt" | "GT" => Some(">"),
        "lt" | "LT" => Some("<"),
        "nbsp" => Some("\u{a0}"),
        "quot" | "QUOT" => Some("\""),
        _ => None,
    };
    if let Some(named) = named {
        return Some(String::from(named));
    }
    let (digits, radix) = if let Some(digits) = entity
        .strip_prefix("#x")
        .or_else(|| entity.strip_prefix("#X"))
    {
        (digits, 16)
    } else {
        (entity.strip_prefix('#')?, 10)
    };
    let valid_digits = match radix {
        16 => digits.bytes().all(|byte| byte.is_ascii_hexdigit()),
        10 => digits.bytes().all(|byte| byte.is_ascii_digit()),
        _ => false,
    };
    if digits.is_empty() || !valid_digits {
        return None;
    }
    let scalar = u32::from_str_radix(digits, radix).ok()?;
    let scalar = html_numeric_reference_scalar(scalar);
    char::from_u32(scalar).map(|character| character.to_string())
}

pub(super) const fn html_numeric_reference_scalar(scalar: u32) -> u32 {
    match scalar {
        0 | 0xd800..=0xdfff | 0x11_0000..=u32::MAX => 0xfffd,
        0x80 => 0x20ac,
        0x82 => 0x201a,
        0x83 => 0x0192,
        0x84 => 0x201e,
        0x85 => 0x2026,
        0x86 => 0x2020,
        0x87 => 0x2021,
        0x88 => 0x02c6,
        0x89 => 0x2030,
        0x8a => 0x0160,
        0x8b => 0x2039,
        0x8c => 0x0152,
        0x8e => 0x017d,
        0x91 => 0x2018,
        0x92 => 0x2019,
        0x93 => 0x201c,
        0x94 => 0x201d,
        0x95 => 0x2022,
        0x96 => 0x2013,
        0x97 => 0x2014,
        0x98 => 0x02dc,
        0x99 => 0x2122,
        0x9a => 0x0161,
        0x9b => 0x203a,
        0x9c => 0x0153,
        0x9e => 0x017e,
        0x9f => 0x0178,
        scalar => scalar,
    }
}

pub(super) fn form_decode_once(encoded: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut index = 0;
    while index < encoded.len() {
        let byte = encoded[index];
        let high = encoded.get(index + 1).copied().and_then(hex_value);
        let low = encoded.get(index + 2).copied().and_then(hex_value);
        if byte == b'+' {
            decoded.push(b' ');
            index += 1;
        } else if byte == b'%'
            && let (Some(high), Some(low)) = (high, low)
        {
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }
    decoded
}

pub(super) const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
