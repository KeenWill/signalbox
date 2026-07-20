//! Baseline accepted-input user content.
//!
//! ADR-0037 (`docs/decisions/0037-baseline-user-content.md`) is the normative
//! specification. The baseline is one exact text variant. Construction
//! rejects empty text and U+0000 while preserving every other scalar,
//! whitespace character, and line ending unchanged.
//!
//! Construction also rejects text whose UTF-8 encoding exceeds the
//! provisional owner-decided bound recorded in the decision log
//! (`docs/decisions.md`, 2026-07-20): one mebibyte. The bound counts UTF-8
//! bytes rather than scalar values because the durable representation
//! (PostgreSQL `octet_length` over UTF-8 `text`) and the wire both measure
//! bytes; a scalar-count bound would admit up to four mebibytes of bytes.

/// A nonempty decoded Unicode scalar sequence containing no U+0000, at most
/// [`Self::MAX_UTF8_BYTES`] UTF-8 bytes long.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct NonEmptyUnicodeText(String);

impl NonEmptyUnicodeText {
    /// The provisional owner-decided inclusive maximum text size: one
    /// mebibyte of UTF-8 bytes.
    ///
    /// This is a floor for admission, not the resource-governance policy;
    /// ADR-0037's resource-governance open question remains open. The bound
    /// rejects before construction and never truncates or rewrites content.
    pub const MAX_UTF8_BYTES: usize = 1_048_576;

    /// Checks one decoded string without trimming or normalization.
    ///
    /// Checks run in a fixed order — emptiness, then the size bound, then
    /// U+0000 — so a retained rejected string never exceeds
    /// [`Self::MAX_UTF8_BYTES`].
    pub fn try_new(value: String) -> Result<Self, NonEmptyUnicodeTextError> {
        if value.is_empty() {
            return Err(NonEmptyUnicodeTextError {
                value: Some(value),
                failure: NonEmptyUnicodeTextFailure::Empty,
            });
        }
        if value.len() > Self::MAX_UTF8_BYTES {
            return Err(NonEmptyUnicodeTextError {
                value: None,
                failure: NonEmptyUnicodeTextFailure::Oversized {
                    utf8_byte_length: value.len(),
                },
            });
        }
        if value.contains('\0') {
            return Err(NonEmptyUnicodeTextError {
                value: Some(value),
                failure: NonEmptyUnicodeTextFailure::ContainsNull,
            });
        }
        Ok(Self(value))
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

/// Why a decoded string cannot become baseline user text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NonEmptyUnicodeTextFailure {
    /// The decoded scalar sequence is empty.
    Empty,
    /// The decoded scalar sequence contains U+0000.
    ContainsNull,
    /// The UTF-8 encoding exceeds [`NonEmptyUnicodeText::MAX_UTF8_BYTES`].
    Oversized {
        /// The rejected encoding's exact UTF-8 length in bytes.
        utf8_byte_length: usize,
    },
}

/// Failed text construction retaining the rejected input where that is safe.
///
/// Empty and U+0000 failures retain the rejected string unchanged. The
/// oversized failure deliberately deviates from that retain-the-input
/// pattern: it retains only the byte length its failure variant carries,
/// because holding an arbitrarily large rejected string inside the error
/// value would recreate the resource hazard the size bound exists to
/// prevent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NonEmptyUnicodeTextError {
    value: Option<String>,
    failure: NonEmptyUnicodeTextFailure,
}

impl NonEmptyUnicodeTextError {
    /// Returns why the rejected string was invalid.
    pub const fn failure(&self) -> NonEmptyUnicodeTextFailure {
        self.failure
    }

    /// Borrows the retained rejected string; `None` exactly for
    /// [`NonEmptyUnicodeTextFailure::Oversized`], which retains only its
    /// byte length.
    pub fn retained_value(&self) -> Option<&str> {
        self.value.as_deref()
    }

    /// Returns the retained rejected string and failure.
    pub fn into_parts(self) -> (Option<String>, NonEmptyUnicodeTextFailure) {
        (self.value, self.failure)
    }
}

/// The complete baseline user-content algebra.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum UserContent {
    /// Exact decoded user text.
    Text {
        /// The checked nonempty text value.
        value: NonEmptyUnicodeText,
    },
}

impl UserContent {
    /// Checks and constructs exact baseline text.
    pub fn try_text(value: String) -> Result<Self, NonEmptyUnicodeTextError> {
        Ok(Self::Text {
            value: NonEmptyUnicodeText::try_new(value)?,
        })
    }

