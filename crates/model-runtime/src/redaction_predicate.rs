//! Shadow credential predicate for the observation boundary in runtime-substrate.
//!
//! The same byte predicate scans two inputs: canonical JSON of ObservationFact
//! (excluding caller-owned correlation), and decoded provider content. Content
//! joins string leaves in lexical object-key / array order and observation
//! arrival order, without field separators. Embedded JSON objects, arrays, and
//! strings project their decoded keys and string values in place, joining content on
//! either side in the same window; plain text is not escape-decoded.
//! Text, thinking, and argument deltas additionally reconstruct separate streams
//! by (kind, part index), retaining their tails across intervening observations.
//! Argument streams decode JSON escapes, including escapes split across deltas.
//! Proposed arguments also decode when malformed and join the following fields
//! in canonical order; their decoded tail survives the proposal boundary.
//! One sink belongs to one physical request; its correlation is not provider data.
//!
//! This deliberately checks cross-field joins beyond consumer stream joins.
//! A hit on the existing redactor's output is a disagreement about output safety,
//! not a comparison of replacement markers or speculative prefix redactions.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::ObservationFact;

static DISAGREEMENTS: AtomicU64 = AtomicU64::new(0);

/// Number of forwarded facts that the shadow credential predicate found unsafe
/// since process start. Contains no credential, content, or identity labels.
pub fn credential_redaction_disagreements() -> u64 {
    DISAGREEMENTS.load(Ordering::Relaxed)
}

#[cfg_attr(
    test,
    expect(
        clippy::panic,
        reason = "A shadow disagreement must fail the runtime unit test without changing production forwarding."
    )
)]
pub(crate) fn record_disagreement() {
    DISAGREEMENTS.fetch_add(1, Ordering::Relaxed);
    #[cfg(test)]
    panic!("credential redaction predicate disagrees with forwarded output");
}

/// Only the last credential-length minus one bytes survive each push.
#[derive(Default)]
struct Window {
    tail: Vec<u8>,
}

impl Window {
    fn inspect(&mut self, bytes: &[u8], credential: &[u8]) -> bool {
        if credential.is_empty() {
            return false;
        }
        // slice::windows handles contiguous matches; combining only the prefix
        // needed for a boundary match avoids allocating an observation-sized copy.
        let retained = credential.len() - 1;
        let mut boundary = std::mem::take(&mut self.tail);
        boundary.extend_from_slice(&bytes[..bytes.len().min(retained)]);
        let found = boundary
            .windows(credential.len())
            .any(|part| part == credential)
            || bytes
                .windows(credential.len())
                .any(|part| part == credential);
        if bytes.len() >= retained {
            boundary.clear();
            boundary.extend_from_slice(&bytes[bytes.len() - retained..]);
        } else {
            let excess = boundary.len().saturating_sub(retained);
            boundary.drain(..excess);
        }
        self.tail = boundary;
        found
    }
}

/// serde_json decodes complete escape units; only an unfinished unit (at most
/// a surrogate pair) needs storage across provider chunks. Parsing a full Value
/// cannot handle partial or malformed streamed arguments and loses duplicate keys.
#[derive(Default)]
struct JsonEscapes {
    pending: String,
}

impl JsonEscapes {
    fn push(&mut self, text: &str, mut inspect: impl FnMut(&str)) {
        self.push_units(text, &mut inspect);
    }

