//! Ordered accepted-input user content.
//!
//! The normative specification is `docs/spec/blob-storage.md`.

use std::{fmt, num::NonZeroU64};

use crate::BlobDigest;

/// Maximum number of ordered parts in one accepted input.
pub const MAX_USER_CONTENT_PARTS: usize = 256;
/// Maximum aggregate UTF-8 bytes across every text part.
pub const MAX_USER_CONTENT_TEXT_BYTES: usize = 1_048_576;
/// Maximum encoded bytes in one declared media type.
pub const MAX_DECLARED_MEDIA_TYPE_BYTES: usize = 255;
/// Maximum encoded bytes in one attachment display filename.
pub const MAX_ATTACHMENT_DISPLAY_FILENAME_BYTES: usize = 255;

/// A nonempty decoded Unicode scalar sequence containing no U+0000.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct NonEmptyUnicodeText(String);

impl fmt::Debug for NonEmptyUnicodeText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NonEmptyUnicodeText(<redacted>)")
    }
}

impl NonEmptyUnicodeText {
    /// Checks one decoded string without trimming or normalization.
    pub fn try_new(value: String) -> Result<Self, NonEmptyUnicodeTextError> {
        let failure = if value.is_empty() {
            Some(NonEmptyUnicodeTextFailure::Empty)
        } else if value.contains('\0') {
            Some(NonEmptyUnicodeTextFailure::ContainsNull)
        } else {
            None
        };

        match failure {
            Some(failure) => Err(NonEmptyUnicodeTextError { value, failure }),
            None => Ok(Self(value)),
        }
    }

    /// Borrows the exact checked text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the exact checked text.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Why a decoded string cannot become user text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NonEmptyUnicodeTextFailure {
    /// The decoded scalar sequence is empty.
    Empty,
    /// The decoded scalar sequence contains U+0000.
    ContainsNull,
    /// The text exceeds the aggregate user-content byte bound.
    TooLong,
}

/// Failed text construction retaining the rejected string unchanged.
#[derive(Clone, Eq, PartialEq)]
pub struct NonEmptyUnicodeTextError {
    value: String,
    failure: NonEmptyUnicodeTextFailure,
}

impl fmt::Debug for NonEmptyUnicodeTextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NonEmptyUnicodeTextError")
            .field("failure", &self.failure)
            .finish()
    }
}

impl NonEmptyUnicodeTextError {
    /// Returns why the rejected string was invalid.
    pub const fn failure(&self) -> NonEmptyUnicodeTextFailure {
        self.failure
    }

    /// Borrows the rejected string unchanged.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the rejected string and failure.
    pub fn into_parts(self) -> (String, NonEmptyUnicodeTextFailure) {
        (self.value, self.failure)
    }
}

/// Closed semantic kind declared for one attachment.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AttachmentKind {
    /// Image content.
    Image,
    /// Page- or document-oriented content.
    Document,
    /// Other file content.
    File,
}

/// Immutable catalog fact needed to render and verify one referenced blob.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttachmentBlobFact {
    digest: BlobDigest,
    byte_length: NonZeroU64,
}

impl AttachmentBlobFact {
    /// Associates one global blob identity with its positive byte length.
    pub const fn new(digest: BlobDigest, byte_length: NonZeroU64) -> Self {
        Self {
            digest,
            byte_length,
        }
    }

    /// Returns the referenced global blob identity.
    pub const fn digest(self) -> BlobDigest {
        self.digest
    }

    /// Returns the immutable positive byte length.
    pub const fn byte_length(self) -> NonZeroU64 {
        self.byte_length
    }
}

/// Exact checked visible-ASCII media-type declaration.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DeclaredMediaType(String);

impl DeclaredMediaType {
    /// Inclusive encoded-byte bound for one declaration.
    pub const MAX_BYTES: usize = MAX_DECLARED_MEDIA_TYPE_BYTES;

