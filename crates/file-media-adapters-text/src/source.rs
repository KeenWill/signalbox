use signalbox_file_media_runtime::{CancellationSignal, ProcessorFailure, VerifiedBlobSource};

use crate::{MAX_TEXT_FAMILY_BYTES, PROBE_PREFIX_BYTES, json_adapter::ProbeExtent};

pub(crate) async fn read_complete(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
    maximum_bytes: u64,
) -> Result<Option<Vec<u8>>, ProcessorFailure> {
    if cancellation.is_cancelled() {
        return Err(ProcessorFailure::Cancelled);
    }
    if source.byte_length().get() > MAX_TEXT_FAMILY_BYTES.min(maximum_bytes) {
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
    read_prefix(source, cancellation, PROBE_PREFIX_BYTES).await
}

pub(crate) async fn read_validation_prefix(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
    maximum_bytes: u64,
) -> Result<Vec<u8>, ProcessorFailure> {
    read_prefix(source, cancellation, PROBE_PREFIX_BYTES.min(maximum_bytes)).await
}

async fn read_prefix(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
    maximum_bytes: u64,
) -> Result<Vec<u8>, ProcessorFailure> {
    if cancellation.is_cancelled() {
        return Err(ProcessorFailure::Cancelled);
    }
    let length = source
        .byte_length()
        .min(std::num::NonZeroU64::new(maximum_bytes).ok_or(ProcessorFailure::Failed)?);
    source
        .read_range(0, length)
        .await
        .map_err(|_| ProcessorFailure::Failed)
}

/// Decodes probe bytes whose trailing scalar may have been cut by the probe
/// boundary, discarding an incomplete final scalar as a read artifact.
///
/// Only sound for a genuinely truncated prefix. Use [`probe_utf8_within`] when
/// the extent is known, so a complete source is never judged on a shortened
/// view of its own bytes.
pub(crate) fn probe_utf8(bytes: &[u8]) -> Option<&str> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(error) if error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).ok()
        }
        Err(_) => None,
    }
}

/// Decodes probe bytes according to how much of the source they cover.
///
/// A truncated prefix may end mid-scalar because the probe boundary cut the
/// source, so the incomplete trailing scalar is a read artifact and is dropped.
/// A complete source has no such artifact: every byte is real content, so an
/// incomplete trailing scalar means the source itself is not valid UTF-8 and no
/// structural candidate may be claimed from the shortened text.
pub(crate) fn probe_utf8_within(bytes: &[u8], extent: ProbeExtent) -> Option<&str> {
    match extent {
        ProbeExtent::CompleteSource => std::str::from_utf8(bytes).ok(),
        ProbeExtent::TruncatedPrefix => probe_utf8(bytes),
    }
}

pub(crate) fn checked_utf8(bytes: Vec<u8>) -> Result<String, &'static str> {
    let text = String::from_utf8(bytes).map_err(|_| "invalid_utf8")?;
    if text.contains('\0') {
        Err("nul_byte")
    } else {
        Ok(text)
    }
}