    /// Borrows the exact text value.
    pub const fn text(&self) -> &NonEmptyUnicodeText {
        match self {
            Self::Text { value } => value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{NonEmptyUnicodeText, NonEmptyUnicodeTextFailure, UserContent};

    /// ADR-0037: empty text and U+0000 fail construction while retaining the
    /// rejected decoded value.
    #[test]
    fn empty_and_null_text_are_rejected_without_rewriting() {
        let empty = String::new();
        let empty_error = NonEmptyUnicodeText::try_new(empty.clone())
            .expect_err("empty text is outside the baseline");
        assert_eq!(empty_error.retained_value(), Some(empty.as_str()));
        assert_eq!(
            empty_error.into_parts(),
            (Some(empty), NonEmptyUnicodeTextFailure::Empty)
        );

        let with_null = String::from("before\0after");
        let null_error = NonEmptyUnicodeText::try_new(with_null.clone())
            .expect_err("text containing U+0000 is outside the baseline");
        assert_eq!(null_error.retained_value(), Some(with_null.as_str()));
        assert_eq!(
            null_error.into_parts(),
            (Some(with_null), NonEmptyUnicodeTextFailure::ContainsNull)
        );
    }

    /// Decision log 2026-07-20: text of exactly 1,048,576 UTF-8 bytes is
    /// within the provisional bound, for one-byte scalars and for a
    /// multi-byte scalar ending exactly at the bound alike.
    #[test]
    fn text_at_the_exact_byte_bound_is_constructible() {
        assert_eq!(
            NonEmptyUnicodeText::MAX_UTF8_BYTES,
            1_048_576,
            "the provisional owner-decided bound is one mebibyte of UTF-8 bytes"
        );

        let ascii_at_bound = "a".repeat(1_048_576);
        let ascii = NonEmptyUnicodeText::try_new(ascii_at_bound.clone())
            .expect("text of exactly 1,048,576 UTF-8 bytes is admitted");
        assert_eq!(ascii.as_str(), ascii_at_bound);

        let mut multibyte_at_bound = "a".repeat(1_048_574);
        multibyte_at_bound.push('\u{e9}');
        let multibyte = NonEmptyUnicodeText::try_new(multibyte_at_bound.clone())
            .expect("a two-byte scalar ending exactly at the bound is admitted");
        assert_eq!(multibyte.as_str(), multibyte_at_bound);
    }

    /// Decision log 2026-07-20: one byte over the bound fails construction,
    /// and the error retains only the byte length, never the oversized
    /// content.
    #[test]
    fn oversized_text_is_rejected_retaining_only_its_byte_length() {
        let oversized = "a".repeat(1_048_577);
        let error = NonEmptyUnicodeText::try_new(oversized)
            .expect_err("text over the one-mebibyte provisional bound is outside the baseline");
        assert_eq!(
            error.retained_value(),
            None,
            "an oversized rejection must not retain the oversized content"
        );
        assert_eq!(
            error.into_parts(),
            (
                None,
                NonEmptyUnicodeTextFailure::Oversized {
                    utf8_byte_length: 1_048_577,
                }
            )
        );
    }

    /// Decision log 2026-07-20: the bound counts UTF-8 bytes, not scalar
    /// values — a two-byte scalar straddling the bound is rejected even
    /// though the text has exactly 1,048,576 scalar values.
    #[test]
    fn multi_byte_scalar_straddling_the_byte_bound_is_rejected() {
        let mut straddling = "a".repeat(1_048_575);
        straddling.push('\u{e9}');
        assert_eq!(straddling.chars().count(), 1_048_576);

        let error = NonEmptyUnicodeText::try_new(straddling)
            .expect_err("a scalar-count bound would wrongly admit this text");
        assert_eq!(
            error.into_parts(),
            (
                None,
                NonEmptyUnicodeTextFailure::Oversized {
                    utf8_byte_length: 1_048_577,
                }
            )
        );
    }

    /// Decision log 2026-07-20: the size bound is checked before U+0000, so
    /// no retained rejected string can exceed the bound.
    #[test]
    fn oversized_text_containing_null_reports_oversized_without_retention() {
        let mut oversized_with_null = "a".repeat(1_048_576);
        oversized_with_null.push('\0');

        let error = NonEmptyUnicodeText::try_new(oversized_with_null)
            .expect_err("oversized text is outside the baseline whatever it contains");
        assert_eq!(
            error.into_parts(),
            (
                None,
                NonEmptyUnicodeTextFailure::Oversized {
                    utf8_byte_length: 1_048_577,
                }
            )
        );
    }

    /// INV-005 / INV-012: content preserves exact scalars, whitespace, and
    /// line endings, and equality does not normalize them.
    #[test]
    fn inv005_inv012_text_is_exact_and_structurally_equal() {
        let exact = String::from(" \tline one\r\ncafe\u{301}\n ");
        let content = UserContent::try_text(exact.clone()).expect("nonempty text is valid");

        assert_eq!(content.text().as_str(), exact);
        assert_eq!(
            content,
            UserContent::try_text(exact).expect("the same text remains valid")
        );
        assert_ne!(
            content,
            UserContent::try_text(String::from(" \tline one\ncafé\n "))
                .expect("normalization-distinct text is still valid")
        );
    }

    /// ADR-0037: whitespace-only content remains constructible.
    #[test]
    fn whitespace_only_text_is_content() {
        assert_eq!(
            UserContent::try_text(String::from(" \t\r\n"))
                .expect("whitespace is content")
                .text()
                .as_str(),
            " \t\r\n"
        );
    }
}
