//! Bounded server-sent-event framing backed by `eventsource-stream`.

use std::{convert::Infallible, fmt, pin::Pin, task::Context};

use eventsource_stream::{EventStream, EventStreamError, Eventsource};
use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::{Stream, task::noop_waker_ref};

/// One framed SSE record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseRecord {
    /// The nonempty `event` field, including `message`; absent or reset is `None`.
    pub event: Option<String>,
    /// The record's data lines joined with newlines.
    pub data: String,
}

/// A terminal SSE framing failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseFramingError {
    /// One raw line or one decoded record exceeded the configured size limit.
    RecordTooLarge {
        /// The configured limit, in bytes.
        limit: usize,
    },
    /// The input was not a valid UTF-8 event stream.
    InvalidUtf8 {
        /// Rendered parser detail.
        detail: String,
    },
}

impl fmt::Display for SseFramingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RecordTooLarge { limit } => {
                write!(formatter, "SSE record exceeds the {limit}-byte limit")
            }
            Self::InvalidUtf8 { detail } => {
                write!(formatter, "SSE stream is not valid UTF-8: {detail}")
            }
        }
    }
}

impl std::error::Error for SseFramingError {}

/// Records completed by one push followed by an optional terminal error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsePushOutcome {
    /// Records completed in stream order.
    pub records: Vec<SseRecord>,
    /// The terminal framing failure, when one occurred.
    pub error: Option<SseFramingError>,
}

/// How the stream stood when transport ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseTermination {
    /// The stream ended at a record boundary.
    Clean,
    /// The stream ended with undispatched material.
    TruncatedRecord,
}

type ByteStream = UnboundedReceiver<Result<Vec<u8>, Infallible>>;

/// Incremental bounded wrapper around the maintained SSE decoder.
pub struct SseFraming {
    record_limit: usize,
    sender: Option<UnboundedSender<Result<Vec<u8>, Infallible>>>,
    stream: Pin<Box<EventStream<ByteStream>>>,
    failed: Option<SseFramingError>,
    line_buffer: Vec<u8>,
    at_stream_start: bool,
    event_len: Option<usize>,
    joined_data_len: Option<usize>,
    pending_lf_swallow: bool,
    utf8_tail: Vec<u8>,
}

impl fmt::Debug for SseFraming {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SseFraming")
            .field("record_limit", &self.record_limit)
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

impl SseFraming {
    /// Creates a decoder with a hard raw-line and decoded-record byte limit.
    pub fn new(record_limit: usize) -> Self {
        let (sender, receiver) = unbounded();
        Self {
            record_limit,
            sender: Some(sender),
            stream: Box::pin(receiver.eventsource()),
            failed: None,
            line_buffer: Vec::new(),
            at_stream_start: true,
            event_len: None,
            joined_data_len: None,
            pending_lf_swallow: false,
            utf8_tail: Vec::new(),
        }
    }

    /// Supplies one transport chunk and returns every completed event.
    pub fn push(&mut self, chunk: &[u8]) -> SsePushOutcome {
        if let Some(error) = &self.failed {
            return SsePushOutcome {
                records: Vec::new(),
                error: Some(error.clone()),
            };
        }
        let (valid_bytes, utf8_error) = self.validated_utf8_prefix(chunk);
        let mut records = Vec::new();
        if let Err(error) = self.accept_lines(&valid_bytes, &mut records) {
            self.failed = Some(error);
        }
        if self.failed.is_none() {
            self.failed = utf8_error;
        }
        SsePushOutcome {
            records,
            error: self.failed.clone(),
        }
    }

    /// Reports whether accepted bytes have not produced a record boundary.
    pub fn holds_unframed_bytes(&self) -> bool {
        !self.line_buffer.is_empty()
            || self.event_len.is_some()
            || self.joined_data_len.is_some()
            || !self.utf8_tail.is_empty()
    }

    /// Closes the maintained decoder and reports whether material was partial.
    pub fn finish(mut self) -> SseTermination {
        self.sender.take();
        let _ = self.poll_records();
        if self.holds_unframed_bytes() {
            SseTermination::TruncatedRecord
        } else {
            SseTermination::Clean
        }
    }