    fn push_units(&mut self, text: &str, inspect: &mut dyn FnMut(&str)) {
        for character in text.chars() {
            if self.pending.is_empty() && character != '\\' {
                inspect(character.encode_utf8(&mut [0; 4]));
                continue;
            }
            self.pending.push(character);
            let bytes = self.pending.as_bytes();
            if bytes.len() == 1 {
                continue;
            }
            if bytes[1] == b'u' {
                if bytes.len() < 6 {
                    continue;
                }
                let high_surrogate = self
                    .pending
                    .get(2..6)
                    .and_then(|hex| u16::from_str_radix(hex, 16).ok())
                    .is_some_and(|unit| (0xd800..=0xdbff).contains(&unit));
                if high_surrogate && bytes.len() < 12 {
                    continue;
                }
            }
            match serde_json::from_str::<String>(&format!("\"{}\"", self.pending)) {
                Ok(decoded) => inspect(&decoded),
                Err(_) => {
                    // A malformed unit must not swallow a later valid escape.
                    // Each retry removes its leading backslash; recursion is
                    // bounded by the one pending escape unit, not the payload.
                    let remaining = self.pending.split_off(1);
                    self.pending.clear();
                    inspect("\\");
                    self.push_units(&remaining, inspect);
                    continue;
                }
            }
            self.pending.clear();
        }
    }
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum Kind {
    Text,
    Thinking,
    Arguments,
}

#[derive(Default)]
struct Stream {
    literal: Window,
    decoded: Window,
    escapes: JsonEscapes,
}

#[derive(Default)]
pub(crate) struct ObservationPredicate {
    serialized: Window,
    content: Window,
    escaped_content: Window,
    json_content: Window,
    streams: BTreeMap<(Kind, u32), Stream>,
}

impl ObservationPredicate {
    pub(crate) fn inspect(&mut self, fact: &ObservationFact, credential: &[u8]) -> bool {
        if credential.is_empty() {
            return false;
        }
        // Value's sorted keys define the projection order independently of
        // provider object-member order. This is an audit encoding, not a wire API.
        let Ok(mut value) = serde_json::to_value(fact) else {
            return true;
        };
        value.sort_all_objects();
        let Ok(serialized) = serde_json::to_vec(&value) else {
            return true;
        };
        let mut found = self.serialized.inspect(&serialized, credential);
        let streamed_arguments = matches!(fact, ObservationFact::ToolArgumentsDelta { .. });
        let mut join_decoded_stream = false;
        visit_strings(&value, None, &mut |field, text| {
            found |= self.content.inspect(text.as_bytes(), credential);
            let json = serde_json::from_str::<serde_json::Value>(text)
                .ok()
                // Scalar spellings in ordinary string fields retain their
                // literal bytes; only structured/string JSON projects leaves.
                .filter(|value| {
                    matches!(
                        value,
                        serde_json::Value::Object(_)
                            | serde_json::Value::Array(_)
                            | serde_json::Value::String(_)
                    )
                })
                .map(|mut value| {
                    value.sort_all_objects();
                    value
                });
            if !streamed_arguments {
                // The proposed-argument field retains its JSON role even when
                // incomplete or malformed. Decode it at its projection position
                // so its tail can join later fields and observations.
                let proposed_arguments = matches!(fact, ObservationFact::ToolCallProposed(_))
                    && field == Some("arguments_json");
                if proposed_arguments {
                    JsonEscapes::default().push(text, |unit| {
                        found |= self.escaped_content.inspect(unit.as_bytes(), credential);
                        if json.is_none() {
                            found |= self.json_content.inspect(unit.as_bytes(), credential);
                        }
                    });
                } else {
                    found |= self.escaped_content.inspect(text.as_bytes(), credential);
                    if json.is_none() {
                        found |= self.json_content.inspect(text.as_bytes(), credential);
                    }
                }
            }
            if let Some(json) = json {
                // Replace the embedded JSON at this field's position with its
                // leaves; its tail must join the following ordinary content.
                visit_json_content(&json, &mut |unit| {
                    found |= self.json_content.inspect(unit.as_bytes(), credential);
                });
            } else {
                join_decoded_stream = streamed_arguments;
            }
        });
        let stream = match fact {
            ObservationFact::TextDelta { index, text } => Some((Kind::Text, *index, text)),
            ObservationFact::ThinkingDelta { index, text } => Some((Kind::Thinking, *index, text)),
            ObservationFact::ToolArgumentsDelta { index, fragment } => {
                Some((Kind::Arguments, *index, fragment))
            }
            _ => None,
        };
        if let Some((kind, index, text)) = stream {
            let stream = self.streams.entry((kind, index)).or_default();
            found |= stream.literal.inspect(text.as_bytes(), credential);
            if kind == Kind::Arguments {
                stream.escapes.push(text, |unit| {
                    found |= stream.decoded.inspect(unit.as_bytes(), credential);
                    found |= self.escaped_content.inspect(unit.as_bytes(), credential);
                    if join_decoded_stream {
                        found |= self.json_content.inspect(unit.as_bytes(), credential);
                    }
                });
            }
        }
        found
    }
}

fn visit_strings(
    value: &serde_json::Value,
    field: Option<&str>,
    inspect: &mut impl FnMut(Option<&str>, &str),
) {
    match value {
        serde_json::Value::String(text) => inspect(field, text),
        serde_json::Value::Array(values) => {
            for value in values {
                visit_strings(value, field, inspect);
            }
        }
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                visit_strings(value, Some(key), inspect);
            }
        }
        _ => {}
    }
}

