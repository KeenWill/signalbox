//! Server-sent-events record framing.
//!
//! A provider-agnostic incremental parser from transport byte chunks to SSE
//! records. Interpreting record contents belongs to each provider adapter;
//! this layer only frames, and it reports framing trouble as typed errors so
//! an adapter can surface stream-integrity evidence instead of guessing.
//!
//! Framing follows the WHATWG event-stream grammar for the constructs
//! providers use: `event` and `data` fields, multi-line data joined with
//! newlines, comment lines, a UTF-8 BOM at stream start, and `\n`, `\r\n`,
//! or `\r` line terminators. The `id` and `retry` fields are parsed and
//! ignored — no adapter resumes streams, and resuming would be a second
//! request this layer must never make (ADR-0005).

/// One framed SSE record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseRecord {
    /// The record's `event` field, when one was set.
    pub event: Option<String>,
    /// The record's data: every `data` line joined with `\n`.
    pub data: String,
}

/// A framing failure. Terminal for the stream: after an error the framer's
/// state no longer corresponds to a record boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseFramingError {
    /// One record exceeded the configured size limit.
    RecordTooLarge {
        /// The configured limit, in bytes.
        limit: usize,
    },
    /// A line is not valid UTF-8.
    InvalidUtf8 {
        /// Rendered description of the invalid sequence.
        detail: String,
    },
}

impl std::fmt::Display for SseFramingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RecordTooLarge { limit } => {
                write!(f, "SSE record exceeds the {limit}-byte limit")
            }
            Self::InvalidUtf8 { detail } => write!(f, "SSE line is not valid UTF-8: {detail}"),
        }
    }
}

impl std::error::Error for SseFramingError {}

/// How the stream stood when the transport ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseTermination {
    /// The stream ended at a record boundary.
    Clean,
    /// The stream ended inside an undispatched record; its partial content
    /// was discarded, which the adapter surfaces as integrity evidence.
    TruncatedRecord,
}

/// Incremental SSE framer: push transport chunks in, take framed records
/// out.
#[derive(Debug)]
pub struct SseFraming {
    record_limit: usize,
    at_stream_start: bool,
    pending_lf_swallow: bool,
    line_buffer: Vec<u8>,
    current_event: Option<String>,
    data_lines: Vec<String>,
    accumulated_data: usize,
}

impl SseFraming {
    /// A framer rejecting any record larger than `record_limit` bytes.
    pub fn new(record_limit: usize) -> Self {
        Self {
            record_limit,
            at_stream_start: true,
            pending_lf_swallow: false,
            line_buffer: Vec::new(),
            current_event: None,
            data_lines: Vec::new(),
            accumulated_data: 0,
        }
    }

