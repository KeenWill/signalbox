//! Checked payloads for journaled clock, random, deadline and event primitives.

use crate::{InlineFramePayload, ProgramRunId};
use serde::{Deserialize, Serialize};

/// Milliseconds since the Unix epoch, encoded as decimal text without precision loss.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct UnixMillis(pub u64);

impl UnixMillis {
    pub fn encode(self) -> InlineFramePayload {
        encode(&ClockPayload {
            unix_ms: self.0.to_string(),
        })
    }

    pub fn decode(payload: &InlineFramePayload) -> Option<Self> {
        let raw: ClockPayload = serde_json::from_slice(payload.as_bytes()).ok()?;
        Some(Self(decimal(&raw.unix_ms)?))
    }
}

/// One uniformly sampled full-width unsigned integer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RandomValue(pub u64);

impl RandomValue {
    pub fn encode(self) -> InlineFramePayload {
        encode(&RandomPayload {
            value: self.0.to_string(),
        })
    }

    pub fn decode(payload: &InlineFramePayload) -> Option<Self> {
        let raw: RandomPayload = serde_json::from_slice(payload.as_bytes()).ok()?;
        Some(Self(decimal(&raw.value)?))
    }
}

/// An absolute deadline retained in the request itself at admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SleepUntil(pub UnixMillis);

impl SleepUntil {
    pub fn encode(self) -> InlineFramePayload {
        encode(&SleepPayload {
            deadline_unix_ms: self.0.0.to_string(),
        })
    }

    pub fn decode(payload: &InlineFramePayload) -> Option<Self> {
        let raw: SleepPayload = serde_json::from_slice(payload.as_bytes()).ok()?;
        Some(Self(UnixMillis(decimal(&raw.deadline_unix_ms)?)))
    }
}

/// Retained answer deliveries from one program journal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgramEventSource {
    ProgramAnswers(ProgramRunId),
}

/// The next source answer strictly after this journal position (zero starts the stream).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AwaitProgramEvent {
    pub source: ProgramEventSource,
    pub after: u64,
}

impl AwaitProgramEvent {
    pub fn encode(self) -> InlineFramePayload {
        let ProgramEventSource::ProgramAnswers(run) = self.source;
        encode(&EventWaitPayload {
            source: EventSourcePayload::ProgramAnswers {
                run: run.into_uuid().to_string(),
            },
            after: self.after.to_string(),
        })
    }

    pub fn decode(payload: &InlineFramePayload) -> Option<Self> {
        let raw: EventWaitPayload = serde_json::from_slice(payload.as_bytes()).ok()?;
        let EventSourcePayload::ProgramAnswers { run } = raw.source;
        let id = uuid::Uuid::parse_str(&run).ok()?;
        if !id.to_string().eq_ignore_ascii_case(&run) {
            return None;
        }
        Some(Self {
            source: ProgramEventSource::ProgramAnswers(ProgramRunId::from_uuid(id)),
            after: decimal(&raw.after)?,
        })
    }
}

/// Exact retained answer bytes and their source journal position.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramEvent {
    pub position: u64,
    pub payload: InlineFramePayload,
}

impl ProgramEvent {
    pub fn encode(&self) -> InlineFramePayload {
        encode(&EventPayload {
            position: self.position.to_string(),
            payload: self.payload.as_bytes().to_vec(),
        })
    }

    pub fn decode(payload: &InlineFramePayload) -> Option<Self> {
        let raw: EventPayload = serde_json::from_slice(payload.as_bytes()).ok()?;
        let position = decimal(&raw.position)?;
        if position == 0 {
            return None;
        }
        Some(Self {
            position,
            payload: InlineFramePayload::new(raw.payload),
        })
    }
}

fn decimal(value: &str) -> Option<u64> {
    let parsed: u64 = value.parse().ok()?;
    (parsed.to_string() == value).then_some(parsed)
}

#[allow(
    clippy::unreachable,
    reason = "Private payload records contain only infallible JSON strings and bytes."
)]
fn encode(value: &impl Serialize) -> InlineFramePayload {
    // These private records contain only strings and bytes; JSON encoding is infallible.
    InlineFramePayload::new(serde_json::to_vec(value).unwrap_or_else(|_| unreachable!()))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClockPayload {
    unix_ms: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RandomPayload {
    value: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SleepPayload {
    deadline_unix_ms: String,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum EventSourcePayload {
    ProgramAnswers { run: String },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventWaitPayload {
    source: EventSourcePayload,
    after: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventPayload {
    position: String,
    payload: Vec<u8>,
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "Fixture UUIDs are literal valid identities."
)]
mod tests {
    use super::*;

    #[test]
    fn primitive_codecs_preserve_full_width_values() {
        let clock = UnixMillis(9_007_199_254_740_993);
        let random = RandomValue(u64::MAX);
        let deadline = SleepUntil(clock);
        assert_eq!(
            clock.encode().as_bytes(),
            br#"{"unix_ms":"9007199254740993"}"#
        );
        assert_eq!(UnixMillis::decode(&clock.encode()), Some(clock));
        assert_eq!(
            random.encode().as_bytes(),
            br#"{"value":"18446744073709551615"}"#
        );
        assert_eq!(RandomValue::decode(&random.encode()), Some(random));
        assert_eq!(
            deadline.encode().as_bytes(),
            br#"{"deadline_unix_ms":"9007199254740993"}"#
        );
        assert_eq!(SleepUntil::decode(&deadline.encode()), Some(deadline));
    }

    #[test]
    fn event_codecs_preserve_source_position_and_exact_bytes() {
        const SOURCE: &str = "12345678-1234-1234-1234-123456789abc";
        let wait = AwaitProgramEvent {
            source: ProgramEventSource::ProgramAnswers(ProgramRunId::from_uuid(
                uuid::Uuid::parse_str(SOURCE).expect("fixture UUID"),
            )),
            after: u64::MAX,
        };
        assert_eq!(wait.encode().as_bytes(), br#"{"source":{"kind":"program_answers","run":"12345678-1234-1234-1234-123456789abc"},"after":"18446744073709551615"}"#);
        assert_eq!(AwaitProgramEvent::decode(&wait.encode()), Some(wait));
        let uppercase_source = InlineFramePayload::new(
            br#"{"source":{"kind":"program_answers","run":"12345678-1234-1234-1234-123456789ABC"},"after":"18446744073709551615"}"#.as_slice(),
        );
        assert_eq!(AwaitProgramEvent::decode(&uppercase_source), Some(wait));
        let event = ProgramEvent {
            position: u64::MAX,
            payload: InlineFramePayload::new(vec![0, 128, 255]),
        };
        assert_eq!(
            event.encode().as_bytes(),
            br#"{"position":"18446744073709551615","payload":[0,128,255]}"#
        );
        assert_eq!(ProgramEvent::decode(&event.encode()), Some(event));
    }

    #[test]
    fn deadline_codec_refuses_noncanonical_or_imprecise_inputs() {
        for bytes in [
            br#"{"deadline_unix_ms":9007199254740993}"#.as_slice(),
            br#"{"deadline_unix_ms":"01"}"#,
            br#"{"deadline_unix_ms":"18446744073709551616"}"#,
            br#"{"deadline_unix_ms":"-1"}"#,
            br#"{"deadline_unix_ms":"1","duration":"2"}"#,
        ] {
            assert_eq!(
                SleepUntil::decode(&InlineFramePayload::new(bytes)),
                None,
                "{bytes:?}"
            );
        }
    }
}