    /// Checks one media-type declaration without normalization.
    pub fn try_new(value: String) -> Result<Self, DeclaredMediaTypeError> {
        let failure = if value.is_empty() {
            Some(DeclaredMediaTypeFailure::Empty)
        } else if value.len() > MAX_DECLARED_MEDIA_TYPE_BYTES {
            Some(DeclaredMediaTypeFailure::TooLong)
        } else if !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
            Some(DeclaredMediaTypeFailure::NotVisibleAscii)
        } else {
            None
        };

        match failure {
            Some(failure) => Err(DeclaredMediaTypeError { value, failure }),
            None => Ok(Self(value)),
        }
    }

    /// Borrows the exact declaration.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Closed reason a media-type declaration was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeclaredMediaTypeFailure {
    /// The declaration was empty.
    Empty,
    /// The declaration exceeded its encoded-byte bound.
    TooLong,
    /// A byte was outside visible ASCII.
    NotVisibleAscii,
}

/// Failed media-type construction retaining the rejected value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclaredMediaTypeError {
    value: String,
    failure: DeclaredMediaTypeFailure,
}

impl DeclaredMediaTypeError {
    /// Returns the closed rejection reason.
    pub const fn failure(&self) -> DeclaredMediaTypeFailure {
        self.failure
    }

    /// Borrows the rejected declaration.
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// Exact checked basename shown to a user for one attachment.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct AttachmentDisplayFilename(String);

impl AttachmentDisplayFilename {
    /// Inclusive encoded-byte bound for one display filename.
    pub const MAX_BYTES: usize = MAX_ATTACHMENT_DISPLAY_FILENAME_BYTES;

    /// Checks one display filename without path or Unicode normalization.
    pub fn try_new(value: String) -> Result<Self, AttachmentDisplayFilenameError> {
        let failure = if value.is_empty() {
            Some(AttachmentDisplayFilenameFailure::Empty)
        } else if value.len() > MAX_ATTACHMENT_DISPLAY_FILENAME_BYTES {
            Some(AttachmentDisplayFilenameFailure::TooLong)
        } else if value == "." || value == ".." {
            Some(AttachmentDisplayFilenameFailure::ReservedBasename)
        } else if value.contains('/') || value.contains('\\') {
            Some(AttachmentDisplayFilenameFailure::ContainsPathSeparator)
        } else if value.contains('\0') {
            Some(AttachmentDisplayFilenameFailure::ContainsNull)
        } else {
            None
        };

        match failure {
            Some(failure) => Err(AttachmentDisplayFilenameError { value, failure }),
            None => Ok(Self(value)),
        }
    }

    /// Borrows the exact display filename.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AttachmentDisplayFilename {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AttachmentDisplayFilename(<redacted>)")
    }
}

/// Closed reason a display filename was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttachmentDisplayFilenameFailure {
    /// The filename was empty.
    Empty,
    /// Its encoded bytes exceeded the bound.
    TooLong,
    /// It was `.` or `..`.
    ReservedBasename,
    /// It contained slash or backslash.
    ContainsPathSeparator,
    /// It contained U+0000.
    ContainsNull,
}

/// Failed display-filename construction retaining the rejected value.
#[derive(Clone, Eq, PartialEq)]
pub struct AttachmentDisplayFilenameError {
    value: String,
    failure: AttachmentDisplayFilenameFailure,
}

impl fmt::Debug for AttachmentDisplayFilenameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttachmentDisplayFilenameError")
            .field("failure", &self.failure)
            .finish()
    }
}

impl AttachmentDisplayFilenameError {
    /// Returns the closed rejection reason.
    pub const fn failure(&self) -> AttachmentDisplayFilenameFailure {
        self.failure
    }

    /// Borrows the rejected filename.
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// One exact part of an ordered accepted input.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum UserContentPart {
    /// Exact decoded user text.
    Text {
        /// Checked nonempty text.
        value: NonEmptyUnicodeText,
    },
    /// Immutable blob attachment and caller-declared metadata.
    Attachment {
        /// Global immutable byte identity.
        digest: BlobDigest,
        /// Closed semantic attachment kind.
        kind: AttachmentKind,
        /// Exact checked media-type declaration.
        media_type: DeclaredMediaType,
        /// Optional redacted display basename.
        display_filename: Option<AttachmentDisplayFilename>,
    },
}

