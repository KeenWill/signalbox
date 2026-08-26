//! Corpus-driven and exhaustive-split regression coverage for
//! `redact_text`/`redact_json` in `cli_redaction.rs`, included there via
//! `#[path]` as `redaction_corpus_tests`.
//!
//! Pins the fixed classification of `testdata/redaction-corpus.txt` against
//! `testdata/redaction-corpus.classifications`, then exhaustively splits each
//! corpus line at every one- and two-boundary UTF-8 offset to check that the
//! stateful streaming redactor removes markers the non-streamed path removes;
//! the remaining `KNOWN_FAILING_SPLITS` ledger records exact, tracked exceptions.

use std::collections::BTreeSet;

use crate::{Observation, ObservationFact, ObservationSink, TokenUsage};

use super::{REDACTED_JSON_OBJECT, RedactingSink, ToolArgumentRedaction, redact_json, redact_text};

const CORPUS: &str = include_str!("testdata/redaction-corpus.txt");
const CLASSIFICATIONS: &str = include_str!("testdata/redaction-corpus.classifications");
const SYNTHETIC_SECRET_MARKER: &str = "SYNTHETIC-SECRET";
const CORPUS_LINE_COUNT: usize = 147;
const EXPECTED_REDACTED_COUNT: usize = 119;
const EXPECTED_ACCEPTED_UNCOVERED_COUNT: usize = 28;
const EXPECTED_KNOWN_FAILING_COUNT: usize = 0;
/// The same defect ledger one delta deeper, now empty after exhaustive
/// enumeration of every guarded two-split case.
const KNOWN_FAILING_TWO_SPLIT_CASES: usize = 0;
/// The exact fragmentations that still leak, as `(corpus line, split)`.
///
/// This is a defect ledger for shapes the contract COVERS and the sink leaks.
/// It is categorically not `ACCEPTED-UNCOVERED`, which records shapes the
/// specification openly declines to cover; nothing here is fine.
///
/// The match is exact in both directions. A split that appears and is not
/// listed is a regression. A listed split that stops leaking is progress, and
/// it must shrink this ledger rather than let it overstate the damage.
const KNOWN_FAILING_SPLITS: [DivergentSplit; 0] = [];
const CORRELATION: u8 = 7;
const BOUNDARY_FRAGMENT: &str = "{}";
const GENERATOR_SEED: u64 = 0x5eed_c0de_d15c_a11e;
const DEFAULT_GENERATIVE_CASES: usize = 512;
const SOAK_GENERATIVE_CASES: usize = 32_768;
/// Every UTF-8 boundary of every corpus line, both empty-delta ends
/// included. Enumerating the single-split class exhaustively is what keeps
/// it closed: sampling split points misses the specific boundary a held
/// state mishandles, which is how three boundary defects reached review.
const EXHAUSTIVE_SINGLE_SPLIT_CASES: usize = 6_230;
/// The enumerated cases whose joined line `redact_text` actually redacts, so
/// the stateful path is held to a nonvacuous obligation. The remainder are
/// the accepted-uncovered lines, enumerated but unguarded.
const EXHAUSTIVE_SINGLE_SPLIT_GUARDED_CASES: usize = 5_164;
/// Every ordered pair of UTF-8 boundaries of every corpus line.
const EXHAUSTIVE_TWO_SPLIT_CASES: usize = 147_848;
const EXHAUSTIVE_TWO_SPLIT_GUARDED_CASES: usize = 126_024;
const LCG_MULTIPLIER: u64 = 6_364_136_223_846_793_005;
const LCG_INCREMENT: u64 = 1_442_695_040_888_963_407;
const ASCII_NOISE: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_-";
/// Cases the original Claude engine redacted that were not represented by
/// the Codex corpus, including its fail-closed control-character rule.
const ORIGINAL_CLAUDE_REDACTION_CORPUS: [&str; 7] = [
    "Authorization: SYNTHETIC-SECRET-CLAUDE-01",
    "api_key=SYNTHETIC-SECRET-CLAUDE-02",
    "api_\u{001b}[0mkey=SYNTHETIC-SECRET-CLAUDE-03",
    "api_\u{0007}key=SYNTHETIC-SECRET-CLAUDE-04",
    r#"{"text":"api_\u001b[0mkey=SYNTHETIC-SECRET-CLAUDE-05"}"#,
    "secret=SYNTHETIC-SECRET-CLAUDE-06",
    r#"detail: "client_secret":"SYNTHETIC-SECRET-CLAUDE-07""#,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CorpusStatus {
    Redacted,
    AcceptedUncovered,
    /// The shape is covered by the contract and the implementation leaks it.
    ///
    /// This is a defect ledger, categorically not `AcceptedUncovered`: that
    /// status records what the contract openly declines to cover, while this
    /// one records a covered shape the sink fails. Nothing here is fine.
    ///
    /// The ledger is matched exactly rather than required to be empty. A
    /// covered shape that starts leaking fails as a regression, and a listed
    /// one that stops leaking also fails, forcing the ledger to shrink instead
    /// of overstating the damage. Requiring emptiness instead would make the
    /// suite permanently red, which cannot ship the reduction it measures and
    /// teaches readers to ignore the signal.
    KnownFailing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CorpusEvent {
    Delta,
    Boundary,
    Usage,
    Suppress,
    Seed,
    Drop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CorpusMode {
    Stateless,
    Stateful,
}

#[derive(Debug, PartialEq, Eq)]
enum CorpusPart {
    Text(String),
    Event(CorpusEvent),
}

#[derive(Debug, PartialEq, Eq)]
struct CorpusCase {
    parts: Vec<CorpusPart>,
    mode: CorpusMode,
}

#[derive(Debug, PartialEq, Eq)]
struct CorpusExpectation {
    line: usize,
    status: CorpusStatus,
    reason: String,
}

#[derive(Debug)]
struct ObservedOutput {
    channel: &'static str,
    text: String,
}

#[derive(Debug, PartialEq, Eq)]
struct ClassificationMismatch {
    line: usize,
    expected: CorpusStatus,
    actual: CorpusStatus,
    reason: String,
    surviving_channels: Vec<&'static str>,
}

#[derive(Debug, PartialEq, Eq)]
struct CorpusSummary {
    lines: usize,
    redacted: usize,
    accepted_uncovered: usize,
    known_failing: Vec<KnownFailure>,
    mismatches: Vec<ClassificationMismatch>,
}

#[derive(Debug, PartialEq, Eq)]
struct KnownFailure {
    line: usize,
    reason: String,
    surviving_channels: Vec<&'static str>,
}

/// One divergent fragmentation. Named fields rather than a positional pair:
/// the two values are the same type with different meanings, so a
/// transposition would compile and a failure would print unlabelled numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DivergentSplit {
    line: usize,
    split: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DivergentTwoSplit {
    line: usize,
    first_split: usize,
    second_split: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct SplitLeak {
    line: usize,
    splits: Vec<usize>,
    fragments: Vec<String>,
    emitted: String,
}

#[derive(Debug, PartialEq, Eq)]
struct SplitSummary {
    lines: usize,
    cases: usize,
    guarded_cases: usize,
    leaks: Vec<SplitLeak>,
}

impl SplitSummary {
    /// The exact divergent fragmentations, canonically ordered.
    fn divergent_splits(&self) -> Vec<DivergentSplit> {
        let mut inventory = self
            .leaks
            .iter()
            .map(|leak| DivergentSplit {
                line: leak.line,
                split: leak.splits[0],
            })
            .collect::<Vec<_>>();
        inventory.sort_unstable_by_key(|entry| (entry.line, entry.split));
        inventory
    }

    fn divergent_two_splits(&self) -> Vec<DivergentTwoSplit> {
        let mut inventory = self
            .leaks
            .iter()
            .map(|leak| DivergentTwoSplit {
                line: leak.line,
                first_split: leak.splits[0],
                second_split: leak.splits[1],
            })
            .collect::<Vec<_>>();
        inventory.sort_unstable();
        inventory
    }
}

fn recorded_two_split_leaks() -> BTreeSet<DivergentTwoSplit> {
    BTreeSet::new()
}

fn assert_exact_two_split_leak_ledger(summary: &SplitSummary) {
    let actual = summary
        .divergent_two_splits()
        .into_iter()
        .collect::<BTreeSet<_>>();
    let recorded = recorded_two_split_leaks();
    let added = actual.difference(&recorded).copied().collect::<Vec<_>>();
    let removed = recorded.difference(&actual).copied().collect::<Vec<_>>();

    assert_eq!(
        recorded.len(),
        KNOWN_FAILING_TWO_SPLIT_CASES,
        "the recorded two-split ledger count drifted"
    );
    assert!(
        added.is_empty() && removed.is_empty(),
        "two-split leak ledger changed\nadded leaks (regressions): {added:#?}\n\
         removed leaks (fixes): {removed:#?}"
    );
}

#[derive(Clone, Copy)]
enum TerminalContext {
    Seed,
    Drop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GeneratedFamily {
    ApiKey,
    EscapedApiKey,
    TokenName,
    SpacedPasswordFlag,
    UrlUserinfo,
    MalformedQuotedJsonKey,
    MysqlPassword,
    SigningKey,
}

const GENERATED_FAMILIES: [GeneratedFamily; 8] = [
    GeneratedFamily::ApiKey,
    GeneratedFamily::EscapedApiKey,
    GeneratedFamily::TokenName,
    GeneratedFamily::SpacedPasswordFlag,
    GeneratedFamily::UrlUserinfo,
    GeneratedFamily::MalformedQuotedJsonKey,
    GeneratedFamily::MysqlPassword,
    GeneratedFamily::SigningKey,
];

impl GeneratedFamily {
    fn from_ordinal(ordinal: usize) -> Self {
        GENERATED_FAMILIES[ordinal % GENERATED_FAMILIES.len()]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GeneratedInterlude {
    None,
    ToolArgumentsBoundary,
    UsageReported,
    Finish,
}

const GENERATED_INTERLUDES: [GeneratedInterlude; 4] = [
    GeneratedInterlude::None,
    GeneratedInterlude::ToolArgumentsBoundary,
    GeneratedInterlude::UsageReported,
    GeneratedInterlude::Finish,
];

#[derive(Debug, PartialEq, Eq)]
struct GeneratedCase {
    ordinal: usize,
    family: GeneratedFamily,
    marker: String,
    input: String,
    chunks: Vec<String>,
    interludes: Vec<GeneratedInterlude>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SuppressionAction {
    UsageReported,
    Finish,
    ProviderChunk(usize),
}

const SUPPRESSION_BARRIERS: [SuppressionAction; 2] =
    [SuppressionAction::UsageReported, SuppressionAction::Finish];

struct DeterministicGenerator {
    state: u64,
}

impl DeterministicGenerator {
    fn seeded(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(LCG_MULTIPLIER)
            .wrapping_add(LCG_INCREMENT);
        self.state
    }

    fn index(&mut self, exclusive_end: usize) -> usize {
        (self.next_u64() % exclusive_end as u64) as usize
    }
}

fn decode_corpus_line(encoded: &str) -> CorpusCase {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut remaining = encoded;
    let mut mode = CorpusMode::Stateless;
    while let Some(token_start) = remaining.find("<|") {
        text.push_str(&remaining[..token_start]);
        let token_tail = &remaining[token_start..];
        let token_end = token_tail
            .find("|>")
            .map(|end| end + 2)
            .expect("corpus tokens must have a closing `|>`");
        let token = &token_tail[..token_end];
        match token {
            "<|NL|>" => text.push('\n'),
            "<|CR|>" => text.push('\r'),
            "<|TAB|>" => text.push('\t'),
            "<|SOH|>" => text.push('\u{0001}'),
            "<|ZWSP|>" => text.push('\u{200b}'),
            "<|SHY|>" => text.push('\u{00ad}'),
            "<|NBHY|>" => text.push('\u{2011}'),
            "<|MIDDOT|>" => text.push('\u{00b7}'),
            "<|RLO|>" => text.push('\u{202e}'),
            "<|D|>" => {
                parts.push(CorpusPart::Text(std::mem::take(&mut text)));
                parts.push(CorpusPart::Event(CorpusEvent::Delta));
                mode = CorpusMode::Stateful;
            }
            "<|B|>" => {
                parts.push(CorpusPart::Text(std::mem::take(&mut text)));
                parts.push(CorpusPart::Event(CorpusEvent::Boundary));
                mode = CorpusMode::Stateful;
            }
            "<|U|>" => {
                parts.push(CorpusPart::Text(std::mem::take(&mut text)));
                parts.push(CorpusPart::Event(CorpusEvent::Usage));
                mode = CorpusMode::Stateful;
            }
            "<|SUPPRESS|>" => {
                parts.push(CorpusPart::Text(std::mem::take(&mut text)));
                parts.push(CorpusPart::Event(CorpusEvent::Suppress));
                mode = CorpusMode::Stateful;
            }
            "<|SEED|>" => {
                parts.push(CorpusPart::Text(std::mem::take(&mut text)));
                parts.push(CorpusPart::Event(CorpusEvent::Seed));
                mode = CorpusMode::Stateful;
            }
            "<|DROP|>" => {
                parts.push(CorpusPart::Text(std::mem::take(&mut text)));
                parts.push(CorpusPart::Event(CorpusEvent::Drop));
                mode = CorpusMode::Stateful;
            }
            unknown => panic!("unknown corpus token `{unknown}`"),
        }
        remaining = &token_tail[token_end..];
    }
    text.push_str(remaining);
    parts.push(CorpusPart::Text(text));
    CorpusCase { parts, mode }
}

fn parse_expectation_line(encoded: &str, expected_line: usize) -> CorpusExpectation {
    let mut fields = encoded.splitn(3, " | ");
    let line = fields
        .next()
        .expect("classification line must carry its line number")
        .parse::<usize>()
        .expect("classification line number must be decimal");
    let status = match fields
        .next()
        .expect("classification line must carry its status")
    {
        "REDACTED" => CorpusStatus::Redacted,
        "ACCEPTED-UNCOVERED" => CorpusStatus::AcceptedUncovered,
        "KNOWN-FAILING" => CorpusStatus::KnownFailing,
        unknown => panic!("unknown corpus classification `{unknown}`"),
    };
    let reason = fields
        .next()
        .expect("classification line must carry its cited reason")
        .to_string();
    assert_eq!(
        line, expected_line,
        "classification lines must exactly parallel corpus lines"
    );
    assert!(
        reason.contains("redaction.rs") || reason.contains("runtime-substrate.md"),
        "classification reason must cite its in-tree authority or mechanism"
    );
    CorpusExpectation {
        line,
        status,
        reason,
    }
}

fn corpus_expectations() -> Vec<CorpusExpectation> {
    CLASSIFICATIONS
        .lines()
        .enumerate()
        .map(|(index, line)| parse_expectation_line(line, index + 1))
        .collect()
}

fn stateless_outputs(input: &str) -> Vec<ObservedOutput> {
    let mut observed = Vec::<Observation<u8>>::new();
    let sink = RedactingSink::new(&mut observed);
    let tool_arguments = match sink.redact_tool_arguments("", input) {
        ToolArgumentRedaction::Admitted(text) => text,
        ToolArgumentRedaction::Suppressed => REDACTED_JSON_OBJECT.to_string(),
    };
    vec![
        ObservedOutput {
            channel: "text",
            text: redact_text(input),
        },
        ObservedOutput {
            channel: "json",
            text: redact_json(input),
        },
        ObservedOutput {
            channel: "failure message",
            text: sink.redact_terminal_failure_text(input),
        },
        ObservedOutput {
            channel: "tool arguments",
            text: tool_arguments,
        },
        ObservedOutput {
            channel: "provider id",
            text: sink.redact_provider_id("", input),
        },
    ]
}

fn emit_text_delta(sink: &mut RedactingSink<'_, u8>, index: &mut u32, text: String) {
    sink.observe(Observation {
        correlation: CORRELATION,
        fact: ObservationFact::TextDelta {
            index: *index,
            text,
        },
    });
    *index += 1;
}

fn observed_stream_outputs(observed: Vec<Observation<u8>>) -> Vec<ObservedOutput> {
    observed
        .into_iter()
        .filter_map(|observation| match observation.fact {
            ObservationFact::TextDelta { text, .. } => Some(ObservedOutput {
                channel: "streamed text",
                text,
            }),
            ObservationFact::ThinkingDelta { text, .. } => Some(ObservedOutput {
                channel: "streamed thinking",
                text,
            }),
            ObservationFact::ToolArgumentsDelta { fragment, .. } => Some(ObservedOutput {
                channel: "streamed tool arguments",
                text: fragment,
            }),
            _ => None,
        })
        .collect()
}

fn stateful_outputs(case: CorpusCase) -> Vec<ObservedOutput> {
    let mut observed = Vec::new();
    let mut direct_outputs = Vec::new();
    let terminal_capture;
    {
        let mut sink = RedactingSink::new(&mut observed);
        sink.begin_terminal_text_capture(true);
        let mut text = String::new();
        let mut index = 0_u32;
        let mut terminal_context = None;
        for part in case.parts {
            match part {
                CorpusPart::Text(fragment) => text.push_str(&fragment),
                CorpusPart::Event(CorpusEvent::Delta) => {
                    emit_text_delta(&mut sink, &mut index, std::mem::take(&mut text));
                }
                CorpusPart::Event(CorpusEvent::Boundary) => {
                    if !text.is_empty() {
                        emit_text_delta(&mut sink, &mut index, std::mem::take(&mut text));
                    }
                    sink.observe(Observation {
                        correlation: CORRELATION,
                        fact: ObservationFact::ToolArgumentsDelta {
                            index,
                            fragment: BOUNDARY_FRAGMENT.to_string(),
                        },
                    });
                }
                CorpusPart::Event(CorpusEvent::Usage) => {
                    if !text.is_empty() {
                        emit_text_delta(&mut sink, &mut index, std::mem::take(&mut text));
                    }
                    sink.observe(Observation {
                        correlation: CORRELATION,
                        fact: ObservationFact::UsageReported(TokenUsage::unreported()),
                    });
                }
                CorpusPart::Event(CorpusEvent::Suppress) => {
                    if !text.is_empty() {
                        emit_text_delta(&mut sink, &mut index, std::mem::take(&mut text));
                    }
                    sink.suppress_remaining();
                }
                CorpusPart::Event(CorpusEvent::Seed) => {
                    let sanitized = sink.redact_provider_id("", &text);
                    sink.seed_emitted_context(&text);
                    direct_outputs.push(ObservedOutput {
                        channel: "provider id",
                        text: sanitized,
                    });
                    text.clear();
                    terminal_context = Some(TerminalContext::Seed);
                }
                CorpusPart::Event(CorpusEvent::Drop) => {
                    sink.extend_dropped_context(&text);
                    text.clear();
                    terminal_context = Some(TerminalContext::Drop);
                }
            }
        }
        match terminal_context {
            Some(TerminalContext::Seed | TerminalContext::Drop) => {
                direct_outputs.push(ObservedOutput {
                    channel: "failure message",
                    text: sink.redact_terminal_failure_text(&text),
                });
            }
            None if !text.is_empty() => emit_text_delta(&mut sink, &mut index, text),
            None => {}
        }
        sink.finish();
        terminal_capture = sink.take_terminal_text_capture().into_text();
    }
    let mut outputs = observed_stream_outputs(observed);
    outputs.push(ObservedOutput {
        channel: "terminal completion text",
        text: terminal_capture,
    });
    outputs.extend(direct_outputs);
    outputs
}

fn outputs_for(case: CorpusCase) -> Vec<ObservedOutput> {
    match case.mode {
        CorpusMode::Stateful => stateful_outputs(case),
        CorpusMode::Stateless => {
            let input = match case.parts.as_slice() {
                [CorpusPart::Text(input)] => input,
                _ => panic!("a stateless corpus line must decode to exactly one text part"),
            };
            stateless_outputs(input)
        }
    }
}

fn status_for(outputs: &[ObservedOutput]) -> CorpusStatus {
    if outputs
        .iter()
        .any(|output| output.text.contains(SYNTHETIC_SECRET_MARKER))
    {
        CorpusStatus::AcceptedUncovered
    } else {
        CorpusStatus::Redacted
    }
}

fn surviving_channels(outputs: &[ObservedOutput]) -> Vec<&'static str> {
    outputs
        .iter()
        .filter(|output| output.text.contains(SYNTHETIC_SECRET_MARKER))
        .map(|output| output.channel)
        .collect()
}

fn run_corpus() -> CorpusSummary {
    let expectations = corpus_expectations();
    let mut redacted = 0;
    let mut accepted_uncovered = 0;
    let mut known_failing = Vec::new();
    let mut mismatches = Vec::new();
    let corpus_lines: Vec<&str> = CORPUS.lines().collect();
    assert_eq!(
        corpus_lines.len(),
        expectations.len(),
        "each corpus line must have exactly one classification"
    );
    for (index, (encoded, expectation)) in corpus_lines.into_iter().zip(expectations).enumerate() {
        assert!(
            encoded.contains(SYNTHETIC_SECRET_MARKER),
            "corpus line {} must carry the synthetic marker",
            index + 1
        );
        let outputs = outputs_for(decode_corpus_line(encoded));
        let actual = status_for(&outputs);
        if expectation.status == CorpusStatus::KnownFailing {
            known_failing.push(KnownFailure {
                line: expectation.line,
                reason: expectation.reason.clone(),
                surviving_channels: surviving_channels(&outputs),
            });
            // A ledger entry that no longer leaks is progress, and the ledger
            // must shrink to match rather than keep claiming the damage.
            if actual == CorpusStatus::Redacted {
                mismatches.push(ClassificationMismatch {
                    line: expectation.line,
                    expected: CorpusStatus::KnownFailing,
                    actual,
                    reason: expectation.reason,
                    surviving_channels: Vec::new(),
                });
            }
            continue;
        }
        match actual {
            CorpusStatus::Redacted => redacted += 1,
            CorpusStatus::AcceptedUncovered | CorpusStatus::KnownFailing => {
                accepted_uncovered += 1;
            }
        }
        if actual != expectation.status {
            mismatches.push(ClassificationMismatch {
                line: expectation.line,
                expected: expectation.status,
                actual,
                reason: expectation.reason,
                surviving_channels: surviving_channels(&outputs),
            });
        }
    }
    CorpusSummary {
        lines: redacted + accepted_uncovered + known_failing.len(),
        redacted,
        accepted_uncovered,
        known_failing,
        mismatches,
    }
}

fn assert_original_codex_corpus_is_still_covered() {
    let summary = run_corpus();
    assert!(
        summary.mismatches.is_empty(),
        "original Codex corpus coverage regressed: {:#?}",
        summary.mismatches
    );
}

fn assert_original_claude_corpus_is_still_covered() {
    for text in ORIGINAL_CLAUDE_REDACTION_CORPUS {
        assert!(
            !redact_text(text).contains(SYNTHETIC_SECRET_MARKER),
            "merged stateless engine redacts less than the original Claude engine for {text:?}"
        );
        let boundaries = split_boundaries(text);
        for split in boundaries {
            let output =
                fragmented_stateful_output(&[text[..split].to_string(), text[split..].to_string()]);
            assert!(
                !output.contains(SYNTHETIC_SECRET_MARKER),
                "merged stateful engine redacts less than the original Claude engine for \
                 {text:?} at split {split}"
            );
        }
        let boundaries = split_boundaries(text);
        for (first_position, first) in boundaries.iter().copied().enumerate() {
            for second in boundaries[first_position..].iter().copied() {
                let output = fragmented_stateful_output(&[
                    text[..first].to_string(),
                    text[first..second].to_string(),
                    text[second..].to_string(),
                ]);
                assert!(
                    !output.contains(SYNTHETIC_SECRET_MARKER),
                    "merged stateful engine redacts less than the original Claude engine for \
                     {text:?} at splits {first} and {second}"
                );
            }
        }
    }
}

/// The joined text a corpus line denotes, with its committed event schedule
/// dropped: split enumeration supplies its own fragmentation, so a line's
/// pinned delta positions must not constrain the boundaries enumerated here.
fn corpus_line_text(encoded: &str) -> String {
    let mut text = String::new();
    for part in decode_corpus_line(encoded).parts {
        match part {
            CorpusPart::Text(fragment) => text.push_str(&fragment),
            CorpusPart::Event(_) => {}
        }
    }
    text
}

/// Every byte position a delta may end at, which for UTF-8 is every character
/// boundary. Both ends are included: an empty leading or trailing delta is a
/// fragmentation a provider can produce.
fn split_boundaries(text: &str) -> Vec<usize> {
    (0..=text.len())
        .filter(|split| text.is_char_boundary(*split))
        .collect()
}

/// Whether the joined line obliges the stateful path at all. `redact_text` is
/// the stateless reference, so a line it leaves marked is enumerated for its
/// case count but carries no assertion.
fn split_case_is_guarded(text: &str) -> bool {
    !redact_text(text).contains(SYNTHETIC_SECRET_MARKER)
}

fn fragmented_stateful_output(fragments: &[String]) -> String {
    let mut observed = Vec::new();
    let terminal_capture;
    {
        let mut sink = RedactingSink::new(&mut observed);
        sink.begin_terminal_text_capture(true);
        let mut index = 0_u32;
        for fragment in fragments {
            emit_text_delta(&mut sink, &mut index, fragment.clone());
        }
        sink.finish();
        terminal_capture = sink.take_terminal_text_capture().into_text();
    }
    let mut emitted = observed_stream_outputs(observed)
        .into_iter()
        .map(|output| output.text)
        .collect::<String>();
    emitted.push_str(&terminal_capture);
    emitted
}

fn record_split_case(
    summary: &mut SplitSummary,
    line: usize,
    guarded: bool,
    splits: Vec<usize>,
    fragments: Vec<String>,
) {
    summary.cases += 1;
    if !guarded {
        return;
    }
    summary.guarded_cases += 1;
    let emitted = fragmented_stateful_output(&fragments);
    if emitted.contains(SYNTHETIC_SECRET_MARKER) {
        summary.leaks.push(SplitLeak {
            line,
            splits,
            fragments,
            emitted,
        });
    }
}

/// Exhaustive single-split enumeration: every corpus line cut once at every
/// UTF-8 boundary, driven as two text deltas and a finish.
fn run_exhaustive_single_splits() -> SplitSummary {
    let mut summary = SplitSummary {
        lines: 0,
        cases: 0,
        guarded_cases: 0,
        leaks: Vec::new(),
    };
    for (index, encoded) in CORPUS.lines().enumerate() {
        summary.lines += 1;
        let text = corpus_line_text(encoded);
        let guarded = split_case_is_guarded(&text);
        for split in split_boundaries(&text) {
            let fragments = vec![text[..split].to_string(), text[split..].to_string()];
            record_split_case(&mut summary, index + 1, guarded, vec![split], fragments);
        }
    }
    summary
}

/// Exhaustive two-split enumeration: every corpus line cut at every ordered
/// pair of UTF-8 boundaries, driven as three text deltas and a finish. The
/// two-split space is still small enough to enumerate whole, so the soak
/// enumerates it rather than sampling it.
fn run_exhaustive_two_splits() -> SplitSummary {
    let mut summary = SplitSummary {
        lines: 0,
        cases: 0,
        guarded_cases: 0,
        leaks: Vec::new(),
    };
    for (index, encoded) in CORPUS.lines().enumerate() {
        summary.lines += 1;
        let text = corpus_line_text(encoded);
        let guarded = split_case_is_guarded(&text);
        let boundaries = split_boundaries(&text);
        for (first_position, first) in boundaries.iter().copied().enumerate() {
            for second in boundaries[first_position..].iter().copied() {
                let fragments = vec![
                    text[..first].to_string(),
                    text[first..second].to_string(),
                    text[second..].to_string(),
                ];
                record_split_case(
                    &mut summary,
                    index + 1,
                    guarded,
                    vec![first, second],
                    fragments,
                );
            }
        }
    }
    summary
}

fn generated_ascii(rng: &mut DeterministicGenerator) -> String {
    let length = rng.index(9);
    let mut value = String::with_capacity(length);
    for _ in 0..length {
        value.push(ASCII_NOISE[rng.index(ASCII_NOISE.len())] as char);
    }
    value
}

fn generated_input(family: GeneratedFamily, marker: &str, noise: &str) -> String {
    let value = format!("AAAA-{marker}-BBBB");
    match family {
        GeneratedFamily::ApiKey => format!("{noise}: api_key={value} tail"),
        GeneratedFamily::EscapedApiKey => {
            format!("{noise}: note \\u0061pi_key={value} tail")
        }
        GeneratedFamily::TokenName => format!("{noise}: GITHUB_TOKEN={value} tail"),
        GeneratedFamily::SpacedPasswordFlag => {
            format!("{noise}: codex --password \t {value} tail")
        }
        GeneratedFamily::UrlUserinfo => {
            format!("postgres://user:{value}@host/{noise}")
        }
        GeneratedFamily::MalformedQuotedJsonKey => {
            format!(r#"{{"x,"client_secret":"{value}","note":"{noise}"}}"#)
        }
        GeneratedFamily::MysqlPassword => format!("{noise}: MYSQL_PWD={value} tail"),
        GeneratedFamily::SigningKey => format!("{noise}: signing_key={value} tail"),
    }
}

fn generated_interlude(rng: &mut DeterministicGenerator) -> GeneratedInterlude {
    GENERATED_INTERLUDES[rng.index(GENERATED_INTERLUDES.len())]
}

fn generate_case(ordinal: usize, rng: &mut DeterministicGenerator) -> GeneratedCase {
    let family = GeneratedFamily::from_ordinal(ordinal);
    let marker = format!("{SYNTHETIC_SECRET_MARKER}-GENERATED-{ordinal:08x}");
    let noise = generated_ascii(rng);
    let input = generated_input(family, &marker, &noise);
    let mut chunks = Vec::new();
    let mut interludes = Vec::new();
    let mut offset = 0;
    while offset < input.len() {
        if rng.index(5) == 0 {
            chunks.push(String::new());
            interludes.push(generated_interlude(rng));
        }
        let remaining = input.len() - offset;
        let chunk_length = 1 + rng.index(remaining.min(17));
        let end = offset + chunk_length;
        chunks.push(input[offset..end].to_string());
        interludes.push(generated_interlude(rng));
        offset = end;
    }
    GeneratedCase {
        ordinal,
        family,
        marker,
        input,
        chunks,
        interludes,
    }
}

fn observe_generated_interlude(
    sink: &mut RedactingSink<'_, u8>,
    interlude: GeneratedInterlude,
    index: u32,
) {
    match interlude {
        GeneratedInterlude::None => {}
        GeneratedInterlude::ToolArgumentsBoundary => sink.observe(Observation {
            correlation: CORRELATION,
            fact: ObservationFact::ToolArgumentsDelta {
                index,
                fragment: BOUNDARY_FRAGMENT.to_string(),
            },
        }),
        GeneratedInterlude::UsageReported => sink.observe(Observation {
            correlation: CORRELATION,
            fact: ObservationFact::UsageReported(TokenUsage::unreported()),
        }),
        GeneratedInterlude::Finish => sink.finish(),
    }
}

fn generated_stateful_output(case: &GeneratedCase) -> String {
    let mut observed = Vec::new();
    let terminal_capture;
    {
        let mut sink = RedactingSink::new(&mut observed);
        sink.begin_terminal_text_capture(true);
        for (index, (chunk, interlude)) in case
            .chunks
            .iter()
            .zip(case.interludes.iter().copied())
            .enumerate()
        {
            sink.observe(Observation {
                correlation: CORRELATION,
                fact: ObservationFact::TextDelta {
                    index: index as u32,
                    text: chunk.clone(),
                },
            });
            observe_generated_interlude(&mut sink, interlude, index as u32);
        }
        sink.finish();
        terminal_capture = sink.take_terminal_text_capture().into_text();
    }
    let mut output = observed_stream_outputs(observed)
        .into_iter()
        .map(|observed| observed.text)
        .collect::<String>();
    output.push_str(&terminal_capture);
    output
}

#[track_caller]
fn assert_stateful_equals_stateless(seed: u64, case_count: usize) {
    let mut rng = DeterministicGenerator::seeded(seed);
    for ordinal in 0..case_count {
        let case = generate_case(ordinal, &mut rng);
        let stateless = redact_text(&case.input);
        let stateful = generated_stateful_output(&case);
        assert!(
            !stateless.contains(&case.marker),
            "generated stateless redaction retained the planted marker for case \
             {} ({:?}); input={:?}, stateless={:?}",
            case.ordinal,
            case.family,
            case.input,
            stateless
        );
        assert!(
            !stateful.contains(&case.marker),
            "STATEFUL-EQUALS-STATELESS failed for generated case {} ({:?}); \
             input={:?}, chunks={:?}, interludes={:?}, stateless={:?}, stateful={:?}",
            case.ordinal,
            case.family,
            case.input,
            case.chunks,
            case.interludes,
            stateless,
            stateful
        );
    }
}

fn suppression_actions(
    case: &GeneratedCase,
    rng: &mut DeterministicGenerator,
) -> Vec<SuppressionAction> {
    let mut actions = Vec::new();
    for chunk_index in 0..case.chunks.len() {
        let barrier = SUPPRESSION_BARRIERS[rng.index(SUPPRESSION_BARRIERS.len())];
        actions.push(barrier);
        if rng.index(3) == 0 {
            actions.push(barrier);
        }
        actions.push(SuppressionAction::ProviderChunk(chunk_index));
    }
    actions
}

fn apply_suppression_action(
    sink: &mut RedactingSink<'_, u8>,
    case: &GeneratedCase,
    action: SuppressionAction,
    index: &mut u32,
) {
    match action {
        SuppressionAction::UsageReported => sink.observe(Observation {
            correlation: CORRELATION,
            fact: ObservationFact::UsageReported(TokenUsage::unreported()),
        }),
        SuppressionAction::Finish => sink.finish(),
        SuppressionAction::ProviderChunk(chunk_index) => {
            emit_text_delta(sink, index, case.chunks[chunk_index].clone());
        }
    }
}

#[track_caller]
fn assert_suppression_is_absorbing(seed: u64, case_count: usize) {
    let mut rng = DeterministicGenerator::seeded(seed);
    for ordinal in 0..case_count {
        let case = generate_case(ordinal, &mut rng);
        let actions = suppression_actions(&case, &mut rng);
        let mut observed = Vec::new();
        let terminal_capture;
        {
            let mut sink = RedactingSink::new(&mut observed);
            sink.begin_terminal_text_capture(true);
            sink.suppress_remaining();
            assert!(
                sink.is_suppressing(),
                "SUPPRESSION IS ABSORBING must begin suppressed"
            );
            let mut index = 0_u32;
            for action in actions {
                apply_suppression_action(&mut sink, &case, action, &mut index);
                assert!(
                    sink.is_suppressing(),
                    "SUPPRESSION IS ABSORBING failed for generated case {} ({:?}) \
                     after {:?}",
                    case.ordinal,
                    case.family,
                    action
                );
            }
            sink.finish();
            assert!(
                sink.is_suppressing(),
                "SUPPRESSION IS ABSORBING failed at the terminal finish for \
                 generated case {} ({:?})",
                case.ordinal,
                case.family
            );
            terminal_capture = sink.take_terminal_text_capture().into_text();
        }
        let mut output = observed_stream_outputs(observed)
            .into_iter()
            .map(|observed| observed.text)
            .collect::<String>();
        output.push_str(&terminal_capture);
        assert!(
            !output.contains(&case.marker),
            "SUPPRESSION IS ABSORBING emitted the marker for generated case {} \
             ({:?}): {:?}",
            case.ordinal,
            case.family,
            output
        );
    }
}

fn generator_replay(seed: u64) -> String {
    let mut rng = DeterministicGenerator::seeded(seed);
    let case = generate_case(0, &mut rng);
    let chunk_lengths = case.chunks.iter().map(String::len).collect::<Vec<_>>();
    format!(
        "ordinal={}; family={:?}; input={:?}; chunk_lengths={:?}; interludes={:?}",
        case.ordinal, case.family, case.input, chunk_lengths, case.interludes
    )
}

#[test]
fn corpus_decoder_decodes_byte_and_event_tokens() {
    let decoded = decode_corpus_line(
        "a<|NL|>b<|CR|><|TAB|><|SOH|><|ZWSP|><|SHY|><|NBHY|><|MIDDOT|><|RLO|><|D|><|D|>c<|B|><|U|><|SUPPRESS|><|SEED|><|DROP|>d",
    );

    assert_eq!(
        decoded,
        CorpusCase {
            parts: vec![
                CorpusPart::Text(
                    "a\nb\r\t\u{0001}\u{200b}\u{00ad}\u{2011}\u{00b7}\u{202e}".to_string()
                ),
                CorpusPart::Event(CorpusEvent::Delta),
                CorpusPart::Text(String::new()),
                CorpusPart::Event(CorpusEvent::Delta),
                CorpusPart::Text("c".to_string()),
                CorpusPart::Event(CorpusEvent::Boundary),
                CorpusPart::Text(String::new()),
                CorpusPart::Event(CorpusEvent::Usage),
                CorpusPart::Text(String::new()),
                CorpusPart::Event(CorpusEvent::Suppress),
                CorpusPart::Text(String::new()),
                CorpusPart::Event(CorpusEvent::Seed),
                CorpusPart::Text(String::new()),
                CorpusPart::Event(CorpusEvent::Drop),
                CorpusPart::Text("d".to_string()),
            ],
            mode: CorpusMode::Stateful,
        }
    );
}

#[test]
fn classification_parser_requires_the_parallel_line_and_cited_reason() {
    let parsed = parse_expectation_line(
        "007 | ACCEPTED-UNCOVERED | redaction.rs::credential_key: accepted scope limit.",
        7,
    );

    assert_eq!(
        parsed,
        CorpusExpectation {
            line: 7,
            status: CorpusStatus::AcceptedUncovered,
            reason: "redaction.rs::credential_key: accepted scope limit.".to_string(),
        }
    );
}

#[test]
fn stateless_driver_classifies_redacted_and_uncovered_shapes() {
    let redacted = outputs_for(decode_corpus_line(
        "api_key=SYNTHETIC-SECRET-HELPER-REDACTED",
    ));
    let uncovered = outputs_for(decode_corpus_line(
        "Authentication: SYNTHETIC-SECRET-HELPER-UNCOVERED",
    ));

    assert_eq!(status_for(&redacted), CorpusStatus::Redacted);
    assert_eq!(status_for(&uncovered), CorpusStatus::AcceptedUncovered);
}

#[test]
fn stateful_driver_preserves_empty_deltas() {
    let held = outputs_for(decode_corpus_line(
        "ap<|D|><|D|>i_key=<|D|>SYNTHETIC-SECRET-HELPER-HELD",
    ));

    assert_eq!(status_for(&held), CorpusStatus::Redacted);
}

#[test]
fn stateful_driver_redacts_escaped_held_text() {
    let escaped = outputs_for(decode_corpus_line(
        "note \\u0061pi_key=<|D|>SYNTHETIC-SECRET-HELPER-RELEASED",
    ));

    assert_eq!(status_for(&escaped), CorpusStatus::Redacted);
}

/// Split enumeration cuts the text a corpus line denotes, not its encoding:
/// an event token contributes no bytes and an escape contributes its
/// character, so the boundaries enumerated are the ones a provider can split.
#[test]
fn corpus_line_text_drops_events_and_decodes_escapes() {
    let text = corpus_line_text("api_key=<|D|>value<|TAB|>tail");

    assert_eq!(text, "api_key=value\ttail");
}

/// Split positions are every character boundary, both ends included, so a
/// multibyte character is never cut in half and no fragmentation is skipped.
#[test]
fn split_boundaries_span_both_ends_and_skip_continuation_bytes() {
    let boundaries = split_boundaries("añb");

    assert_eq!(boundaries, vec![0, 1, 3, 4]);
}

/// Every synthetic corpus line has an explicit, exact final
/// classification across all redaction output surfaces it can drive.
#[test]
fn redaction_corpus_classification_is_exact() {
    let summary = run_corpus();

    assert_eq!(summary.lines, CORPUS_LINE_COUNT);
    assert!(
        summary.mismatches.is_empty(),
        "corpus classifications changed in either direction: {:#?}",
        summary.mismatches
    );
    assert_eq!(summary.redacted, EXPECTED_REDACTED_COUNT);
    assert_eq!(
        summary.accepted_uncovered,
        EXPECTED_ACCEPTED_UNCOVERED_COUNT
    );
    assert_eq!(
        summary.known_failing.len(),
        EXPECTED_KNOWN_FAILING_COUNT,
        "KNOWN-FAILING is a defect ledger for shapes the contract covers and \
         the sink leaks — never an ACCEPTED-UNCOVERED classification, which \
         records what the specification openly declines. The ledger matches \
         exactly or this fails: {:#?}",
        summary.known_failing
    );
}

/// The merged engine is monotone over the union of both original adapters'
/// recorded behavior: no case either original redacted may become visible.
#[test]
fn merged_engine_redacts_at_least_the_original_engine_union() {
    assert_original_codex_corpus_is_still_covered();
    assert_original_claude_corpus_is_still_covered();
}

#[test]
fn deterministic_generator_replays_the_pinned_seed() {
    let replay = generator_replay(GENERATOR_SEED);

    assert_eq!(
        replay,
        "ordinal=0; family=ApiKey; input=\"qr8r_v8: api_key=AAAA-SYNTHETIC-SECRET-GENERATED-00000000-BBBB tail\"; chunk_lengths=[8, 13, 14, 0, 8, 6, 0, 10, 5, 1, 1, 1]; interludes=[Finish, None, ToolArgumentsBoundary, Finish, ToolArgumentsBoundary, UsageReported, None, UsageReported, Finish, None, ToolArgumentsBoundary, UsageReported]"
    );
}

/// STATEFUL-EQUALS-STATELESS over the whole single-split class: every corpus
/// line cut once at every UTF-8 boundary must emit no marker that
/// `redact_text` removes from the joined line. This class is enumerated, not
/// sampled — a sampled split point is exactly what a boundary defect hides
/// behind — so no single-delta boundary in the committed corpus is untested.
#[test]
fn stateful_equals_stateless_for_every_single_corpus_split() {
    let summary = run_exhaustive_single_splits();

    assert_eq!(summary.lines, CORPUS_LINE_COUNT);
    assert_eq!(summary.cases, EXHAUSTIVE_SINGLE_SPLIT_CASES);
    assert_eq!(summary.guarded_cases, EXHAUSTIVE_SINGLE_SPLIT_GUARDED_CASES);
    assert_eq!(
        summary.divergent_splits(),
        KNOWN_FAILING_SPLITS.to_vec(),
        "leaks: {:#?}",
        summary.leaks
    );
}

/// STATEFUL-EQUALS-STATELESS: every generated delta/event schedule must
/// remove a planted marker that `redact_text` also removes from the joined
/// input; every corpus-seeded family is required to satisfy both sides. The
/// generator covers the multi-split and event-schedule shapes whose space is
/// too large to enumerate; the single-split class is enumerated instead.
#[test]
fn stateful_equals_stateless_for_generated_corpus_families() {
    assert_stateful_equals_stateless(GENERATOR_SEED, DEFAULT_GENERATIVE_CASES);
}

/// SUPPRESSION IS ABSORBING: after fail-closed suppression, repeated usage
/// reports, explicit finishes, and provider deltas never re-arm emission.
#[test]
fn suppression_is_absorbing_for_generated_event_sequences() {
    assert_suppression_is_absorbing(GENERATOR_SEED, DEFAULT_GENERATIVE_CASES);
}

/// Exhaustive two-split coverage for STATEFUL-EQUALS-STATELESS. The
/// single-split class runs by default; the two-split class is the same
/// construction one delta deeper and is still small enough to enumerate
/// whole, so the soak enumerates it rather than sampling it.
#[test]
#[ignore = "exhaustive two-split redaction enumeration"]
fn stateful_equals_stateless_for_every_two_corpus_splits() {
    let summary = run_exhaustive_two_splits();

    assert_eq!(summary.lines, CORPUS_LINE_COUNT);
    assert_eq!(summary.cases, EXHAUSTIVE_TWO_SPLIT_CASES);
    assert_eq!(summary.guarded_cases, EXHAUSTIVE_TWO_SPLIT_GUARDED_CASES);
    assert_exact_two_split_leak_ledger(&summary);
}

/// Deterministic long-run coverage for STATEFUL-EQUALS-STATELESS.
#[test]
#[ignore = "deterministic 32,768-case redaction soak"]
fn stateful_equals_stateless_generated_soak() {
    assert_stateful_equals_stateless(GENERATOR_SEED, SOAK_GENERATIVE_CASES);
}

/// Deterministic long-run coverage for SUPPRESSION IS ABSORBING.
#[test]
#[ignore = "deterministic 32,768-case redaction soak"]
fn suppression_is_absorbing_generated_soak() {
    assert_suppression_is_absorbing(GENERATOR_SEED, SOAK_GENERATIVE_CASES);
}
