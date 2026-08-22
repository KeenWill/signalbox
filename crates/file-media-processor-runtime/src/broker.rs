use std::num::NonZeroU64;

use serde::{Serialize, de::DeserializeOwned};
use signalbox_file_media_runtime::MAX_PROCESSOR_FRAME_BYTES;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::protocol::WireReadEnvelope;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BrokerError {
    Eof,
    Frame,
    Range,
}

pub(crate) struct RangeBroker {
    source_bytes: u64,
    maximum_range_bytes: u64,
    envelope: WireReadEnvelope,
    cumulative_bytes: u64,
    range_count: u32,
    arbitrary_range_count: u32,
    next_stream_offset: u64,
}

impl RangeBroker {
    pub(crate) const fn new(
        source_bytes: u64,
        envelope: WireReadEnvelope,
        maximum_range_bytes: u64,
    ) -> Self {
        Self {
            source_bytes,
            maximum_range_bytes,
            envelope,
            cumulative_bytes: 0,
            range_count: 0,
            arbitrary_range_count: 0,
            next_stream_offset: 0,
        }
    }

    pub(crate) fn admit(&mut self, offset: u64, length: u64) -> Result<NonZeroU64, BrokerError> {
        let length = NonZeroU64::new(length).ok_or(BrokerError::Range)?;
        if length.get() > self.maximum_range_bytes {
            return Err(BrokerError::Range);
        }
        let end = offset.checked_add(length.get()).ok_or(BrokerError::Range)?;
        if end > self.source_bytes {
            return Err(BrokerError::Range);
        }
        let cumulative = self
            .cumulative_bytes
            .checked_add(length.get())
            .ok_or(BrokerError::Range)?;
        let count = self.range_count.checked_add(1).ok_or(BrokerError::Range)?;
        match self.envelope {
            WireReadEnvelope::Probe {
                prefix_bytes,
                suffix_bytes,
                ranges,
                cumulative_bytes,
            } => {
                let suffix_start = self.source_bytes.saturating_sub(suffix_bytes);
                let in_prefix = end <= prefix_bytes.min(self.source_bytes);
                let in_suffix = offset >= suffix_start;
                let maximum_requests = ranges.checked_add(2).ok_or(BrokerError::Range)?;
                let arbitrary = self
                    .arbitrary_range_count
                    .checked_add(u32::from(!in_prefix && !in_suffix))
                    .ok_or(BrokerError::Range)?;
                if arbitrary > ranges || count > maximum_requests || cumulative > cumulative_bytes {
                    return Err(BrokerError::Range);
                }
                self.arbitrary_range_count = arbitrary;
            }
            WireReadEnvelope::Streaming {
                ranges,
                cumulative_bytes,
            } => {
                if offset != self.next_stream_offset
                    || count > ranges
                    || cumulative > cumulative_bytes
                {
                    return Err(BrokerError::Range);
                }
                self.next_stream_offset = end;
            }
            WireReadEnvelope::RandomAccess {
                ranges,
                cumulative_bytes,
            } => {
                if count > ranges || cumulative > cumulative_bytes {
                    return Err(BrokerError::Range);
                }
            }
        }
        self.cumulative_bytes = cumulative;
        self.range_count = count;
        Ok(length)
    }
}

pub(crate) async fn write_frame<Writer, Value>(
    writer: &mut Writer,
    value: &Value,
) -> Result<(), BrokerError>
where
    Writer: AsyncWrite + Unpin,
    Value: Serialize,
{
    write_frame_with_limit(writer, value, MAX_PROCESSOR_FRAME_BYTES).await
}

pub(crate) async fn write_frame_with_limit<Writer, Value>(
    writer: &mut Writer,
    value: &Value,
    maximum_bytes: usize,
) -> Result<(), BrokerError>
where
    Writer: AsyncWrite + Unpin,
    Value: Serialize,
{
    let encoded = serde_json::to_vec(value).map_err(|_| BrokerError::Frame)?;
    if encoded.len() > maximum_bytes || maximum_bytes > MAX_PROCESSOR_FRAME_BYTES {
        return Err(BrokerError::Frame);
    }
    let length = u32::try_from(encoded.len()).map_err(|_| BrokerError::Frame)?;
    writer
        .write_all(&length.to_be_bytes())
        .await
        .map_err(|_| BrokerError::Frame)?;
    writer
        .write_all(&encoded)
        .await
        .map_err(|_| BrokerError::Frame)?;
    writer.flush().await.map_err(|_| BrokerError::Frame)
}

