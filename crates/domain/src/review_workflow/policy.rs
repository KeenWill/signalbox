//! Frozen review policy for `docs/spec/review-workflows.md`.

use super::value::{ReviewConfidence, ReviewPositiveNumberError};
use std::num::NonZeroU32;

const REVIEW_POLICY_VERSION_ONE_MINIMUM_JUDGE_BASIS_POINTS: u16 = 7_000;
const REVIEW_POLICY_VERSION_ONE_MINIMUM_PUBLICATION_BASIS_POINTS: u16 = 8_000;

/// An ordinal review-policy version.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReviewPolicyVersion(NonZeroU32);

impl ReviewPolicyVersion {
    /// Returns version one.
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

    /// Returns the positive integer version.
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// Complete confidence policy frozen into one review run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewPolicy {
    pub(super) version: ReviewPolicyVersion,
    pub(super) minimum_judge_confidence: ReviewConfidence,
    pub(super) minimum_publication_confidence: ReviewConfidence,
}

impl ReviewPolicy {
    /// Constructs a policy, rejecting unordered thresholds or a noncanonical
    /// version-one tuple.
    pub const fn try_new(
        version: ReviewPolicyVersion,
        minimum_judge_confidence: ReviewConfidence,
        minimum_publication_confidence: ReviewConfidence,
    ) -> Result<Self, ReviewPolicyError> {
        let is_unsupported_version = version.get() != ReviewPolicyVersion::one().get();
        let is_noncanonical_version_one = minimum_judge_confidence.basis_points()
            != REVIEW_POLICY_VERSION_ONE_MINIMUM_JUDGE_BASIS_POINTS
            || minimum_publication_confidence.basis_points()
                != REVIEW_POLICY_VERSION_ONE_MINIMUM_PUBLICATION_BASIS_POINTS;
        if is_unsupported_version
            || is_noncanonical_version_one
            || minimum_publication_confidence.basis_points()
                < minimum_judge_confidence.basis_points()
        {
            Err(ReviewPolicyError {
                version,
                minimum_judge_confidence,
                minimum_publication_confidence,
            })
        } else {
            Ok(Self {
                version,
                minimum_judge_confidence,
                minimum_publication_confidence,
            })
        }
    }

    /// Returns the accepted version-one 70%/80% policy.
    pub const fn version_one() -> Self {
        Self {
            version: ReviewPolicyVersion::one(),
            minimum_judge_confidence: ReviewConfidence(
                REVIEW_POLICY_VERSION_ONE_MINIMUM_JUDGE_BASIS_POINTS,
            ),
            minimum_publication_confidence: ReviewConfidence(
                REVIEW_POLICY_VERSION_ONE_MINIMUM_PUBLICATION_BASIS_POINTS,
            ),
        }
    }

    /// Returns the policy version.
    pub const fn version(self) -> ReviewPolicyVersion {
        self.version
    }

    /// Returns the minimum confidence for judgment.
    pub const fn minimum_judge_confidence(self) -> ReviewConfidence {
        self.minimum_judge_confidence
    }

    /// Returns the minimum confidence for unattended publication.
    pub const fn minimum_publication_confidence(self) -> ReviewConfidence {
        self.minimum_publication_confidence
    }
}

/// A noncanonical or unordered review-policy tuple.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewPolicyError {
    version: ReviewPolicyVersion,
    minimum_judge_confidence: ReviewConfidence,
    minimum_publication_confidence: ReviewConfidence,
}

impl ReviewPolicyError {
    /// Returns the rejected complete policy tuple.
    pub const fn into_parts(self) -> (ReviewPolicyVersion, ReviewConfidence, ReviewConfidence) {
        (
            self.version,
            self.minimum_judge_confidence,
            self.minimum_publication_confidence,
        )
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests use explicit fixture expectations"
)]
mod tests;
