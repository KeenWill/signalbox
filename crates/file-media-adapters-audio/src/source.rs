use std::num::NonZeroU64;

use signalbox_file_media_runtime::{
    CancellationSignal, MAX_PROCESSOR_FRAME_BYTES, ProcessorFailure, VerifiedBlobSource,
};

use crate::MAX_AUDIO_SOURCE_BYTES;

pub(crate) async fn read_complete(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<Option<Vec<u8>>, ProcessorFailure> {
    if cancellation.is_cancelled() {
        return Err(ProcessorFailure::Cancelled);
    }
    if source.byte_length().get() > MAX_AUDIO_SOURCE_BYTES {
        return Ok(None);
    }
    let source_length = source.byte_length().get();
    let capacity = usize::try_from(source_length).map_err(|_| ProcessorFailure::Failed)?;
    let maximum_chunk =
        u64::try_from(MAX_PROCESSOR_FRAME_BYTES / 2).map_err(|_| ProcessorFailure::Failed)?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut offset = 0_u64;
    while offset < source_length {
        if cancellation.is_cancelled() {
            return Err(ProcessorFailure::Cancelled);
        }
        let length = NonZeroU64::new((source_length - offset).min(maximum_chunk))
            .ok_or(ProcessorFailure::Failed)?;
        let chunk = source
            .read_range(offset, length)
            .await
            .map_err(|_| ProcessorFailure::Failed)?;
        if chunk.len() != usize::try_from(length.get()).map_err(|_| ProcessorFailure::Failed)? {
            return Err(ProcessorFailure::Failed);
        }
        bytes.extend_from_slice(&chunk);
        offset = offset
            .checked_add(length.get())
            .ok_or(ProcessorFailure::Failed)?;
    }
    if cancellation.is_cancelled() {
        return Err(ProcessorFailure::Cancelled);
    }
    Ok(Some(bytes))
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
        .min(std::num::NonZeroU64::new(64).ok_or(ProcessorFailure::Failed)?);
    source
        .read_range(0, length)
        .await
        .map_err(|_| ProcessorFailure::Failed)
}