    fn accept_lines(
        &mut self,
        chunk: &[u8],
        records: &mut Vec<SseRecord>,
    ) -> Result<(), SseFramingError> {
        for byte in chunk.iter().copied() {
            if self.pending_lf_swallow {
                self.pending_lf_swallow = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\n' => self.finish_raw_line(records)?,
                b'\r' => {
                    self.finish_raw_line(records)?;
                    self.pending_lf_swallow = true;
                }
                _ => {
                    self.line_buffer.push(byte);
                    if self.line_buffer.len() > self.record_limit {
                        return Err(SseFramingError::RecordTooLarge {
                            limit: self.record_limit,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn validated_utf8_prefix(&mut self, chunk: &[u8]) -> (Vec<u8>, Option<SseFramingError>) {
        let mut bytes = std::mem::take(&mut self.utf8_tail);
        bytes.extend_from_slice(chunk);
        match std::str::from_utf8(&bytes) {
            Ok(_) => (bytes, None),
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                let suffix = bytes.split_off(valid_up_to);
                if error.error_len().is_none() {
                    self.utf8_tail = suffix;
                    (bytes, None)
                } else {
                    (
                        bytes,
                        Some(SseFramingError::InvalidUtf8 {
                            detail: error.to_string(),
                        }),
                    )
                }
            }
        }
    }

    fn finish_raw_line(&mut self, records: &mut Vec<SseRecord>) -> Result<(), SseFramingError> {
        let mut raw_line = std::mem::take(&mut self.line_buffer);
        if self.at_stream_start {
            self.at_stream_start = false;
            if raw_line.starts_with(b"\xef\xbb\xbf") {
                raw_line.drain(..3);
            }
        }
        let line = &raw_line;
        let boundary = line.is_empty();
        if !boundary {
            let mut parts = line.splitn(2, |byte| *byte == b':');
            let field = parts.next().unwrap_or_default();
            let value = parts.next().unwrap_or_default();
            let value = value.strip_prefix(b" ").unwrap_or(value);
            match field {
                b"event" => self.event_len = (!value.is_empty()).then_some(value.len()),
                b"data" => {
                    self.joined_data_len = Some(self.joined_data_len.map_or(value.len(), |len| {
                        len.saturating_add(1).saturating_add(value.len())
                    }));
                }
                _ => {}
            }
            if self
                .joined_data_len
                .unwrap_or_default()
                .saturating_add(self.event_len.unwrap_or_default())
                > self.record_limit
            {
                return Err(SseFramingError::RecordTooLarge {
                    limit: self.record_limit,
                });
            }
        }
        // Forward only admitted complete lines, so the decoder cannot accumulate
        // data past the retained-record bound or defer a terminal CR until EOF.
        raw_line.push(b'\n');
        let sent = self
            .sender
            .as_mut()
            .map(|sender| sender.unbounded_send(Ok(raw_line)));
        if !matches!(sent, Some(Ok(()))) {
            return Err(SseFramingError::InvalidUtf8 {
                detail: String::from("decoder input channel closed"),
            });
        }
        records.extend(self.poll_records());
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        if boundary {
            self.event_len = None;
            self.joined_data_len = None;
        }
        Ok(())
    }

    fn poll_records(&mut self) -> Vec<SseRecord> {
        let mut records = Vec::new();
        let mut context = Context::from_waker(noop_waker_ref());
        loop {
            match self.stream.as_mut().poll_next(&mut context) {
                std::task::Poll::Ready(Some(Ok(event))) => {
                    let event_name = self.event_len.map(|_| event.event);
                    records.push(SseRecord {
                        event: event_name,
                        data: event.data,
                    });
                }
                std::task::Poll::Ready(Some(Err(error))) => {
                    self.failed = Some(map_parser_error(error));
                    break;
                }
                std::task::Poll::Ready(None) | std::task::Poll::Pending => break,
            }
        }
        records
    }
}

fn map_parser_error(error: EventStreamError<Infallible>) -> SseFramingError {
    SseFramingError::InvalidUtf8 {
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{SseFraming, SseFramingError, SseRecord, SseTermination};

    #[test]
    fn fragmented_event_is_decoded_by_eventsource_stream() {
        let mut framing = SseFraming::new(1024);
        assert!(framing.push(b"event: ping\ndata: {\"pi").records.is_empty());
        assert_eq!(
            framing.push(b"ng\":true}\n\n").records,
            vec![SseRecord {
                event: Some(String::from("ping")),
                data: String::from("{\"ping\":true}"),
            }]
        );
        assert_eq!(framing.finish(), SseTermination::Clean);
    }

    #[test]
    fn data_only_event_uses_the_default_event_type() {
        let mut framing = SseFraming::new(1024);
        assert_eq!(
            framing.push(b"data: hello\n\n").records,
            vec![SseRecord {
                event: None,
                data: String::from("hello"),
            }]
        );
    }

    #[test]
    fn raw_line_limit_fails_before_unbounded_buffering() {
        let mut framing = SseFraming::new(4);
        let outcome = framing.push(b"data:");
        assert_eq!(
            outcome.error,
            Some(SseFramingError::RecordTooLarge { limit: 4 })
        );
    }

    #[test]
    fn incomplete_record_is_reported_at_transport_end() {
        let mut framing = SseFraming::new(1024);
        assert!(framing.push(b"data: partial\n").records.is_empty());
        assert_eq!(framing.finish(), SseTermination::TruncatedRecord);
    }

    #[test]
    fn ignored_complete_lines_leave_no_unframed_material() {
        let mut framing = SseFraming::new(32);
        assert_eq!(
            framing
                .push(b": comment\nid: 7\nretry: 100\nunknown: value\nevent:\n")
                .error,
            None
        );
        assert!(!framing.holds_unframed_bytes());
        assert_eq!(framing.finish(), SseTermination::Clean);
    }

    #[test]
    fn ignored_lines_do_not_clear_retained_data() {
        let mut framing = SseFraming::new(32);
        assert_eq!(framing.push(b"data:\n: comment\n").error, None);
        assert!(framing.holds_unframed_bytes());
        assert_eq!(framing.finish(), SseTermination::TruncatedRecord);
    }

    #[test]
    fn explicit_message_is_distinct_from_missing_or_reset_event() {
        let mut framing = SseFraming::new(32);
        assert_eq!(
            framing
                .push(b"event: message\ndata: a\n\ndata: b\n\nevent: message\nevent:\ndata: c\n\n")
                .records,
            vec![
                SseRecord {
                    event: Some(String::from("message")),
                    data: String::from("a")
                },
                SseRecord {
                    event: None,
                    data: String::from("b")
                },
                SseRecord {
                    event: None,
                    data: String::from("c")
                },
            ],
        );
    }

    #[test]
    fn event_without_data_resets_at_a_blank_line() {
        let mut framing = SseFraming::new(32);
        assert_eq!(
            framing.push(b"event: message\n\ndata: a\n\n").records,
            vec![SseRecord {
                event: None,
                data: String::from("a")
            }],
        );
        assert!(!framing.holds_unframed_bytes());
    }

    #[test]
    fn cumulative_data_limit_fails_before_the_blank_separator() {
        let mut framing = SseFraming::new(16);
        assert_eq!(framing.push(b"data: 1234567890\n").error, None);
        let outcome = framing.push(b"data: 123456\n");
        assert_eq!(
            outcome.error,
            Some(SseFramingError::RecordTooLarge { limit: 16 })
        );
        assert!(outcome.records.is_empty());
    }

    #[test]
    fn records_before_a_cumulative_failure_are_delivered() {
        let mut framing = SseFraming::new(16);
        let outcome = framing.push(b"data: kept\n\ndata: 1234567890\ndata: 123456\n");
        assert_eq!(
            outcome.records,
            vec![SseRecord {
                event: None,
                data: String::from("kept")
            }]
        );
        assert_eq!(
            outcome.error,
            Some(SseFramingError::RecordTooLarge { limit: 16 })
        );
    }

    #[test]
    fn empty_data_separators_count_toward_the_record_limit() {
        let mut framing = SseFraming::new(5);
        assert_eq!(
            framing.push(b"data\ndata\ndata\ndata\ndata\ndata\n").error,
            None
        );
        assert_eq!(
            framing.push(b"data\n").error,
            Some(SseFramingError::RecordTooLarge { limit: 5 })
        );
    }

    #[test]
    fn replaced_event_values_release_their_record_budget() {
        let mut framing = SseFraming::new(18);
        assert_eq!(
            framing
                .push(b"event: aaaaaaaaaa\nevent: bbbbbbbbbb\ndata: 12345678\n\n")
                .records,
            vec![SseRecord {
                event: Some(String::from("bbbbbbbbbb")),
                data: String::from("12345678")
            }],
        );
        assert_eq!(
            framing.push(b"event: message\ndata: 123456789012\n").error,
            Some(SseFramingError::RecordTooLarge { limit: 18 })
        );
    }

    #[test]
    fn split_bom_and_crlf_preserve_explicit_event_metadata() {
        let mut framing = SseFraming::new(32);
        assert_eq!(framing.push(b"\xef").error, None);
        assert_eq!(framing.push(b"\xbb\xbfevent: message\r").error, None);
        assert_eq!(framing.push(b"").error, None);
        assert_eq!(
            framing.push(b"\ndata: a\r\r").records,
            vec![SseRecord {
                event: Some(String::from("message")),
                data: String::from("a")
            }],
        );
        assert_eq!(framing.finish(), SseTermination::Clean);
    }

    #[test]
    fn invalid_utf8_is_terminal() {
        let mut framing = SseFraming::new(1024);
        assert!(matches!(
            framing.push(&[0xff]).error,
            Some(SseFramingError::InvalidUtf8 { .. })
        ));
    }
}
