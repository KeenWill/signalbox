//! Bounded review values for `docs/spec/review-workflows.md`.

use std::num::{NonZeroU32, NonZeroU64};

const REVIEW_KEY_MAXIMUM_BYTES: usize = 1_024;
const REVIEW_TEXT_MAXIMUM_BYTES: usize = 65_536;
const REVIEW_CONFIDENCE_MAXIMUM_BASIS_POINTS: u16 = 10_000;

/// Exact bounded text used for opaque provider, repository, revision, path, and external keys.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ReviewKey(String);

impl ReviewKey {
    /// Checks a nonempty key without trimming or normalization.
    pub fn try_new(value: String) -> Result<Self, ReviewValueError> {
        validate_review_value(value, REVIEW_KEY_MAXIMUM_BYTES).map(Self)
    }

    /// Borrows the exact checked key.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the exact checked key.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Exact bounded narrative text used for finding content and reasons.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ReviewText(String);

impl ReviewText {
    /// Checks nonempty text without trimming or normalization.
    pub fn try_new(value: String) -> Result<Self, ReviewValueError> {
        validate_review_value(value, REVIEW_TEXT_MAXIMUM_BYTES).map(Self)
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

fn validate_review_value(value: String, maximum_bytes: usize) -> Result<String, ReviewValueError> {
    let failure = if value.is_empty() {
        Some(ReviewValueFailure::Empty)
    } else if value.contains('\0') {
        Some(ReviewValueFailure::ContainsNull)
    } else if value.len() > maximum_bytes {
        Some(ReviewValueFailure::TooLong { maximum_bytes })
    } else {
        None
    };

    match failure {
        Some(failure) => Err(ReviewValueError { value, failure }),
        None => Ok(value),
    }
}

/// Why an exact review-workflow string cannot be admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewValueFailure {
    /// The string is empty.
    Empty,
    /// The string contains U+0000.
    ContainsNull,
    /// The UTF-8 representation exceeds the field's byte bound.
    TooLong {
        /// Maximum admitted UTF-8 byte count.
        maximum_bytes: usize,
    },
}

/// Failed string construction retaining the rejected value unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewValueError {
    value: String,
    failure: ReviewValueFailure,
}

impl ReviewValueError {
    /// Returns why the string was rejected.
    pub const fn failure(&self) -> ReviewValueFailure {
        self.failure
    }

    /// Borrows the rejected string.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the rejected string and failure.
    pub fn into_parts(self) -> (String, ReviewValueFailure) {
        (self.value, self.failure)
    }
}

/// A positive external change-request number.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReviewChangeRequestNumber(NonZeroU64);

impl ReviewChangeRequestNumber {
    /// Checks that `value` is positive.
    pub const fn try_new(value: u64) -> Result<Self, ReviewPositiveNumberError> {
        match NonZeroU64::new(value) {
            Some(value) => Ok(Self(value)),
            None => Err(ReviewPositiveNumberError),
        }
    }

    /// Returns the positive integer value.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// A one-based finding-event or external-observation ordinal.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReviewEventOrdinal(NonZeroU32);

impl ReviewEventOrdinal {
    /// Returns ordinal one.
    pub const fn one() -> Self {
        Self(NonZeroU32::MIN)
    }

    /// Checks that `value` is positive.
    pub const fn try_new(value: u32) -> Result<Self, ReviewPositiveNumberError> {
        match NonZeroU32::new(value) {
            Some(value) => Ok(Self(value)),
            None => Err(ReviewPositiveNumberError),
        }
    }

    /// Returns the one-based integer value.
    pub const fn get(self) -> u32 {
        self.0.get()
    }

    pub(super) fn checked_successor(self) -> Option<Self> {
        match self.get().checked_add(1) {
            Some(value) => NonZeroU32::new(value).map(Self),
            None => None,
        }
    }
}

/// A zero value where the review workflow requires a positive integer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewPositiveNumberError;

/// Exact confidence in basis points.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReviewConfidence(pub(super) u16);

impl ReviewConfidence {
    /// Checks a basis-point value from zero through 10,000.
    pub const fn try_from_basis_points(basis_points: u16) -> Result<Self, ReviewConfidenceError> {
        if basis_points <= REVIEW_CONFIDENCE_MAXIMUM_BASIS_POINTS {
            Ok(Self(basis_points))
        } else {
            Err(ReviewConfidenceError { basis_points })
        }
    }

    /// Returns the exact basis-point value.
    pub const fn basis_points(self) -> u16 {
        self.0
    }
}

/// Confidence outside the closed zero-through-10,000 basis-point range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewConfidenceError {
    basis_points: u16,
}

impl ReviewConfidenceError {
    /// Returns the rejected basis-point value.
    pub const fn basis_points(self) -> u16 {
        self.basis_points
    }
}

/// The independent confidence axes carried by one review finding.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ReviewFindingConfidenceAxes {
    is_real_confidence: ReviewConfidence,
    severity_label_confidence: ReviewConfidence,
}

impl ReviewFindingConfidenceAxes {
    /// Creates explicitly labeled is-real and severity-label confidence axes.
    pub const fn new(
        is_real_confidence: ReviewConfidence,
        severity_label_confidence: ReviewConfidence,
    ) -> Self {
        Self {
            is_real_confidence,
            severity_label_confidence,
        }
    }

    /// Returns confidence that the issue is real.
    pub const fn is_real_confidence(self) -> ReviewConfidence {
        self.is_real_confidence
    }

    /// Returns confidence that the severity label is correct.
    pub const fn severity_label_confidence(self) -> ReviewConfidence {
        self.severity_label_confidence
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests use explicit fixture expectations"
)]
mod tests;