    /// Consumes one transport chunk, returning every record it completes.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseRecord>, SseFramingError> {
        let mut records = Vec::new();
        let mut bytes = chunk;
        if self.pending_lf_swallow {
            // The previous chunk ended in a CR terminator; a LF opening
            // this chunk belongs to the same terminator.
            if bytes.first() == Some(&b'\n') {
                bytes = &bytes[1..];
            }
            self.pending_lf_swallow = false;
        }
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'\n' => {
                    self.finish_line(&mut records)?;
                    index += 1;
                }
                b'\r' => {
                    self.finish_line(&mut records)?;
                    match bytes.get(index + 1) {
                        Some(b'\n') => index += 2,
                        Some(_) => index += 1,
                        None => {
                            self.pending_lf_swallow = true;
                            index += 1;
                        }
                    }
                }
                byte => {
                    self.line_buffer.push(byte);
                    index += 1;
                }
            }
        }
        self.check_limit()?;
        Ok(records)
    }

    /// Reports how the stream stood at end of transport.
    pub fn finish(self) -> SseTermination {
        if !self.line_buffer.is_empty()
            || !self.data_lines.is_empty()
            || self.current_event.is_some()
        {
            SseTermination::TruncatedRecord
        } else {
            SseTermination::Clean
        }
    }

    fn finish_line(&mut self, records: &mut Vec<SseRecord>) -> Result<(), SseFramingError> {
        let mut line = std::mem::take(&mut self.line_buffer);
        if self.at_stream_start {
            if line.starts_with(&[0xEF, 0xBB, 0xBF]) {
                line.drain(..3);
            }
            self.at_stream_start = false;
        }
        let line = String::from_utf8(line).map_err(|error| SseFramingError::InvalidUtf8 {
            detail: error.to_string(),
        })?;
        if line.is_empty() {
            if !self.data_lines.is_empty() {
                records.push(SseRecord {
                    event: self.current_event.take(),
                    data: std::mem::take(&mut self.data_lines).join("\n"),
                });
            } else {
                self.current_event = None;
            }
            self.accumulated_data = 0;
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_str(), ""),
        };
        match field {
            "event" => self.current_event = Some(value.to_string()),
            "data" => {
                self.accumulated_data += value.len();
                self.data_lines.push(value.to_string());
            }
            // `id` and `retry` support stream resumption, which would be a
            // second request; parsed and deliberately dropped. Unknown
            // fields are ignored per the event-stream grammar.
            _ => {}
        }
        self.check_limit()
    }

    fn check_limit(&self) -> Result<(), SseFramingError> {
        if self.line_buffer.len() + self.accumulated_data > self.record_limit {
            return Err(SseFramingError::RecordTooLarge {
                limit: self.record_limit,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{SseFraming, SseFramingError, SseRecord, SseTermination};

    fn framer() -> SseFraming {
        SseFraming::new(1024)
    }

    fn record(event: Option<&str>, data: &str) -> SseRecord {
        SseRecord {
            event: event.map(str::to_string),
            data: data.to_string(),
        }
    }

    #[test]
    fn one_chunk_frames_event_and_data() {
        let mut framing = framer();

        let records = framing
            .push(b"event: message_start\ndata: {\"a\":1}\n\n")
            .expect("well-formed chunk frames");

        assert_eq!(records, vec![record(Some("message_start"), "{\"a\":1}")]);
        assert_eq!(framing.finish(), SseTermination::Clean);
    }

    #[test]
    fn a_record_split_across_chunks_frames_once_complete() {
        let mut framing = framer();

        let first = framing
            .push(b"event: ping\ndata: {\"pi")
            .expect("partial chunk accepted");
        let second = framing
            .push(b"ng\":true}\n\n")
            .expect("completing chunk frames");

        assert_eq!(first, vec![]);
        assert_eq!(second, vec![record(Some("ping"), "{\"ping\":true}")]);
    }

    #[test]
    fn multi_line_data_joins_with_newline() {
        let mut framing = framer();

        let records = framing
            .push(b"data: first\ndata: second\n\n")
            .expect("multi-line data frames");

        assert_eq!(records, vec![record(None, "first\nsecond")]);
    }

    #[test]
    fn crlf_terminators_frame_like_lf() {
        let mut framing = framer();

        let records = framing
            .push(b"event: delta\r\ndata: x\r\n\r\n")
            .expect("CRLF-terminated chunk frames");

        assert_eq!(records, vec![record(Some("delta"), "x")]);
    }

    #[test]
    fn cr_terminator_split_before_its_lf_frames_once() {
        let mut framing = framer();

        let first = framing
            .push(b"data: x\r")
            .expect("chunk ending in CR accepted");
        let second = framing
            .push(b"\n\r\n")
            .expect("LF continuing the CR terminator frames");

        assert_eq!(first, vec![]);
        assert_eq!(second, vec![record(None, "x")]);
    }

    #[test]
    fn cr_terminator_split_before_a_data_line_does_not_split_the_record() {
        let mut framing = framer();

        let first = framing
            .push(b"data: a\r")
            .expect("chunk ending in CR accepted");
        let second = framing
            .push(b"\ndata: b\n\n")
            .expect("continuation chunk frames");

        assert_eq!(first, vec![]);
        assert_eq!(second, vec![record(None, "a\nb")]);
    }

    #[test]
    fn comment_lines_and_unknown_fields_are_ignored() {
        let mut framing = framer();

        let records = framing
            .push(b": keep-alive\nid: 7\nretry: 100\ndata: kept\n\n")
            .expect("comments and unknown fields are tolerated");

        assert_eq!(records, vec![record(None, "kept")]);
    }

    #[test]
    fn leading_bom_is_stripped_from_the_first_line_only() {
        let mut framing = framer();

        let records = framing
            .push(b"\xEF\xBB\xBFdata: first\n\n")
            .expect("a BOM before the first line is tolerated");

        assert_eq!(records, vec![record(None, "first")]);
    }

    #[test]
    fn event_without_data_dispatches_nothing_and_resets() {
        let mut framing = framer();

        let records = framing
            .push(b"event: orphan\n\ndata: later\n\n")
            .expect("an event with no data is dropped at its blank line");

        assert_eq!(records, vec![record(None, "later")]);
        assert_eq!(framing.finish(), SseTermination::Clean);
    }

    #[test]
    fn eof_inside_a_record_reports_truncation() {
        let mut framing = framer();

        let records = framing
            .push(b"event: message_delta\ndata: {\"partial\":")
            .expect("partial accepted");

        assert_eq!(records, vec![]);
        assert_eq!(framing.finish(), SseTermination::TruncatedRecord);
    }

    #[test]
    fn eof_after_undispatched_complete_lines_reports_truncation() {
        let mut framing = framer();

        let records = framing
            .push(b"data: complete line, no blank separator\n")
            .expect("undispatched record accepted");

        assert_eq!(records, vec![]);
        assert_eq!(framing.finish(), SseTermination::TruncatedRecord);
    }

    #[test]
    fn oversized_record_is_rejected_with_the_limit() {
        let mut framing = SseFraming::new(8);

        let error = framing
            .push(b"data: 123456789\n")
            .expect_err("a record beyond the limit must be rejected");

        assert_eq!(error, SseFramingError::RecordTooLarge { limit: 8 });
    }

    #[test]
    fn invalid_utf8_line_is_rejected_as_utf8_error() {
        let mut framing = framer();

        let error = framing
            .push(b"data: \xFF\xFE\n\n")
            .expect_err("an invalid UTF-8 line must be rejected");

        assert!(matches!(error, SseFramingError::InvalidUtf8 { .. }));
    }
}