impl UserContentPart {
    /// Checks and constructs one exact text part.
    pub fn try_text(value: String) -> Result<Self, NonEmptyUnicodeTextError> {
        Ok(Self::Text {
            value: NonEmptyUnicodeText::try_new(value)?,
        })
    }
}

/// Ordered, nonempty accepted-input content.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct UserContent {
    parts: Vec<UserContentPart>,
}

impl UserContent {
    /// Maximum number of ordered parts in one accepted input.
    pub const MAX_PARTS: usize = MAX_USER_CONTENT_PARTS;
    /// Inclusive aggregate UTF-8 byte bound across all text parts.
    pub const MAX_TEXT_BYTES: usize = MAX_USER_CONTENT_TEXT_BYTES;

    /// Checks and constructs exact single-text content.
    pub fn try_text(value: String) -> Result<Self, NonEmptyUnicodeTextError> {
        let value = NonEmptyUnicodeText::try_new(value)?;
        if value.as_str().len() > Self::MAX_TEXT_BYTES {
            return Err(NonEmptyUnicodeTextError {
                value: value.into_string(),
                failure: NonEmptyUnicodeTextFailure::TooLong,
            });
        }
        Ok(Self {
            parts: vec![UserContentPart::Text { value }],
        })
    }

    /// Checks one complete ordered parts sequence.
    pub fn try_parts(parts: Vec<UserContentPart>) -> Result<Self, UserContentError> {
        if let Some(failure) = user_content_failure(&parts) {
            return Err(UserContentError { parts, failure });
        }
        Ok(Self { parts })
    }

    /// Borrows the ordered parts.
    pub fn parts(&self) -> &[UserContentPart] {
        &self.parts
    }

    /// Returns the ordered parts.
    pub fn into_parts(self) -> Vec<UserContentPart> {
        self.parts
    }

    /// Borrows text when this is exactly one text part.
    pub fn single_text(&self) -> Option<&NonEmptyUnicodeText> {
        match self.parts.as_slice() {
            [UserContentPart::Text { value }] => Some(value),
            _ => None,
        }
    }
}

fn user_content_failure(parts: &[UserContentPart]) -> Option<UserContentFailure> {
    if parts.is_empty() {
        return Some(UserContentFailure::Empty);
    }
    if parts.len() > MAX_USER_CONTENT_PARTS {
        return Some(UserContentFailure::TooManyParts);
    }

    let mut aggregate_text_bytes = 0_usize;
    let mut previous_was_text = false;
    for part in parts {
        match part {
            UserContentPart::Text { value } => {
                if previous_was_text {
                    return Some(UserContentFailure::AdjacentTextParts);
                }
                let Some(next_text_bytes) = aggregate_text_bytes.checked_add(value.as_str().len())
                else {
                    return Some(UserContentFailure::TextTooLarge);
                };
                aggregate_text_bytes = next_text_bytes;
                if aggregate_text_bytes > UserContent::MAX_TEXT_BYTES {
                    return Some(UserContentFailure::TextTooLarge);
                }
                previous_was_text = true;
            }
            UserContentPart::Attachment { .. } => previous_was_text = false,
        }
    }
    None
}

/// Failed content construction retaining the rejected parts unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserContentError {
    parts: Vec<UserContentPart>,
    failure: UserContentFailure,
}

impl UserContentError {
    /// Returns the closed structural rejection reason.
    pub const fn failure(&self) -> UserContentFailure {
        self.failure
    }

    /// Borrows the rejected ordered parts unchanged.
    pub fn parts(&self) -> &[UserContentPart] {
        &self.parts
    }

    /// Returns the rejected parts and failure.
    pub fn into_parts(self) -> (Vec<UserContentPart>, UserContentFailure) {
        (self.parts, self.failure)
    }
}

