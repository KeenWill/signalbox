//! Bounded review value tests for `docs/spec/review-workflows.md`.

use super::*;

#[test]
fn review_key_rejects_utf8_content_over_byte_budget() {
    let too_long =
        ReviewKey::try_new("a".repeat(REVIEW_KEY_MAXIMUM_BYTES + 1)).expect_err("keys are bounded");
    assert_eq!(
        too_long.failure(),
        ReviewValueFailure::TooLong {
            maximum_bytes: REVIEW_KEY_MAXIMUM_BYTES
        }
    );
}
