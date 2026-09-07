//! Frozen review policy tests for `docs/spec/review-workflows.md`.

use super::*;

#[test]
fn version_one_policy_has_exact_threshold_tuple() {
    assert_eq!(
        ReviewPolicy::version_one()
            .minimum_judge_confidence()
            .basis_points(),
        7_000
    );
    assert_eq!(
        ReviewPolicy::version_one()
            .minimum_publication_confidence()
            .basis_points(),
        8_000
    );
}

#[test]
fn policy_rejects_publication_threshold_below_judgment() {
    let invalid = ReviewPolicy::try_new(
        ReviewPolicyVersion::one(),
        ReviewConfidence::try_from_basis_points(8_001).expect("bounded confidence"),
        ReviewConfidence::try_from_basis_points(8_000).expect("bounded confidence"),
    )
    .expect_err("publication cannot be easier than judgment");
    assert_eq!(
        invalid.into_parts().1.basis_points(),
        8_001,
        "rejected policy remains inspectable"
    );
}

#[test]
fn version_one_policy_rejects_noncanonical_tuple() {
    let noncanonical_version_one = ReviewPolicy::try_new(
        ReviewPolicyVersion::one(),
        ReviewConfidence::try_from_basis_points(7_000).expect("bounded confidence"),
        ReviewConfidence::try_from_basis_points(8_001).expect("bounded confidence"),
    )
    .expect_err("version one has one exact threshold tuple");
    assert_eq!(
        noncanonical_version_one.into_parts(),
        (
            ReviewPolicyVersion::one(),
            ReviewConfidence::try_from_basis_points(7_000).expect("bounded confidence"),
            ReviewConfidence::try_from_basis_points(8_001).expect("bounded confidence"),
        )
    );
}

#[test]
fn unknown_policy_version_is_rejected() {
    let error = ReviewPolicy::try_new(
        ReviewPolicyVersion::try_new(2).expect("positive version"),
        ReviewConfidence::try_from_basis_points(7_000).expect("bounded confidence"),
        ReviewConfidence::try_from_basis_points(8_000).expect("bounded confidence"),
    )
    .expect_err("unknown policy versions fail closed");
    assert_eq!(
        error.into_parts(),
        (
            ReviewPolicyVersion::try_new(2).expect("positive version"),
            ReviewConfidence::try_from_basis_points(7_000).expect("bounded confidence"),
            ReviewConfidence::try_from_basis_points(8_000).expect("bounded confidence"),
        )
    );
}
