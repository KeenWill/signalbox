//! Checks for streamed pack headers and decoded objects.

use crate::failure::LocalGitFailure;

pub(super) fn validate_pack_header(
    header: &[u8],
    content_end: usize,
    object_count: usize,
) -> Result<(), LocalGitFailure> {
    if content_end < 12
        || header.len() != 12
        || &header[..4] != b"PACK"
        || !matches!(&header[4..8], [0, 0, 0, 2] | [0, 0, 0, 3])
        || u32::from_be_bytes(
            header[8..12]
                .try_into()
                .map_err(|_| LocalGitFailure::Repository)?,
        ) as usize
            != object_count
    {
        return Err(LocalGitFailure::Repository);
    }
    Ok(())
}

pub(super) fn validate_decoded_size(
    size: usize,
    limit: Option<usize>,
) -> Result<(), LocalGitFailure> {
    if limit.is_some_and(|limit| size > limit) {
        Err(LocalGitFailure::Repository)
    } else {
        Ok(())
    }
}
