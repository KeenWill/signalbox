//! Shared blob-store contract applied to the filesystem adapter.

#![allow(
    clippy::expect_used,
    reason = "filesystem conformance tests use explicit fixture expectations"
)]

use signalbox_blob_store::BlobObjectKey;
use signalbox_blob_store_filesystem::FilesystemBlobStore;
use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

fn fixture() -> (TempDir, FilesystemBlobStore) {
    let root = TempDir::new().expect("the fixture creates a temporary store root");
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
        .expect("the fixture makes its store root private");
    let store = FilesystemBlobStore::try_new(root.path().to_path_buf())
        .expect("the temporary directory is an admitted store root");
    (root, store)
}

#[tokio::test]
async fn inv059_filesystem_puts_and_reads_exact_bytes() {
    let (_root, store) = fixture();

    signalbox_blob_store::conformance::assert_put_and_exact_read_back(&store).await;
}

#[tokio::test]
async fn inv059_filesystem_reads_exact_bounded_ranges() {
    let (_root, store) = fixture();

    signalbox_blob_store::conformance::assert_exact_range_read_back(&store).await;
}

#[tokio::test]
async fn inv059_filesystem_deduplicates_an_existing_destination() {
    let (_root, store) = fixture();

    signalbox_blob_store::conformance::assert_existing_destination_deduplicates(&store).await;
}

#[tokio::test]
async fn inv059_filesystem_concurrent_publication_is_no_clobber() {
    let (_root, store) = fixture();

    signalbox_blob_store::conformance::assert_concurrent_publication_deduplicates(&store).await;
}

#[tokio::test]
async fn inv059_filesystem_rejects_publication_verification_failure() {
    let (_root, store) = fixture();

    signalbox_blob_store::conformance::assert_verification_failure(&store).await;
}

#[tokio::test]
async fn inv059_filesystem_repairs_a_corrupt_existing_destination() {
    let (root, store) = fixture();
    let expected = signalbox_blob_store::conformance::expected_fixture();
    let key = BlobObjectKey::for_digest(expected.digest());
    let destination = root.path().join(key.as_str());
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(
            destination
                .parent()
                .expect("the deterministic key has a parent"),
        )
        .expect("the fixture creates the private destination parent");
    std::fs::write(
        &destination,
        signalbox_blob_store::conformance::corrupt_fixture_content(),
    )
    .expect("the fixture injects a corrupt destination");
    std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o600))
        .expect("the fixture makes the corrupt destination private");

    signalbox_blob_store::conformance::assert_corrupt_destination_is_repaired(&store).await;
}

#[test]
fn filesystem_rejects_a_nonprivate_root() {
    let root = TempDir::new().expect("the fixture creates a temporary store root");
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755))
        .expect("the fixture makes its store root nonprivate");

    let error = FilesystemBlobStore::try_new(root.path().to_path_buf())
        .expect_err("a nonprivate store root must be rejected");

    assert_eq!(
        error.to_string(),
        "filesystem blob-store root is not private"
    );
}

#[test]
fn filesystem_sweeps_owned_crash_publication_files() {
    let root = TempDir::new().expect("the fixture creates a temporary store root");
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
        .expect("the fixture makes its store root private");
    let publication_directory = root.path().join(".publish-v1");
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    builder
        .create(&publication_directory)
        .expect("the fixture creates the private publication directory");
    let orphan = publication_directory.join("crash-orphan");
    std::fs::write(&orphan, b"unpublished bytes")
        .expect("the fixture creates a publication orphan");
    std::fs::set_permissions(&orphan, std::fs::Permissions::from_mode(0o600))
        .expect("the fixture makes the publication orphan private");

    let _store = FilesystemBlobStore::try_new(root.path().to_path_buf())
        .expect("the store sweeps a provably owned publication orphan");

    assert!(!orphan.exists());
}
