//! Exact ordered user-content satellite decoding shared by persistence readers.

use serde::Deserialize;
use serde_json::Value;
use signalbox_domain::{
    AttachmentDisplayFilename, AttachmentKind, BlobDigest, DeclaredMediaType, UserContent,
    UserContentPart,
};

#[derive(Debug)]
pub(crate) enum StoredUserContentError {
    Malformed,
    UnsupportedPartKind(String),
    UnsupportedAttachmentKind(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredContentPart {
    position: i16,
    part_kind: String,
    text_value: Option<String>,
    blob_digest: Option<String>,
    attachment_kind: Option<String>,
    declared_media_type: Option<String>,
    display_filename: Option<String>,
}

pub(crate) fn decode(stored: Value) -> Result<UserContent, StoredUserContentError> {
    let stored: Vec<StoredContentPart> =
        serde_json::from_value(stored).map_err(|_| StoredUserContentError::Malformed)?;
    let mut parts = Vec::with_capacity(stored.len());
    for (expected_position, part) in stored.into_iter().enumerate() {
        if usize::try_from(part.position).ok() != Some(expected_position) {
            return Err(StoredUserContentError::Malformed);
        }
        let decoded = match part.part_kind.as_str() {
            "text"
                if part.blob_digest.is_none()
                    && part.attachment_kind.is_none()
                    && part.declared_media_type.is_none()
                    && part.display_filename.is_none() =>
            {
                UserContentPart::try_text(part.text_value.ok_or(StoredUserContentError::Malformed)?)
                    .map_err(|_| StoredUserContentError::Malformed)?
            }
            "attachment" if part.text_value.is_none() => {
                let digest = part
                    .blob_digest
                    .ok_or(StoredUserContentError::Malformed)?
                    .parse::<BlobDigest>()
                    .map_err(|_| StoredUserContentError::Malformed)?;
                let kind = match part.attachment_kind.as_deref() {
                    Some("image") => AttachmentKind::Image,
                    Some("document") => AttachmentKind::Document,
                    Some("file") => AttachmentKind::File,
                    Some(value) => {
                        return Err(StoredUserContentError::UnsupportedAttachmentKind(
                            value.to_owned(),
                        ));
                    }
                    None => return Err(StoredUserContentError::Malformed),
                };
                let media_type = DeclaredMediaType::try_new(
                    part.declared_media_type
                        .ok_or(StoredUserContentError::Malformed)?,
                )
                .map_err(|_| StoredUserContentError::Malformed)?;
                let display_filename = part
                    .display_filename
                    .map(AttachmentDisplayFilename::try_new)
                    .transpose()
                    .map_err(|_| StoredUserContentError::Malformed)?;
                UserContentPart::Attachment {
                    digest,
                    kind,
                    media_type,
                    display_filename,
                }
            }
            "text" | "attachment" => return Err(StoredUserContentError::Malformed),
            value => {
                return Err(StoredUserContentError::UnsupportedPartKind(
                    value.to_owned(),
                ));
            }
        };
        parts.push(decoded);
    }
    UserContent::try_parts(parts).map_err(|_| StoredUserContentError::Malformed)
}
