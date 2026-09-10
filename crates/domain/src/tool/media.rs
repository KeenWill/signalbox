//! Durable content-silent evidence for image results.

use std::num::NonZeroU64;

use crate::BlobDigest;

/// Validation classes eligible for rich presentation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MediaValidationEvidence {
    /// A strong signature was structurally validated.
    StrongSignature,
    /// Structure was independently validated.
    StructuralValidation,
}

/// Exact immutable identity and the reader that validated it.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MediaValidationIdentity {
    digest: BlobDigest,
    /// Borrows the validated canonical media type.
    media_type: String,
    /// Borrows the validating provider name.
    provider: String,
    /// Borrows the validating reader name.
    reader: String,
    /// Borrows the exact reader revision.
    revision: String,
    evidence: MediaValidationEvidence,
}

impl MediaValidationIdentity {
    /// Reconstitutes bounded content-silent reader evidence.
    pub fn try_new(
        digest: BlobDigest,
        media_type: String,
        provider: String,
        reader: String,
        revision: String,
        evidence: MediaValidationEvidence,
    ) -> Option<Self> {
        // File/media reader identities and canonical types follow docs/spec/file-and-media.md.
        let name = |value: &str, maximum| {
            value.len() <= maximum
                && value
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_lowercase())
                && value.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'_' | b'-' | b'.')
                })
        };
        let media_token = |value: &str| {
            !value.is_empty()
                && value.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(
                            byte,
                            b'!' | b'#'
                                | b'$'
                                | b'%'
                                | b'&'
                                | b'\''
                                | b'*'
                                | b'^'
                                | b'_'
                                | b'`'
                                | b'|'
                                | b'~'
                                | b'.'
                                | b'+'
                                | b'-'
                        )
                })
        };
        if media_type.len() > 255
            || !media_type
                .split_once('/')
                .is_some_and(|(kind, subtype)| media_token(kind) && media_token(subtype))
            || !name(&provider, 64)
            || !name(&reader, 64)
            || revision.is_empty()
            || revision.len() > 32
            || !revision.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return None;
        }
        Some(Self {
            digest,
            media_type,
            provider,
            reader,
            revision,
            evidence,
        })
    }
    /// Borrows the validated media_type.
    pub fn media_type(&self) -> &str {
        &self.media_type
    }
    /// Borrows the validated provider.
    pub fn provider(&self) -> &str {
        &self.provider
    }
    /// Borrows the validated reader.
    pub fn reader(&self) -> &str {
        &self.reader
    }
    /// Borrows the validated revision.
    pub fn revision(&self) -> &str {
        &self.revision
    }
    /// Returns the immutable blob identity.
    pub const fn digest(&self) -> BlobDigest {
        self.digest
    }
    /// Returns the admitted validation class.
    pub const fn evidence(&self) -> MediaValidationEvidence {
        self.evidence
    }
}

/// An image reference retaining independent source and presented evidence.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolMediaReference {
    presented: Box<MediaValidationIdentity>,
    source: Box<MediaValidationIdentity>,
    byte_length: NonZeroU64,
}

impl ToolMediaReference {
    /// Admits a direct image within the compiled presentation ceiling.
    pub fn direct_image(
        identity: MediaValidationIdentity,
        byte_length: NonZeroU64,
    ) -> Option<Self> {
        Self::image(identity.clone(), identity, byte_length)
    }
    /// Admits independently validated source and presented identities within the image ceiling.
    pub fn image(
        presented: MediaValidationIdentity,
        source: MediaValidationIdentity,
        byte_length: NonZeroU64,
    ) -> Option<Self> {
        // docs/spec/file-and-media.md: the hard presented-image ceiling is eight MiB.
        (byte_length.get() <= 8 * 1024 * 1024).then_some(Self {
            presented: Box::new(presented),
            source: Box::new(source),
            byte_length,
        })
    }
    /// Borrows the validated presented identity.
    pub const fn presented(&self) -> &MediaValidationIdentity {
        &self.presented
    }
    /// Borrows the independently validated source identity.
    pub const fn source(&self) -> &MediaValidationIdentity {
        &self.source
    }
    /// Returns the exact encoded image byte length.
    pub const fn byte_length(&self) -> NonZeroU64 {
        self.byte_length
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stored_image_identity_requires_canonical_types_and_representable_reader_names() {
        let identity = |media: &str, provider: &str| {
            MediaValidationIdentity::try_new(
                BlobDigest::from_bytes([1; 32]),
                media.into(),
                provider.into(),
                "png".into(),
                "rev:2+build".into(),
                MediaValidationEvidence::StrongSignature,
            )
        };
        assert!(identity("image/png", "image_reader").is_some());
        for media in ["IMAGE/PNG", "image/png; charset=utf-8", "image//png"] {
            assert!(identity(media, "image_reader").is_none());
        }
        for provider in ["ImageReader", "1reader", "reader/name"] {
            assert!(identity("image/png", provider).is_none());
        }
    }
}