/// Closed structural rejection for one complete content sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserContentFailure {
    /// No parts were supplied.
    Empty,
    /// The part-count bound was exceeded.
    TooManyParts,
    /// Two text parts were adjacent and therefore noncanonical.
    AdjacentTextParts,
    /// Aggregate text bytes exceeded the bound.
    TextTooLarge,
}

#[cfg(test)]
mod tests {
    use super::{
        AttachmentDisplayFilename, AttachmentDisplayFilenameFailure, AttachmentKind,
        DeclaredMediaType, DeclaredMediaTypeFailure, MAX_USER_CONTENT_TEXT_BYTES,
        NonEmptyUnicodeText, NonEmptyUnicodeTextFailure, UserContent, UserContentFailure,
        UserContentPart,
    };
    use crate::BlobDigest;

    fn attachment(filename: Option<&str>) -> UserContentPart {
        UserContentPart::Attachment {
            digest: BlobDigest::digest(b"attachment"),
            kind: AttachmentKind::Image,
            media_type: DeclaredMediaType::try_new(String::from("image/png"))
                .expect("the fixture media type is valid"),
            display_filename: filename.map(|value| {
                AttachmentDisplayFilename::try_new(value.to_owned())
                    .expect("the fixture filename is valid")
            }),
        }
    }

    #[test]
    fn empty_text_is_rejected_without_rewriting() {
        let empty = String::new();
        let empty_error = NonEmptyUnicodeText::try_new(empty.clone())
            .expect_err("empty text is outside the baseline");
        assert_eq!(empty_error.value(), empty);
        assert_eq!(
            empty_error.into_parts(),
            (empty, NonEmptyUnicodeTextFailure::Empty)
        );
    }

    #[test]
    fn null_bearing_text_is_rejected_without_rewriting() {
        let with_null = String::from("before\0after");
        let null_error = NonEmptyUnicodeText::try_new(with_null.clone())
            .expect_err("text containing U+0000 is outside the baseline");
        assert_eq!(null_error.value(), with_null);
        assert_eq!(
            null_error.into_parts(),
            (with_null, NonEmptyUnicodeTextFailure::ContainsNull)
        );
    }

    #[test]
    fn user_text_debug_is_redacted() {
        let private_text = "private user text";
        let text = NonEmptyUnicodeText::try_new(String::from(private_text))
            .expect("the fixture text is valid");

        assert!(!format!("{text:?}").contains(private_text));
    }

    #[test]
    fn rejected_user_text_debug_is_redacted() {
        let rejected = "private rejected text\0";
        let error = NonEmptyUnicodeText::try_new(String::from(rejected))
            .expect_err("the fixture text contains U+0000");
        let debug = format!("{error:?}");

        assert!(!debug.contains(rejected));
        assert!(debug.contains("ContainsNull"));
    }

    /// content preserves exact scalar spellings.
    #[test]
    fn parts_preserve_exact_scalars() {
        let exact = String::from(" \tline one\r\ncafe\u{301}\n ");
        let parts = vec![
            UserContentPart::try_text(exact.clone()).expect("text is valid"),
            attachment(Some("chart.png")),
        ];
        let content = UserContent::try_parts(parts.clone()).expect("the ordered fixture is valid");

        assert_eq!(content.parts(), parts.as_slice());
    }

    /// part order participates in structural equality.
    #[test]
    fn part_order_participates_in_equality() {
        let text = UserContentPart::try_text(String::from("before")).expect("text is valid");
        let first = UserContent::try_parts(vec![text.clone(), attachment(Some("chart.png"))])
            .expect("the ordered fixture is valid");
        let reordered = UserContent::try_parts(vec![attachment(Some("chart.png")), text])
            .expect("the reordered fixture is structurally valid");

        assert_ne!(first, reordered);
    }

