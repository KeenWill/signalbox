use signalbox_model_runtime::{Observation, ObservationFact, ObservationSink, TokenUsage};

use super::{RedactingSink, redact_json, redact_text};

const CORPUS: &str = include_str!("testdata/redaction-corpus.txt");
const CLASSIFICATIONS: &str = include_str!("testdata/redaction-corpus.classifications");
const SYNTHETIC_SECRET_MARKER: &str = "SYNTHETIC-SECRET";
const CORPUS_LINE_COUNT: usize = 130;
const BASELINE_REDACTED_COUNT: usize = 57;
const BASELINE_ACCEPTED_UNCOVERED_COUNT: usize = 73;
const CORRELATION: u8 = 7;
const BOUNDARY_FRAGMENT: &str = "{}";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CorpusStatus {
    Redacted,
    AcceptedUncovered,
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
    mismatches: Vec<ClassificationMismatch>,
}

#[derive(Clone, Copy)]
enum TerminalContext {
    Seed,
    Drop,
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
            text: sink.redact_tool_arguments("", input),
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
        sink.begin_terminal_text_capture();
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
                    let sanitized = sink.redact_terminal_failure_text(&text);
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
        terminal_capture = sink.take_terminal_text_capture();
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
        match actual {
            CorpusStatus::Redacted => redacted += 1,
            CorpusStatus::AcceptedUncovered => accepted_uncovered += 1,
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
        lines: redacted + accepted_uncovered,
        redacted,
        accepted_uncovered,
        mismatches,
    }
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
        "007 | ACCEPTED-UNCOVERED | redaction.rs::credential_key: baseline omission.",
        7,
    );

    assert_eq!(
        parsed,
        CorpusExpectation {
            line: 7,
            status: CorpusStatus::AcceptedUncovered,
            reason: "redaction.rs::credential_key: baseline omission.".to_string(),
        }
    );
}

#[test]
fn stateless_driver_classifies_redacted_and_uncovered_shapes() {
    let redacted = outputs_for(decode_corpus_line(
        "api_key=SYNTHETIC-SECRET-HELPER-REDACTED",
    ));
    let uncovered = outputs_for(decode_corpus_line(
        "token=SYNTHETIC-SECRET-HELPER-UNCOVERED",
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
fn stateful_driver_exposes_baseline_escape_release() {
    let escaped_release = outputs_for(decode_corpus_line(
        "note \\u0061pi_key=<|D|>SYNTHETIC-SECRET-HELPER-RELEASED",
    ));

    assert_eq!(
        status_for(&escaped_release),
        CorpusStatus::AcceptedUncovered
    );
}

/// Every synthetic corpus line has an explicit, exact baseline
/// classification across all redaction output surfaces it can drive.
#[test]
fn redaction_corpus_classification_is_exact() {
    let summary = run_corpus();

    assert_eq!(summary.lines, CORPUS_LINE_COUNT);
    assert_eq!(summary.redacted, BASELINE_REDACTED_COUNT);
    assert_eq!(
        summary.accepted_uncovered,
        BASELINE_ACCEPTED_UNCOVERED_COUNT
    );
    assert!(
        summary.mismatches.is_empty(),
        "corpus classifications changed in either direction: {:#?}",
        summary.mismatches
    );
}
