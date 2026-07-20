//! Baseline accepted-input user content.
//!
//! ADR-0037 (`docs/decisions/0037-baseline-user-content.md`) is the normative
//! specification. The baseline is one exact text variant. Construction
//! rejects empty text and U+0000 while preserving every other scalar,
//! whitespace character, and line ending unchanged.

/// A nonempty decoded Unicode scalar sequence containing no U+0000.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct NonEmptyUnicodeText(String);

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

/// Why a decoded string cannot become baseline user text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NonEmptyUnicodeTextFailure {
    /// The decoded scalar sequence is empty.
    Empty,
    /// The decoded scalar sequence contains U+0000.
    ContainsNull,
}

/// Failed text construction retaining the rejected string unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NonEmptyUnicodeTextError {
    value: String,
    failure: NonEmptyUnicodeTextFailure,
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
        assert_eq!(empty_error.value(), empty);
        assert_eq!(
            empty_error.into_parts(),
            (empty, NonEmptyUnicodeTextFailure::Empty)
        );

        let with_null = String::from("before\0after");
        let null_error = NonEmptyUnicodeText::try_new(with_null.clone())
            .expect_err("text containing U+0000 is outside the baseline");
        assert_eq!(null_error.value(), with_null);
        assert_eq!(
            null_error.into_parts(),
            (with_null, NonEmptyUnicodeTextFailure::ContainsNull)
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
