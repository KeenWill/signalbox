use signalbox_application::{
    SearchHighlight, max_search_highlights_per_result, max_search_snippet_bytes,
};

use super::SearchProjectionCorruption;

#[cfg(test)]
const HEADLINE_START: &str = "\u{e000}";
#[cfg(test)]
const HEADLINE_END: &str = "\u{e001}";

pub(super) fn decode_headline(
    marked: String,
    start_marker: &str,
    stop_marker: &str,
) -> Result<(String, Vec<SearchHighlight>), SearchProjectionCorruption> {
    let mut plain = String::new();
    let mut marked_ranges = Vec::new();
    let mut active_start = None;
    let mut remaining = marked.as_str();
    while !remaining.is_empty() {
        if let Some(rest) = remaining.strip_prefix(start_marker) {
            active_start = Some(plain.len());
            remaining = rest;
            continue;
        }
        if let Some(rest) = remaining.strip_prefix(stop_marker) {
            append_marked_range(&mut marked_ranges, active_start.take(), plain.len());
            remaining = rest;
            continue;
        }
        if let Some(rest) = remaining.strip_prefix("&lt;") {
            plain.push('<');
            remaining = rest;
            continue;
        }
        if let Some(rest) = remaining.strip_prefix("&amp;") {
            plain.push('&');
            remaining = rest;
            continue;
        }
        let Some(character) = remaining.chars().next() else {
            break;
        };
        plain.push(character);
        remaining = &remaining[character.len_utf8()..];
    }
    append_marked_range(&mut marked_ranges, active_start, plain.len());

    let (window_start, window_end) = snippet_window(&plain, marked_ranges.first().copied());
    let snippet = plain[window_start..window_end].to_owned();
    let clipped_ranges = marked_ranges
        .into_iter()
        .filter_map(|(start, end)| {
            let clipped_start = start.max(window_start);
            let clipped_end = end.min(window_end);
            (clipped_start < clipped_end).then_some((clipped_start, clipped_end))
        })
        .collect::<Vec<_>>();
    if clipped_ranges.len() > max_search_highlights_per_result() {
        return Err(SearchProjectionCorruption::Invalid("highlight count"));
    }
    let highlights = clipped_ranges
        .into_iter()
        .map(|(start, end)| {
            Ok(SearchHighlight {
                start_byte: u16::try_from(start - window_start)
                    .map_err(|_| SearchProjectionCorruption::Invalid("highlight start"))?,
                end_byte: u16::try_from(end - window_start)
                    .map_err(|_| SearchProjectionCorruption::Invalid("highlight end"))?,
            })
        })
        .collect::<Result<Vec<_>, SearchProjectionCorruption>>()?;
    Ok((snippet, highlights))
}

fn append_marked_range(ranges: &mut Vec<(usize, usize)>, start: Option<usize>, end: usize) {
    let Some(start) = start.filter(|start| *start < end) else {
        return;
    };
    ranges.push((start, end));
}

fn snippet_window(plain: &str, first_match: Option<(usize, usize)>) -> (usize, usize) {
    let bound = max_search_snippet_bytes();
    if plain.len() <= bound {
        return (0, plain.len());
    }
    let desired_start = first_match
        .map(|(start, end)| {
            let match_len = end - start;
            start.saturating_sub(bound.saturating_sub(match_len) / 2)
        })
        .unwrap_or(0)
        .min(plain.len() - bound);
    let mut start = desired_start;
    while !plain.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + bound).min(plain.len());
    while !plain.is_char_boundary(end) {
        end -= 1;
    }
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headline_decoder_returns_plain_bounded_text_and_byte_ranges() {
        const PREFIX: &str = "before <b>";
        const MATCH: &str = "café";
        const SUFFIX: &str = "</b> after";
        let marked = format!("{PREFIX}{HEADLINE_START}{MATCH}{HEADLINE_END}{SUFFIX}");
        let (snippet, highlights) = decode_headline(marked, HEADLINE_START, HEADLINE_END)
            .expect("fixture headline decodes");

        assert_eq!(snippet, format!("{PREFIX}{MATCH}{SUFFIX}"));
        assert_eq!(
            highlights,
            vec![SearchHighlight {
                start_byte: u16::try_from(PREFIX.len()).expect("fixture offset fits"),
                end_byte: u16::try_from(PREFIX.len() + MATCH.len()).expect("fixture offset fits"),
            }]
        );
    }

    #[test]
    fn headline_decoder_caps_large_source_text() {
        let marked = format!(
            "{HEADLINE_START}{}{HEADLINE_END}",
            "x".repeat(max_search_snippet_bytes() * 2)
        );
        let (snippet, highlights) = decode_headline(marked, HEADLINE_START, HEADLINE_END)
            .expect("fixture headline decodes");

        assert_eq!(snippet.len(), max_search_snippet_bytes());
        assert_eq!(
            highlights,
            vec![SearchHighlight {
                start_byte: 0,
                end_byte: u16::try_from(max_search_snippet_bytes())
                    .expect("snippet bound fits highlight offsets"),
            }]
        );
    }

    #[test]
    fn headline_decoder_keeps_a_match_after_a_long_unmatched_prefix() {
        let marked = format!(
            "{}{HEADLINE_START}needle{HEADLINE_END} after",
            "x".repeat(max_search_snippet_bytes() + 64)
        );
        let (snippet, highlights) = decode_headline(marked, HEADLINE_START, HEADLINE_END)
            .expect("fixture headline decodes");

        assert!(snippet.contains("needle"));
        assert_eq!(highlights.len(), 1);
        let highlight = highlights[0];
        assert_eq!(
            &snippet[usize::from(highlight.start_byte)..usize::from(highlight.end_byte)],
            "needle"
        );
        assert!(snippet.len() <= max_search_snippet_bytes());
    }

    #[test]
    fn headline_decoder_preserves_literal_private_use_characters() {
        const START: &str = "<search-start>";
        const STOP: &str = "<search-stop>";
        let marked = format!("literal {HEADLINE_START} {START}needle{STOP} {HEADLINE_END}");
        let (snippet, highlights) =
            decode_headline(marked, START, STOP).expect("fixture headline decodes");

        assert_eq!(
            snippet,
            format!("literal {HEADLINE_START} needle {HEADLINE_END}")
        );
        assert_eq!(highlights.len(), 1);
        let highlight = highlights[0];
        assert_eq!(
            &snippet[usize::from(highlight.start_byte)..usize::from(highlight.end_byte)],
            "needle"
        );
    }

    #[test]
    fn headline_decoder_restores_escaped_framing_text() {
        const START: &str = "<sb-search-start>";
        const STOP: &str = "<sb-search-stop>";
        let marked =
            format!("&lt;sb-search-start> &amp;lt; {START}needle{STOP} &lt;sb-search-stop>");
        let (snippet, highlights) =
            decode_headline(marked, START, STOP).expect("escaped framing text decodes");

        assert_eq!(snippet, "<sb-search-start> &lt; needle <sb-search-stop>");
        assert_eq!(highlights.len(), 1);
        let highlight = highlights[0];
        assert_eq!(
            &snippet[usize::from(highlight.start_byte)..usize::from(highlight.end_byte)],
            "needle"
        );
    }
}