    /// attachment metadata participates in structural
    /// equality.
    #[test]
    fn attachment_metadata_participates_in_equality() {
        let first = UserContent::try_parts(vec![
            UserContentPart::try_text(String::from("before")).expect("text is valid"),
            attachment(Some("chart.png")),
        ])
        .expect("the first fixture is valid");
        let different_filename = UserContent::try_parts(vec![
            UserContentPart::try_text(String::from("before")).expect("text is valid"),
            attachment(Some("diagram.png")),
        ])
        .expect("the differing metadata fixture is valid");

        assert_ne!(first, different_filename);
    }

    #[test]
    fn empty_part_sequence_is_rejected_without_losing_the_parts() {
        let error =
            UserContent::try_parts(Vec::new()).expect_err("an empty part sequence is rejected");

        assert_eq!(error.failure(), UserContentFailure::Empty);
        assert_eq!(error.into_parts(), (Vec::new(), UserContentFailure::Empty));
    }

    #[test]
    fn adjacent_text_parts_are_rejected_without_losing_the_parts() {
        let parts = vec![
            UserContentPart::try_text(String::from("first")).expect("text is valid"),
            UserContentPart::try_text(String::from("second")).expect("text is valid"),
        ];
        let error =
            UserContent::try_parts(parts.clone()).expect_err("adjacent text parts are rejected");

        assert_eq!(error.failure(), UserContentFailure::AdjacentTextParts);
        assert_eq!(
            error.into_parts(),
            (parts, UserContentFailure::AdjacentTextParts)
        );
    }

    #[test]
    fn declared_media_type_rejects_nonvisible_ascii() {
        let media_error = DeclaredMediaType::try_new(String::from("image png"))
            .expect_err("space is not visible media-type data");
        assert_eq!(
            media_error.failure(),
            DeclaredMediaTypeFailure::NotVisibleAscii
        );
    }

    #[test]
    fn attachment_display_filename_rejects_path_spelling() {
        let filename_error = AttachmentDisplayFilename::try_new(String::from("../chart.png"))
            .expect_err("a path spelling is not a display basename");
        assert_eq!(
            filename_error.failure(),
            AttachmentDisplayFilenameFailure::ContainsPathSeparator
        );
    }

    #[test]
    fn attachment_display_filename_preserves_exact_basename() {
        let basename = "chart.png";
        let filename = AttachmentDisplayFilename::try_new(String::from(basename))
            .expect("a basename is valid");

        assert_eq!(filename.as_str(), basename);
    }

    #[test]
    fn attachment_display_filename_debug_is_redacted() {
        let basename = "chart.png";
        let filename = AttachmentDisplayFilename::try_new(String::from(basename))
            .expect("a basename is valid");

        assert!(!format!("{filename:?}").contains(basename));
    }

    #[test]
    fn rejected_attachment_display_filename_debug_is_redacted() {
        let rejected = "../private-chart.png";
        let error = AttachmentDisplayFilename::try_new(String::from(rejected))
            .expect_err("a path spelling is rejected");
        let debug = format!("{error:?}");

        assert!(!debug.contains(rejected));
        assert!(debug.contains("ContainsPathSeparator"));
    }

    #[test]
    fn single_text_has_one_canonical_representation() {
        let exact = " \t\r\n";
        let content =
            UserContent::try_text(String::from(exact)).expect("whitespace remains content");

        assert_eq!(
            content.single_text().map(NonEmptyUnicodeText::as_str),
            Some(exact)
        );
        assert_eq!(content.parts().len(), 1);
    }

    #[test]
    fn single_text_enforces_the_aggregate_byte_bound() {
        let at_bound = UserContent::try_text("x".repeat(MAX_USER_CONTENT_TEXT_BYTES))
            .expect("the exact aggregate text bound is admitted");
        let over_bound = UserContent::try_text("x".repeat(MAX_USER_CONTENT_TEXT_BYTES + 1))
            .expect_err("one byte beyond the aggregate bound is rejected");

        assert_eq!(
            at_bound
                .single_text()
                .map(NonEmptyUnicodeText::as_str)
                .map(str::len),
            Some(MAX_USER_CONTENT_TEXT_BYTES)
        );
        assert_eq!(over_bound.failure(), NonEmptyUnicodeTextFailure::TooLong);
    }
}
