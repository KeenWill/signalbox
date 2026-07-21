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

/// A framing failure. Terminal for the stream: once a failure is reported,
/// the framer's state no longer corresponds to a record boundary, later
/// pushes frame nothing, and the same failure is reported again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseFramingError {
    /// One line, or one record's retained content (its data plus its event
    /// value), exceeded the configured size limit.
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

/// What one push framed: every record completed before any failure, then
/// the failure if one occurred.
///
/// Records and failure are reported together, never traded off: a record
/// completed earlier in a chunk is delivered even when a later record in
/// the same chunk fails, so evidence observed before the failure (a
/// provider-model report, for example) does not depend on how the transport
/// happened to batch bytes into chunks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsePushOutcome {
    /// Records completed by this push, in stream order.
    pub records: Vec<SseRecord>,
    /// The framing failure this push ran into, when one occurred; terminal
    /// for the stream.
    pub error: Option<SseFramingError>,
}

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
    failed: Option<SseFramingError>,
    line_buffer: Vec<u8>,
    current_event: Option<String>,
    data_lines: Vec<String>,
    accumulated_content: usize,
}

impl SseFraming {
    /// A framer bounding every line and every record's retained content
    /// (data plus event value) at `record_limit` bytes.
    ///
    /// Comment and ignored-field lines are bounded per line but never
    /// accumulate, so keep-alive comments on a long-lived stream cannot
    /// exhaust the bound.
    pub fn new(record_limit: usize) -> Self {
        Self {
            record_limit,
            at_stream_start: true,
            pending_lf_swallow: false,
            failed: None,
            line_buffer: Vec::new(),
            current_event: None,
            data_lines: Vec::new(),
            accumulated_content: 0,
        }
    }

    /// Consumes one transport chunk, returning every record it completes
    /// and the framing failure it ran into, if any.
    pub fn push(&mut self, chunk: &[u8]) -> SsePushOutcome {
        let mut records = Vec::new();
        if let Some(error) = &self.failed {
            return SsePushOutcome {
                records,
                error: Some(error.clone()),
            };
        }
        let mut bytes = chunk;
        if self.pending_lf_swallow {
            // The previous chunk ended in a CR terminator; a LF opening the
            // next non-empty chunk belongs to the same terminator. An empty
            // chunk consumes nothing and leaves the wait in place.
            let Some(first) = bytes.first() else {
                return SsePushOutcome {
                    records,
                    error: None,
                };
            };
            if *first == b'\n' {
                bytes = &bytes[1..];
            }
            self.pending_lf_swallow = false;
        }
        let mut index = 0;
        while index < bytes.len() {
            let step = match bytes[index] {
                b'\n' => {
                    index += 1;
                    self.finish_line(&mut records)
                }
                b'\r' => {
                    match bytes.get(index + 1) {
                        Some(b'\n') => index += 2,
                        Some(_) => index += 1,
                        None => {
                            self.pending_lf_swallow = true;
                            index += 1;
                        }
                    }
                    self.finish_line(&mut records)
                }
                byte => {
                    self.line_buffer.push(byte);
                    index += 1;
                    Ok(())
                }
            };
            if let Err(error) = step {
                self.failed = Some(error.clone());
                return SsePushOutcome {
                    records,
                    error: Some(error),
                };
            }
        }
        if let Err(error) = self.check_limit() {
            self.failed = Some(error.clone());
            return SsePushOutcome {
                records,
                error: Some(error),
            };
        }
        SsePushOutcome {
            records,
            error: None,
        }
    }