fn visit_json_content(value: &serde_json::Value, inspect: &mut impl FnMut(&str)) {
    match value {
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                inspect(key);
                visit_json_content(value, inspect);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                visit_json_content(value, inspect);
            }
        }
        serde_json::Value::String(text) => inspect(text),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CredentialRedactingSink, CredentialValue, Observation, ObservationSink,
        ProviderReportedModel,
    };
    use proptest::{
        prelude::*,
        test_runner::{Config, RngSeed, TestRunner},
    };

    /// Reproduces the split/escape audit; fixed independently of test environment.
    const PROPERTY_SEED: u64 = 16120260910;

    fn runner() -> TestRunner {
        TestRunner::new(Config {
            cases: 1000,
            rng_seed: RngSeed::Fixed(PROPERTY_SEED),
            failure_persistence: None,
            ..Config::default()
        })
    }

    fn credentials() -> impl Strategy<Value = String> {
        prop::collection::vec(
            prop::sample::select(vec!['a', 'b', '/', '"', '\\', 'é', '😀', '\n', '_', '0']),
            8..32,
        )
        .prop_map(|chars| chars.into_iter().collect())
    }

    fn chunks(text: &str, cuts: &[bool]) -> Vec<String> {
        let mut result = Vec::new();
        let mut start = 0;
        for (ordinal, (offset, _)) in text.char_indices().skip(1).enumerate() {
            if cuts[ordinal % cuts.len()] {
                result.push(text[start..offset].to_owned());
                start = offset;
            }
        }
        result.push(text[start..].to_owned());
        result
    }

    fn escaped(text: &str) -> String {
        text.encode_utf16()
            .map(|unit| format!("\\u{unit:04x}"))
            .collect()
    }

    fn delta(kind: Kind, index: u32, text: String) -> ObservationFact {
        match kind {
            Kind::Text => ObservationFact::TextDelta { index, text },
            Kind::Thinking => ObservationFact::ThinkingDelta { index, text },
            Kind::Arguments => ObservationFact::ToolArgumentsDelta {
                index,
                fragment: text,
            },
        }
    }

    #[test]
    fn split_and_escaped_streams_caught_by_redaction_are_caught_by_predicate() {
        runner()
            .run(
                &(
                    credentials(),
                    prop::collection::vec(any::<bool>(), 1..96),
                    0usize..3,
                    any::<bool>(),
                    any::<bool>(),
                ),
                |(secret, cuts, kind, use_escapes, interleave)| {
                    let kind = [Kind::Text, Kind::Thinking, Kind::Arguments][kind];
                    let source = if kind == Kind::Arguments {
                        if use_escapes {
                            format!("{{\"token\":\"{}\"}}", escaped(&secret))
                        } else {
                            serde_json::json!({"token": secret}).to_string()
                        }
                    } else {
                        secret.clone()
                    };
                    let mut input = Vec::new();
                    for fragment in chunks(&source, &cuts) {
                        input.push(Observation {
                            correlation: (),
                            fact: delta(kind, 0, fragment),
                        });
                        if interleave {
                            input.push(Observation {
                                correlation: (),
                                fact: ObservationFact::ProviderModelReported(
                                    ProviderReportedModel::new("unrelated metadata"),
                                ),
                            });
                            input.push(Observation {
                                correlation: (),
                                fact: delta(kind, 1, "other stream".to_owned()),
                            });
                        }
                    }
                    let mut predicate = ObservationPredicate::default();
                    let mut detected = false;
                    for observation in &input {
                        detected |= predicate.inspect(&observation.fact, secret.as_bytes());
                    }
                    prop_assert!(
                        detected,
                        "reconstructed stream must expose the injected credential"
                    );
                    let credential = CredentialValue::new(secret.into_bytes());
                    let mut observed = Vec::new();
                    let mut sink = CredentialRedactingSink::new(&mut observed, &credential);
                    for observation in &input {
                        sink.observe(observation.clone());
                    }
                    sink.flush();
                    drop(sink);
                    prop_assert_ne!(
                        observed,
                        input,
                        "the existing redactor must also catch this reflection"
                    );
                    Ok(())
                },
            )
            .unwrap();
    }

    #[test]
    fn decoded_joins_catch_credentials_across_arbitrary_part_kinds() {
        runner()
            .run(
                &(
                    credentials(),
                    prop::collection::vec(any::<bool>(), 1..96),
                    prop::collection::vec(0usize..9, 1..96),
                ),
                |(secret, cuts, kinds)| {
                    let mut predicate = ObservationPredicate::default();
                    let mut detected = false;
                    for (ordinal, fragment) in chunks(&secret, &cuts).into_iter().enumerate() {
                        let fact = match kinds[ordinal % kinds.len()] {
                            0 => delta(Kind::Text, ordinal as u32, fragment),
                            1 => delta(Kind::Thinking, ordinal as u32, fragment),
                            2 => delta(Kind::Arguments, ordinal as u32, escaped(&fragment)),
                            3 => ObservationFact::ProviderModelReported(
                                ProviderReportedModel::new(fragment),
                            ),
                            4 => ObservationFact::ExchangeEstablished(crate::ExchangeFacts {
                                provider_request_id: Some(crate::ProviderRequestId::new(fragment)),
                                ..crate::ExchangeFacts::default()
                            }),
                            5 => {
                                ObservationFact::FinishReported(crate::FinishReason::Unrecognized {
                                    provider_token: fragment,
                                })
                            }
                            slot => {
                                let mut proposal = crate::ToolCallProposal {
                                    id: crate::ToolCallId::new(""),
                                    name: crate::ToolName::new(""),
                                    arguments_json: String::new(),
                                };
                                match slot {
                                    6 => proposal.id = crate::ToolCallId::new(fragment),
                                    7 => proposal.name = crate::ToolName::new(fragment),
                                    _ => proposal.arguments_json = escaped(&fragment),
                                }
                                ObservationFact::ToolCallProposed(proposal)
                            }
                        };
                        detected |= predicate.inspect(&fact, secret.as_bytes());
                    }
                    prop_assert!(
                        detected,
                        "decoded field joins must expose the injected credential"
                    );
                    Ok(())
                },
            )
            .unwrap();
    }

    #[test]
    fn complete_json_prefixes_join_following_fields_and_facts() {
        runner()
            .run(
                &(
                    credentials(),
                    prop_oneof![
                        Just("0".to_owned()),
                        Just("true".to_owned()),
                        Just("null".to_owned()),
                        credentials().prop_map(|text| format!("secret{text}")),
                    ],
                    0usize..3,
                    any::<bool>(),
                    0usize..3,
                ),
                |(prefix, suffix, shape, use_escapes, following)| {
                    let secret = format!("{prefix}{suffix}");
                    let value = if use_escapes {
                        format!("\"{}\"", escaped(&prefix))
                    } else {
                        serde_json::to_string(&prefix).unwrap()
                    };
                    let arguments_json = match shape {
                        0 => value,
                        1 => format!("[{value}]"),
                        _ => format!("{{\"x\":{value}}}"),
                    };
                    let mut predicate = ObservationPredicate::default();
                    let proposal = ObservationFact::ToolCallProposed(crate::ToolCallProposal {
                        arguments_json,
                        id: crate::ToolCallId::new(if following == 0 { &suffix } else { "" }),
                        name: crate::ToolName::new(""),
                    });
                    let mut detected = predicate.inspect(&proposal, secret.as_bytes());
                    if following != 0 {
                        let fact = if following == 1 {
                            delta(Kind::Text, 0, suffix.to_owned())
                        } else {
                            delta(Kind::Arguments, 0, escaped(&suffix))
                        };
                        detected |= predicate.inspect(&fact, secret.as_bytes());
                    }
                    prop_assert!(detected, "JSON leaves must join the following content");
                    Ok(())
                },
            )
            .unwrap();
    }

    #[test]
    fn complete_json_prefix_joins_following_proposal_fields() {
        let mut predicate = ObservationPredicate::default();
        let fact = ObservationFact::ToolCallProposed(crate::ToolCallProposal {
            arguments_json: r#"{"x":"fixture_"}"#.to_owned(),
            id: crate::ToolCallId::new("secret"),
            name: crate::ToolName::new(""),
        });
        assert!(predicate.inspect(&fact, b"fixture_secret"));
    }

    #[test]
    fn ordinary_prefix_joins_complete_json_suffix() {
        let mut predicate = ObservationPredicate::default();
        assert!(!predicate.inspect(
            &delta(Kind::Text, 0, "fixture_".to_owned()),
            b"fixture_secret"
        ));
        assert!(predicate.inspect(
            &delta(Kind::Text, 1, r#"["secret"]"#.to_owned()),
            b"fixture_secret"
        ));
    }

    #[test]
    fn ordinary_content_separates_embedded_json_leaves() {
        let mut predicate = ObservationPredicate::default();
        for text in [r#""fixture_""#, "gap", r#""secret""#] {
            assert!(!predicate.inspect(&delta(Kind::Text, 0, text.to_owned()), b"fixture_secret"));
        }
    }

    #[test]
    #[should_panic(expected = "credential redaction predicate disagrees with forwarded output")]
    fn forwarded_json_prefix_and_identifier_suffix_fail_shadow_audit() {
        let credential = CredentialValue::new(b"fixture_secret".to_vec());
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &credential);
        sink.observe(Observation {
            correlation: (),
            fact: ObservationFact::ToolCallProposed(crate::ToolCallProposal {
                arguments_json: r#"{"x":"fixture_"}"#.to_owned(),
                id: crate::ToolCallId::new("secret"),
                name: crate::ToolName::new(""),
            }),
        });
    }

    #[test]
    fn serialized_syntax_is_checked_in_addition_to_content() {
        let mut predicate = ObservationPredicate::default();
        assert!(predicate.inspect(
            &delta(Kind::Text, 0, "safe".to_owned()),
            b"\"text\":\"safe\""
        ));
    }

    #[test]
    fn plain_text_escapes_are_not_decoded() {
        let mut predicate = ObservationPredicate::default();
        assert!(!predicate.inspect(
            &delta(Kind::Text, 0, r"fixture_\u0073ecret".to_owned()),
            b"fixture_secret"
        ));
    }

    #[test]
    fn malformed_proposed_json_cannot_hide_escaped_credentials() {
        for arguments in [
            r#"{"token":"fixture_\u0073ecret"#,
            r#"{"token":"\u000\u0066ixture_secret"#,
            r#"{"token":"\ud800\u0066ixture_secret"#,
        ] {
            let mut predicate = ObservationPredicate::default();
            let fact = ObservationFact::ToolCallProposed(crate::ToolCallProposal {
                id: crate::ToolCallId::new("call"),
                name: crate::ToolName::new("tool"),
                arguments_json: arguments.to_owned(),
            });
            assert!(
                predicate.inspect(&fact, b"fixture_secret"),
                "malformed argument fixture: {arguments}"
            );
        }
    }

    #[test]
    fn malformed_argument_prefix_joins_following_proposal_fields() {
        let mut predicate = ObservationPredicate::default();
        let fact = ObservationFact::ToolCallProposed(crate::ToolCallProposal {
            arguments_json: r#"{"token":"\u0066ixture_"#.to_owned(),
            id: crate::ToolCallId::new("sec"),
            name: crate::ToolName::new("ret"),
        });
        assert!(predicate.inspect(&fact, b"fixture_secret"));
    }

    #[test]
    fn plain_json_shaped_text_keeps_its_bytes_when_joining_decoded_arguments() {
        let mut predicate = ObservationPredicate::default();
        let mut found = false;
        for fact in [
            delta(Kind::Text, 0, "fixture_".to_owned()),
            delta(Kind::Text, 0, r#""\\""#.to_owned()),
            delta(Kind::Arguments, 1, r"\u0073ecret".to_owned()),
        ] {
            found |= predicate.inspect(&fact, br#"fixture_"\\"secret"#);
        }
        assert!(found);
    }

    #[test]
    fn rolling_window_matches_whole_input_for_every_byte_partition() {
        runner()
            .run(
                &(credentials(), prop::collection::vec(1usize..16, 1..64)),
                |(secret, sizes)| {
                    let bytes = format!("prefix{secret}suffix").into_bytes();
                    let mut window = Window::default();
                    let mut cursor = 0;
                    let mut ordinal = 0;
                    let mut found = false;
                    while cursor < bytes.len() {
                        let end = (cursor + sizes[ordinal % sizes.len()]).min(bytes.len());
                        found |= window.inspect(&bytes[cursor..end], secret.as_bytes());
                        prop_assert!(window.tail.len() < secret.len());
                        cursor = end;
                        ordinal += 1;
                    }
                    prop_assert!(found);
                    Ok(())
                },
            )
            .unwrap();
    }
}
