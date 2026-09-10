use signalbox_file_media_runtime::{CancellationSignal, ProcessorFailure, VerifiedBlobSource};

use crate::MAX_IMAGE_SOURCE_BYTES;

pub(crate) async fn read_inspection_prefix(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
    maximum_bytes: u64,
) -> Result<Vec<u8>, ProcessorFailure> {
    if cancellation.is_cancelled() {
        return Err(ProcessorFailure::Cancelled);
    }
    let length = std::num::NonZeroU64::new(
        source
            .byte_length()
            .get()
            .min(maximum_bytes)
            .min(MAX_IMAGE_SOURCE_BYTES),
    )
    .ok_or(ProcessorFailure::Protocol)?;
    source
        .read_range(0, length)
        .await
        .map_err(|_| ProcessorFailure::Failed)
}

/// Bridges a decoder's synchronous ranged reads to the worker's source broker.
pub(crate) struct DecoderSource<'a> {
    source: &'a dyn VerifiedBlobSource,
    position: u64,
    runtime: tokio::runtime::Handle,
}

impl<'a> DecoderSource<'a> {
    pub(crate) fn new(source: &'a dyn VerifiedBlobSource) -> Self {
        Self {
            source,
            position: 0,
            runtime: tokio::runtime::Handle::current(),
        }
    }
}

impl std::io::Read for DecoderSource<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self
            .source
            .byte_length()
            .get()
            .saturating_sub(self.position);
        let length = remaining
            .min(buffer.len() as u64)
            .min(signalbox_file_media_runtime::MAX_PROBE_PREFIX_BYTES);
        let Some(length) = std::num::NonZeroU64::new(length) else {
            return Ok(0);
        };
        let bytes = self
            .runtime
            .block_on(self.source.read_range(self.position, length))
            .map_err(|_| std::io::Error::other("image source read failed"))?;
        if bytes.len() as u64 != length.get() {
            return Err(std::io::Error::other("image source returned a short range"));
        }
        buffer[..bytes.len()].copy_from_slice(&bytes);
        self.position += length.get();
        Ok(bytes.len())
    }
}

impl std::io::Seek for DecoderSource<'_> {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        let position = match position {
            std::io::SeekFrom::Start(offset) => Some(offset),
            std::io::SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
            std::io::SeekFrom::End(offset) => {
                self.source.byte_length().get().checked_add_signed(offset)
            }
        }
        .ok_or_else(|| std::io::Error::other("invalid image source seek"))?;
        self.position = position;
        Ok(position)
    }
}

pub(crate) async fn read_probe_prefix(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<Vec<u8>, ProcessorFailure> {
    if cancellation.is_cancelled() {
        return Err(ProcessorFailure::Cancelled);
    }
    let length = source
        .byte_length()
        .min(std::num::NonZeroU64::new(16).ok_or(ProcessorFailure::Failed)?);
    source
        .read_range(0, length)
        .await
        .map_err(|_| ProcessorFailure::Failed)
}
