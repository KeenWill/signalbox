//! Registry-admitted identities for direct rich presentation.

use std::num::NonZeroU64;

use crate::{CanonicalMediaType, FileDigest, ReaderIdentity, ValidatedFile, ValidationEvidence};

/// Content-silent evidence identifying the exact bytes a reader validated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaValidationIdentity {
    digest: FileDigest,
    media_type: CanonicalMediaType,
    reader: ReaderIdentity,
    evidence: ValidationEvidence,
}

impl MediaValidationIdentity {
    /// Returns the immutable digest.
    pub const fn digest(&self) -> FileDigest {
        self.digest
    }
    /// Borrows the validated canonical media type.
    pub const fn media_type(&self) -> &CanonicalMediaType {
        &self.media_type
    }
    /// Borrows the validating reader and revision.
    pub const fn reader(&self) -> &ReaderIdentity {
        &self.reader
    }
    /// Returns the content-silent validation class.
    pub const fn evidence(&self) -> ValidationEvidence {
        self.evidence
    }
}

/// Rich presentation kinds supported by durable file results.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaPresentationKind {
    /// An immutable encoded image.
    Image,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ImageDimensions {
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// A bounded reference retaining both presented and source validation identities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileMediaReference {
    presented: MediaValidationIdentity,
    source: MediaValidationIdentity,
    byte_length: NonZeroU64,
    kind: MediaPresentationKind,
    dimensions: ImageDimensions,
    derived_views: Vec<crate::ReadViewName>,
}

impl FileMediaReference {
    pub(crate) fn direct_image(validated: &ValidatedFile, dimensions: ImageDimensions) -> Self {
        let identity = MediaValidationIdentity {
            digest: validated.source().digest(),
            media_type: validated.detected_media_type().clone(),
            reader: validated.reader().clone(),
            evidence: validated.validation(),
        };
        Self {
            presented: identity.clone(),
            source: identity,
            byte_length: validated.source().byte_length(),
            kind: MediaPresentationKind::Image,
            dimensions,
            derived_views: validated
                .views()
                .iter()
                .filter(|view| view.image_kind() == Some(crate::ImageViewKind::Generated))
                .map(|view| view.name().clone())
                .collect(),
        }
    }
    pub(crate) fn derived_image(
        source: &ValidatedFile,
        presented: &ValidatedFile,
        dimensions: ImageDimensions,
    ) -> Self {
        let mut reference = Self::direct_image(presented, dimensions);
        reference.source = MediaValidationIdentity {
            digest: source.source().digest(),
            media_type: source.detected_media_type().clone(),
            reader: source.reader().clone(),
            evidence: source.validation(),
        };
        reference
    }
    /// Describes an image that cannot fit the selected target's presentation bounds.
    pub fn large_image_description(&self) -> serde_json::Value {
        serde_json::json!({ "status": "large_image", "width":self.dimensions.width, "height":self.dimensions.height, "byte_length":self.byte_length.get(), "available_views": self.derived_views.iter().map(crate::ReadViewName::as_str).collect::<Vec<_>>() })
    }
    /// Borrows the identity validated for presentation.
    pub const fn presented(&self) -> &MediaValidationIdentity {
        &self.presented
    }
    /// Borrows the original source identity.
    pub const fn source(&self) -> &MediaValidationIdentity {
        &self.source
    }
    /// Returns the exact presented byte length.
    pub const fn byte_length(&self) -> NonZeroU64 {
        self.byte_length
    }
    /// Returns the neutral presentation kind.
    pub const fn kind(&self) -> MediaPresentationKind {
        self.kind
    }
}
