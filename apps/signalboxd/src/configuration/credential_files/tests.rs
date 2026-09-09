use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

/// A private file with arbitrary nonempty synthetic credential bytes.
fn fixture() -> (
    tempfile::NamedTempFile,
    FileCredentialAccess,
    CredentialReference,
) {
    let file = tempfile::NamedTempFile::new().expect("private credential file");
    fs::write(file.path(), b"synthetic-secret").expect("credential bytes");
    let reference = CredentialReference::new("fixture-profile");
    let access = FileCredentialAccess::new(file.path().to_path_buf(), reference.clone());
    (file, access, reference)
}

#[tokio::test]
async fn private_regular_file_is_admitted_at_the_size_ceiling() {
    let (file, access, reference) = fixture();
    let bytes = vec![b'x'; 65_536];
    fs::write(file.path(), &bytes).expect("boundary credential");
    access.validate().expect("startup admission");
    assert_eq!(
        access
            .resolve(&reference)
            .await
            .expect("credential resolves")
            .expose_bytes(),
        bytes
    );
}

#[tokio::test]
async fn oversized_file_is_rejected_at_admission_and_resolution() {
    let (file, access, reference) = fixture();
    file.as_file()
        .set_len(65_537)
        .expect("oversized credential");
    assert_eq!(
        access.validate().expect_err("startup rejects size").failure,
        CredentialAccessFailure::TooLarge
    );
    let error = access
        .resolve(&reference)
        .await
        .expect_err("resolution rejects size");
    assert_eq!(error.reference, reference);
    assert_eq!(error.failure, CredentialAccessFailure::TooLarge);
}

#[tokio::test]
async fn group_or_other_permissions_warn_and_are_read_on_every_resolution() {
    let (file, access, reference) = fixture();
    access.validate().expect("initial private file");
    for mode in [0o640, 0o620, 0o610, 0o604, 0o602, 0o601] {
        fs::set_permissions(file.path(), fs::Permissions::from_mode(mode))
            .expect("changed permissions");
        let credential = access
            .resolve(&reference)
            .await
            .expect("permissive mode does not withhold the credential");
        assert_eq!(credential.expose_bytes(), b"synthetic-secret", "{mode:o}");
        access
            .validate()
            .expect("admission accepts permissive mode after warning");
    }
}

#[test]
fn credential_owner_must_equal_the_effective_user() {
    let (file, _, _) = fixture();
    let metadata = file.as_file().metadata().expect("metadata");
    let different_user = metadata.uid().wrapping_add(1);
    assert_eq!(
        validate_credential_metadata(&metadata, different_user),
        Err(CredentialAccessFailure::WrongOwner)
    );
}

#[tokio::test]
async fn nonregular_targets_are_rejected_without_reading() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let fifo = directory.path().join("fifo");
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .expect("fixture FIFO");
    let socket = directory.path().join("socket");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("fixture socket");
    let reference = CredentialReference::new("special-file-profile");
    for path in [directory.path(), fifo.as_path(), socket.as_path()] {
        let access = FileCredentialAccess::new(path.to_path_buf(), reference.clone());
        assert_eq!(
            access
                .validate()
                .expect_err("startup refuses special file")
                .failure,
            CredentialAccessFailure::NotRegularFile
        );
        assert_eq!(
            access
                .resolve(&reference)
                .await
                .expect_err("resolution refuses special file")
                .failure,
            CredentialAccessFailure::NotRegularFile
        );
    }
}

#[tokio::test]
async fn symlinks_admit_the_final_target_and_recheck_replacements() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let (target, _, reference) = fixture();
    let alias = directory.path().join("alias");
    symlink(target.path(), &alias).expect("credential symlink");
    let access = FileCredentialAccess::new(alias.clone(), reference.clone());
    access.validate().expect("private target is admitted");
    assert_eq!(
        access
            .resolve(&reference)
            .await
            .expect("symlink resolves")
            .expose_bytes(),
        b"synthetic-secret"
    );
    fs::remove_file(&alias).expect("remove alias");
    symlink(directory.path(), &alias).expect("replace alias target");
    assert_eq!(
        access
            .resolve(&reference)
            .await
            .expect_err("replacement is checked")
            .failure,
        CredentialAccessFailure::NotRegularFile
    );
}

#[test]
fn admission_errors_identify_the_reference_without_path_or_contents() {
    let (file, access, reference) = fixture();
    file.as_file().set_len(65_537).expect("oversized file");
    let error = access.validate().expect_err("size admission fails");
    assert_eq!(error.reference, reference);
    assert_eq!(error.failure, CredentialAccessFailure::TooLarge);
    let diagnostic = format!("{error} {error:?}");
    assert!(diagnostic.contains(reference.as_str()));
    assert!(diagnostic.contains("TooLarge"));
    assert!(!diagnostic.contains(file.path().to_str().expect("fixture path")));
    assert!(!diagnostic.contains("synthetic-secret"));
}
