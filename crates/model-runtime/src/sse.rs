//! Bounded server-sent-event framing backed by `eventsource-stream`.

use std::{convert::Infallible, fmt, pin::Pin, task::Context};

use eventsource_stream::{EventStream, EventStreamError, Eventsource};
use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::{Stream, task::noop_waker_ref};

/// One framed SSE record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseRecord {
    /// The record's non-default `event` field.
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
    line_len: usize,
    pending_lf_swallow: bool,
    record_has_material: bool,
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
            line_len: 0,
            pending_lf_swallow: false,
            record_has_material: false,
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
        let accepted = match self.scan_line_bounds(&valid_bytes) {
            Ok(()) => valid_bytes.len(),
            Err((offset, error)) => {
                self.failed = Some(error);
                offset
            }
        };
        if self.failed.is_none() {
            self.failed = utf8_error;
        }
        if accepted > 0 {
            let send_result = self
                .sender
                .as_mut()
                .map(|sender| sender.unbounded_send(Ok(valid_bytes[..accepted].to_vec())));
            if !matches!(send_result, Some(Ok(()))) {
                self.failed = Some(SseFramingError::InvalidUtf8 {
                    detail: String::from("decoder input channel closed"),
                });
            }
        }
        let records = self.poll_records();
        SsePushOutcome {
            records,
            error: self.failed.clone(),
        }
    }

    /// Reports whether accepted bytes have not produced a record boundary.
    pub fn holds_unframed_bytes(&self) -> bool {
        self.line_len > 0 || self.record_has_material || !self.utf8_tail.is_empty()
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

    fn scan_line_bounds(&mut self, chunk: &[u8]) -> Result<(), (usize, SseFramingError)> {
        for (offset, byte) in chunk.iter().copied().enumerate() {
            if self.pending_lf_swallow {
                self.pending_lf_swallow = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\n' => self.finish_raw_line(),
                b'\r' => {
                    self.finish_raw_line();
                    self.pending_lf_swallow = true;
                }
                _ => {
                    self.line_len += 1;
                    if self.line_len > self.record_limit {
                        return Err((
                            offset,
                            SseFramingError::RecordTooLarge {
                                limit: self.record_limit,
                            },
                        ));
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

    fn finish_raw_line(&mut self) {
        self.record_has_material = self.line_len != 0;
        self.line_len = 0;
    }

    fn poll_records(&mut self) -> Vec<SseRecord> {
        let mut records = Vec::new();
        let mut context = Context::from_waker(noop_waker_ref());
        loop {
            match self.stream.as_mut().poll_next(&mut context) {
                std::task::Poll::Ready(Some(Ok(event))) => {
                    let event_name = (event.event != "message").then_some(event.event);
                    if event.data.len() + event_name.as_ref().map_or(0, String::len)
                        > self.record_limit
                    {
                        self.failed = Some(SseFramingError::RecordTooLarge {
                            limit: self.record_limit,
                        });
                        break;
                    }
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
    fn invalid_utf8_is_terminal() {
        let mut framing = SseFraming::new(1024);
        assert!(matches!(
            framing.push(&[0xff]).error,
            Some(SseFramingError::InvalidUtf8 { .. })
        ));
    }
}
