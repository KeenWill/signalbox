//! User input wire representations and validation.

use crate::scalars::{CanonicalBlobDigest, FrameValidationError, deserialize_required_nullable};
use serde::de::{IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// Maximum number of ordered parts in one process-protocol user input.
pub const MAX_USER_INPUT_PARTS: usize = signalbox_domain::UserContent::MAX_PARTS;
/// Maximum aggregate UTF-8 bytes across process-protocol text parts.
pub const MAX_USER_INPUT_TEXT_BYTES: usize = signalbox_domain::UserContent::MAX_TEXT_BYTES;
/// Maximum encoded bytes in one process-protocol attachment media type.
pub const MAX_USER_INPUT_MEDIA_TYPE_BYTES: usize = signalbox_domain::DeclaredMediaType::MAX_BYTES;
/// Maximum encoded bytes in one process-protocol attachment display filename.
pub const MAX_USER_INPUT_DISPLAY_FILENAME_BYTES: usize =
    signalbox_domain::AttachmentDisplayFilename::MAX_BYTES;

/// Closed semantic kind declared for one user attachment on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserAttachmentKind {
    /// Image content.
    Image,
    /// Page- or document-oriented content.
    Document,
    /// Other file content.
    File,
}

/// One exact part in canonical ordered user input.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum UserInputPart {
    /// Exact decoded text.
    Text {
        /// Nonempty text containing no U+0000.
        text: String,
    },
    /// Immutable blob reference and caller-declared metadata.
    Attachment {
        /// Canonical global blob identity.
        digest: CanonicalBlobDigest,
        /// Closed semantic attachment kind.
        kind: UserAttachmentKind,
        /// Exact visible-ASCII media-type declaration.
        media_type: String,
        /// Optional display basename, explicitly null when absent.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        display_filename: Option<String>,
    },
}

impl fmt::Debug for UserInputPart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text { .. } => formatter
                .debug_struct("Text")
                .field("text", &"<redacted>")
                .finish(),
            Self::Attachment {
                digest,
                kind,
                media_type,
                display_filename,
            } => formatter
                .debug_struct("Attachment")
                .field("digest", digest)
                .field("kind", kind)
                .field("media_type", media_type)
                .field(
                    "display_filename",
                    &display_filename.as_ref().map(|_| "<redacted>"),
                )
                .finish(),
        }
    }
}

#[derive(signalbox_derive::Accessors)]
/// Canonical nonempty ordered user-input parts array.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct UserInputContent(
    /// Borrows the exact ordered parts.
    #[get(slice, as = "parts")]
    Vec<UserInputPart>,
);

struct UserInputContentVisitor;

impl<'de> Visitor<'de> for UserInputContentVisitor {
    type Value = UserInputContent;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "at most {MAX_USER_INPUT_PARTS} ordered user-input parts"
        )
    }

    fn visit_seq<AccessT>(self, mut sequence: AccessT) -> Result<Self::Value, AccessT::Error>
    where
        AccessT: SeqAccess<'de>,
    {
        let mut parts = Vec::with_capacity(
            sequence
                .size_hint()
                .unwrap_or_default()
                .min(MAX_USER_INPUT_PARTS),
        );
        while parts.len() < MAX_USER_INPUT_PARTS {
            match sequence.next_element::<UserInputPart>()? {
                Some(part) => parts.push(part),
                None => return Ok(UserInputContent(parts)),
            }
        }
        if sequence.next_element::<IgnoredAny>()?.is_some() {
            return Err(serde::de::Error::custom("too many user-input parts"));
        }
        Ok(UserInputContent(parts))
    }
}

impl<'de> Deserialize<'de> for UserInputContent {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        deserializer.deserialize_seq(UserInputContentVisitor)
    }
}

impl UserInputContent {
    /// Wraps one text part for text-only clients.
    pub fn text(value: String) -> Self {
        Self(vec![UserInputPart::Text { text: value }])
    }

    /// Wraps a complete parts array for structural validation at frame encode.
    pub fn from_parts(parts: Vec<UserInputPart>) -> Self {
        Self(parts)
    }

    /// Borrows text when this is exactly one text part.
    pub fn single_text(&self) -> Option<&str> {
        match self.0.as_slice() {
            [UserInputPart::Text { text }] => Some(text),
            _ => None,
        }
    }

    /// Transfers ownership of the exact ordered parts.
    pub fn into_parts(self) -> Vec<UserInputPart> {
        self.0
    }

    pub(crate) fn validate(&self) -> Result<(), FrameValidationError> {
        if self.0.is_empty() || self.0.len() > MAX_USER_INPUT_PARTS {
            return Err(FrameValidationError::UserContentShape);
        }

        let mut text_bytes = 0_usize;
        let mut previous_was_text = false;
        for part in &self.0 {
            match part {
                UserInputPart::Text { text } => {
                    if previous_was_text || text.is_empty() || text.contains('\0') {
                        return Err(FrameValidationError::UserContentShape);
                    }
                    text_bytes = text_bytes
                        .checked_add(text.len())
                        .ok_or(FrameValidationError::UserContentShape)?;
                    if text_bytes > MAX_USER_INPUT_TEXT_BYTES {
                        return Err(FrameValidationError::UserContentShape);
                    }
                    previous_was_text = true;
                }
                UserInputPart::Attachment {
                    media_type,
                    display_filename,
                    ..
                } => {
                    if media_type.is_empty()
                        || media_type.len() > MAX_USER_INPUT_MEDIA_TYPE_BYTES
                        || !media_type.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
                    {
                        return Err(FrameValidationError::UserContentShape);
                    }
                    if display_filename.as_ref().is_some_and(|filename| {
                        filename.is_empty()
                            || filename.len() > MAX_USER_INPUT_DISPLAY_FILENAME_BYTES
                            || filename == "."
                            || filename == ".."
                            || filename.contains('/')
                            || filename.contains('\\')
                            || filename.contains('\0')
                    }) {
                        return Err(FrameValidationError::UserContentShape);
                    }
                    previous_was_text = false;
                }
            }
        }
        Ok(())
    }
}