    /// Reports how the stream stood at end of transport. Meaningful only on
    /// a stream that reported no framing failure.
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
        if line.len() > self.record_limit {
            return Err(SseFramingError::RecordTooLarge {
                limit: self.record_limit,
            });
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
            self.accumulated_content = 0;
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
            "event" => {
                self.accumulated_content += value.len();
                self.current_event = Some(value.to_string());
            }
            "data" => {
                // Joining retained data lines inserts one newline between
                // each pair; count that allocation as part of the record.
                self.accumulated_content += value.len() + usize::from(!self.data_lines.is_empty());
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
        if self.line_buffer.len() + self.accumulated_content > self.record_limit {
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

    /// Pushes one chunk that must frame without a failure and returns its
    /// completed records.
    #[track_caller]
    fn push_ok(framing: &mut SseFraming, chunk: &[u8]) -> Vec<SseRecord> {
        let outcome = framing.push(chunk);
        assert_eq!(outcome.error, None, "chunk was expected to frame cleanly");
        outcome.records
    }

    #[test]
    fn one_chunk_frames_event_and_data() {
        let mut framing = framer();

        let records = push_ok(&mut framing, b"event: message_start\ndata: {\"a\":1}\n\n");

        assert_eq!(records, vec![record(Some("message_start"), "{\"a\":1}")]);
        assert_eq!(framing.finish(), SseTermination::Clean);
    }

    #[test]
    fn a_record_split_across_chunks_frames_once_complete() {
        let mut framing = framer();

        let first = push_ok(&mut framing, b"event: ping\ndata: {\"pi");
        let second = push_ok(&mut framing, b"ng\":true}\n\n");

        assert_eq!(first, vec![]);
        assert_eq!(second, vec![record(Some("ping"), "{\"ping\":true}")]);
    }

    #[test]
    fn multi_line_data_joins_with_newline() {
        let mut framing = framer();

        let records = push_ok(&mut framing, b"data: first\ndata: second\n\n");

        assert_eq!(records, vec![record(None, "first\nsecond")]);
    }

    #[test]
    fn joined_data_separators_count_toward_the_record_limit() {
        let mut framing = SseFraming::new(5);

        let first = framing.push(b"data:\ndata:\ndata:\ndata:\ndata:\ndata:\n");
        let second = framing.push(b"data:\n");

        assert_eq!(first.error, None);
        assert_eq!(
            second.error,
            Some(SseFramingError::RecordTooLarge { limit: 5 })
        );
    }

    #[test]
    fn crlf_terminators_frame_like_lf() {
        let mut framing = framer();

        let records = push_ok(&mut framing, b"event: delta\r\ndata: x\r\n\r\n");

        assert_eq!(records, vec![record(Some("delta"), "x")]);
    }

    #[test]
    fn cr_terminator_split_before_its_lf_frames_once() {
        let mut framing = framer();

        let first = push_ok(&mut framing, b"data: x\r");
        let second = push_ok(&mut framing, b"\n\r\n");

        assert_eq!(first, vec![]);
        assert_eq!(second, vec![record(None, "x")]);
    }

    #[test]
    fn cr_terminator_split_before_a_data_line_does_not_split_the_record() {
        let mut framing = framer();

        let first = push_ok(&mut framing, b"data: a\r");
        let second = push_ok(&mut framing, b"\ndata: b\n\n");

        assert_eq!(first, vec![]);
        assert_eq!(second, vec![record(None, "a\nb")]);
    }

    #[test]
    fn an_empty_chunk_between_cr_and_lf_does_not_split_the_record() {
        let mut framing = framer();

        let first = push_ok(&mut framing, b"data: a\r");
        let empty = push_ok(&mut framing, b"");
        let second = push_ok(&mut framing, b"\ndata: b\n\n");

        assert_eq!(first, vec![]);
        assert_eq!(empty, vec![]);
        assert_eq!(second, vec![record(None, "a\nb")]);
    }

    #[test]
    fn comment_lines_and_unknown_fields_are_ignored() {
        let mut framing = framer();

        let records = push_ok(
            &mut framing,
            b": keep-alive\nid: 7\nretry: 100\ndata: kept\n\n",
        );

        assert_eq!(records, vec![record(None, "kept")]);
    }

    #[test]
    fn leading_bom_is_stripped_from_the_first_line_only() {
        let mut framing = framer();

        let records = push_ok(&mut framing, b"\xEF\xBB\xBFdata: first\n\n");

        assert_eq!(records, vec![record(None, "first")]);
    }

    #[test]
    fn event_without_data_dispatches_nothing_and_resets() {
        let mut framing = framer();

        let records = push_ok(&mut framing, b"event: orphan\n\ndata: later\n\n");

        assert_eq!(records, vec![record(None, "later")]);
        assert_eq!(framing.finish(), SseTermination::Clean);
    }

    #[test]
    fn eof_inside_a_record_reports_truncation() {
        let mut framing = framer();

        let records = push_ok(&mut framing, b"event: message_delta\ndata: {\"partial\":");

        assert_eq!(records, vec![]);
        assert_eq!(framing.finish(), SseTermination::TruncatedRecord);
    }

    #[test]
    fn eof_after_undispatched_complete_lines_reports_truncation() {
        let mut framing = framer();

        let records = push_ok(&mut framing, b"data: complete line, no blank separator\n");

        assert_eq!(records, vec![]);
        assert_eq!(framing.finish(), SseTermination::TruncatedRecord);
    }

    #[test]
    fn oversized_record_is_rejected_with_the_limit() {
        let mut framing = SseFraming::new(8);

        let outcome = framing.push(b"data: 123456789\n");

        assert_eq!(outcome.records, vec![]);
        assert_eq!(
            outcome.error,
            Some(SseFramingError::RecordTooLarge { limit: 8 })
        );
    }

    #[test]
    fn oversized_event_value_is_rejected_with_the_limit() {
        let mut framing = SseFraming::new(8);

        let outcome = framing.push(b"event: 123456789\n");

        assert_eq!(outcome.records, vec![]);
        assert_eq!(
            outcome.error,
            Some(SseFramingError::RecordTooLarge { limit: 8 })
        );
    }

    #[test]
    fn keep_alive_comments_never_accumulate_toward_the_limit() {
        let mut framing = SseFraming::new(16);

        let first = push_ok(&mut framing, b": ping-1234567\n: ping-1234567\n");
        let second = push_ok(&mut framing, b": ping-1234567\ndata: kept\n\n");

        assert_eq!(first, vec![]);
        assert_eq!(second, vec![record(None, "kept")]);
    }

    #[test]
    fn invalid_utf8_line_is_rejected_as_utf8_error() {
        let mut framing = framer();

        let outcome = framing.push(b"data: \xFF\xFE\n\n");

        assert!(matches!(
            outcome.error,
            Some(SseFramingError::InvalidUtf8 { .. })
        ));
    }

    #[test]
    fn records_completed_before_a_failure_in_the_same_chunk_are_delivered() {
        let mut framing = framer();

        let outcome = framing.push(b"event: kept\ndata: first\n\ndata: \xFF\xFE\n\n");

        assert_eq!(outcome.records, vec![record(Some("kept"), "first")]);
        assert!(matches!(
            outcome.error,
            Some(SseFramingError::InvalidUtf8 { .. })
        ));
    }

    #[test]
    fn a_failed_framer_frames_nothing_and_repeats_its_failure() {
        let mut framing = SseFraming::new(8);
        let first = framing.push(b"data: 123456789\n");

        let second = framing.push(b"data: ok\n\n");

        assert_eq!(
            first.error,
            Some(SseFramingError::RecordTooLarge { limit: 8 })
        );
        assert_eq!(second.records, vec![]);
        assert_eq!(
            second.error,
            Some(SseFramingError::RecordTooLarge { limit: 8 })
        );
    }
}
