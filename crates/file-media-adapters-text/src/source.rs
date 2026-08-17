use signalbox_file_media_runtime::{CancellationSignal, ProcessorFailure, VerifiedBlobSource};

use crate::{MAX_TEXT_FAMILY_BYTES, PROBE_PREFIX_BYTES};

pub(crate) async fn read_complete(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<Option<Vec<u8>>, ProcessorFailure> {
    if cancellation.is_cancelled() {
        return Err(ProcessorFailure::Cancelled);
    }
    if source.byte_length().get() > MAX_TEXT_FAMILY_BYTES {
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
        .min(std::num::NonZeroU64::new(PROBE_PREFIX_BYTES).ok_or(ProcessorFailure::Failed)?);
    source
        .read_range(0, length)
        .await
        .map_err(|_| ProcessorFailure::Failed)
}

pub(crate) fn checked_utf8(bytes: Vec<u8>) -> Result<String, &'static str> {
    let text = String::from_utf8(bytes).map_err(|_| "invalid_utf8")?;
    if text.contains('\0') {
        Err("nul_byte")
    } else {
        Ok(text)
    }
}