pub(crate) async fn read_frame<Reader, Value>(reader: &mut Reader) -> Result<Value, BrokerError>
where
    Reader: AsyncRead + Unpin,
    Value: DeserializeOwned,
{
    read_frame_with_limit(reader, MAX_PROCESSOR_FRAME_BYTES).await
}

pub(crate) async fn read_frame_with_limit<Reader, Value>(
    reader: &mut Reader,
    maximum_bytes: usize,
) -> Result<Value, BrokerError>
where
    Reader: AsyncRead + Unpin,
    Value: DeserializeOwned,
{
    let mut length = [0_u8; 4];
    let first = reader
        .read(&mut length[..1])
        .await
        .map_err(|_| BrokerError::Frame)?;
    if first == 0 {
        return Err(BrokerError::Eof);
    }
    reader
        .read_exact(&mut length[1..])
        .await
        .map_err(|_| BrokerError::Frame)?;
    let length = usize::try_from(u32::from_be_bytes(length)).map_err(|_| BrokerError::Frame)?;
    if length == 0 || length > maximum_bytes || maximum_bytes > MAX_PROCESSOR_FRAME_BYTES {
        return Err(BrokerError::Frame);
    }
    let mut encoded = vec![0_u8; length];
    reader
        .read_exact(&mut encoded)
        .await
        .map_err(|_| BrokerError::Frame)?;
    serde_json::from_slice(&encoded).map_err(|_| BrokerError::Frame)
}

#[cfg(test)]
mod tests {
    use super::{BrokerError, RangeBroker};
    use crate::protocol::WireReadEnvelope;

    #[test]
    fn probe_envelope_rejects_an_extra_arbitrary_range() {
        let mut broker = RangeBroker::new(
            1_000,
            WireReadEnvelope::Probe {
                prefix_bytes: 10,
                suffix_bytes: 10,
                ranges: 1,
                cumulative_bytes: 40,
            },
            100,
        );
        assert!(broker.admit(0, 10).is_ok());
        assert!(broker.admit(500, 10).is_ok());
        assert!(broker.admit(990, 10).is_ok());
        assert_eq!(broker.admit(600, 1), Err(BrokerError::Range));
    }

    #[test]
    fn streaming_envelope_rejects_nonmonotonic_access() {
        let mut broker = RangeBroker::new(
            100,
            WireReadEnvelope::Streaming {
                ranges: 2,
                cumulative_bytes: 100,
            },
            100,
        );
        assert!(broker.admit(0, 10).is_ok());
        assert_eq!(broker.admit(9, 10), Err(BrokerError::Range));
    }

    #[test]
    fn streaming_envelope_rejects_range_fanout_excess() {
        let mut broker = RangeBroker::new(
            100,
            WireReadEnvelope::Streaming {
                ranges: 1,
                cumulative_bytes: 100,
            },
            100,
        );
        assert!(broker.admit(0, 10).is_ok());
        assert_eq!(broker.admit(10, 10), Err(BrokerError::Range));
    }

    #[test]
    fn random_envelope_rejects_cumulative_excess() {
        let mut broker = RangeBroker::new(
            100,
            WireReadEnvelope::RandomAccess {
                ranges: 2,
                cumulative_bytes: 10,
            },
            100,
        );
        assert!(broker.admit(90, 6).is_ok());
        assert_eq!(broker.admit(0, 5), Err(BrokerError::Range));
    }

    #[test]
    fn frame_envelope_rejects_one_oversized_source_reply() {
        let mut broker = RangeBroker::new(
            1_000,
            WireReadEnvelope::Streaming {
                ranges: 10,
                cumulative_bytes: 1_000,
            },
            100,
        );
        assert_eq!(broker.admit(0, 101), Err(BrokerError::Range));
    }
}
