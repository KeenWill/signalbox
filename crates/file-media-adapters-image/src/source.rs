use signalbox_file_media_runtime::{CancellationSignal, ProcessorFailure, VerifiedBlobSource};

use crate::MAX_IMAGE_SOURCE_BYTES;

pub(crate) async fn read_complete(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
    maximum_bytes: u64,
) -> Result<Option<Vec<u8>>, ProcessorFailure> {
    if cancellation.is_cancelled() {
        return Err(ProcessorFailure::Cancelled);
    }
    if source.byte_length().get() > maximum_bytes.min(MAX_IMAGE_SOURCE_BYTES) {
        return Ok(None);
    }
    let bytes = source
        .read_range(0, source.byte_length())
        .await
        .map_err(|_| ProcessorFailure::Failed)?;
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
        .min(std::num::NonZeroU64::new(16).ok_or(ProcessorFailure::Failed)?);
    source
        .read_range(0, length)
        .await
        .map_err(|_| ProcessorFailure::Failed)
}
