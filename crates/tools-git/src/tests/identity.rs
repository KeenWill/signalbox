//! Injected commit-identity admission properties.

use crate::identity::{GitIdentity, InvalidGitIdentity};
use crate::tests::support::{AUTHOR_EMAIL, AUTHOR_NAME};

#[test]
fn injected_identity_rejects_signature_delimiters() {
    let invalid_name = GitIdentity::try_new("Bad<Name", AUTHOR_EMAIL);
    let invalid_email = GitIdentity::try_new(AUTHOR_NAME, "bad@example.test>");

    assert_eq!(invalid_name, Err(InvalidGitIdentity));
    assert_eq!(invalid_email, Err(InvalidGitIdentity));
}

#[test]
fn injected_identity_rejects_padded_fields() {
    let invalid_name = GitIdentity::try_new(" Signalbox Fixer", AUTHOR_EMAIL);
    let invalid_email = GitIdentity::try_new(AUTHOR_NAME, "fixer@example.test ");

    assert_eq!(invalid_name, Err(InvalidGitIdentity));
    assert_eq!(invalid_email, Err(InvalidGitIdentity));
}

#[test]
fn injected_identity_rejects_control_characters() {
    let invalid_name = GitIdentity::try_new("Signalbox\tAdmin", AUTHOR_EMAIL);
    let invalid_email = GitIdentity::try_new(AUTHOR_NAME, "fixer\u{0085}@example.test");

    assert_eq!(invalid_name, Err(InvalidGitIdentity));
    assert_eq!(invalid_email, Err(InvalidGitIdentity));
}
