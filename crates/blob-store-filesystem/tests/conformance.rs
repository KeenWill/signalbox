//! Shared blob-store contract applied to the filesystem adapter.

#![allow(
    clippy::expect_used,
    reason = "filesystem conformance tests use explicit fixture expectations"
)]

use std::{num::NonZeroU64, path::Path};

use signalbox_blob_store::{BlobObjectKey, BlobStore, BlobStoreFailureKind};
use signalbox_blob_store_filesystem::FilesystemBlobStore;
use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

fn try_fixture_in(parent: &Path) -> Option<(TempDir, FilesystemBlobStore)> {
    let root = tempfile::Builder::new()
        .prefix("signalbox-blob-store-")
        .tempdir_in(parent)
        .ok()?;
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).ok()?;
    let store = FilesystemBlobStore::try_new(root.path().to_path_buf()).ok()?;
    Some((root, store))
}

fn fixture() -> (TempDir, FilesystemBlobStore) {
    let working_directory = std::env::current_dir().expect("the test working directory resolves");
    try_fixture_in(&working_directory)
        .or_else(|| try_fixture_in(Path::new("/var/tmp")))
        .or_else(|| try_fixture_in(&std::env::temp_dir()))
        .expect("one test location is positively classified durable local storage")
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
async fn inv059_filesystem_rejects_oversized_ranges() {
    let (_root, store) = fixture();

    signalbox_blob_store::conformance::assert_oversized_range_is_rejected(&store).await;
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
    let (root, store) = fixture();
    let publication_directory = root.path().join(".publish-v1");
    let orphan = publication_directory.join("crash-orphan");
    std::fs::write(&orphan, b"unpublished bytes")
        .expect("the fixture creates a publication orphan");
    std::fs::set_permissions(&orphan, std::fs::Permissions::from_mode(0o600))
        .expect("the fixture makes the publication orphan private");
    drop(store);

    let _store = FilesystemBlobStore::try_new(root.path().to_path_buf())
        .expect("the store sweeps a provably owned publication orphan");

    assert!(!orphan.exists());
}

#[tokio::test]
async fn filesystem_rejects_intermediate_symlinks_on_recorded_reads() {
    let (root, store) = fixture();
    let outside = TempDir::new().expect("the fixture creates an outside directory");
    let outside_file = outside.path().join("object");
    std::fs::write(&outside_file, b"outside bytes").expect("the outside file is created");
    std::fs::set_permissions(&outside_file, std::fs::Permissions::from_mode(0o600))
        .expect("the outside file is private");
    std::os::unix::fs::symlink(outside.path(), root.path().join("alias"))
        .expect("the intermediate symlink is created");
    let key = BlobObjectKey::try_from_recorded("alias/object")
        .expect("the recorded fixture key is lexically safe");

    let error = store
        .open(&key)
        .await
        .expect_err("an intermediate symlink must not escape the store root");

    assert_eq!(error.kind(), BlobStoreFailureKind::Unavailable);
}

#[tokio::test]
async fn filesystem_rejects_reserved_publication_keys() {
    let (root, store) = fixture();
    let unpublished = root.path().join(".publish-v1/unpublished");
    std::fs::write(&unpublished, b"unpublished bytes").expect("the unpublished fixture is created");
    std::fs::set_permissions(&unpublished, std::fs::Permissions::from_mode(0o600))
        .expect("the unpublished fixture is private");
    let key = BlobObjectKey::try_from_recorded(".publish-v1/unpublished")
        .expect("the backend-reserved fixture key is lexically safe");

    let error = store
        .open(&key)
        .await
        .expect_err("the reserved publication subtree must not be readable");

    assert_eq!(error.kind(), BlobStoreFailureKind::Unavailable);
}

#[cfg(not(target_vendor = "apple"))]
#[tokio::test]
async fn filesystem_rejects_fifo_candidates_without_waiting_for_a_writer() {
    let (root, store) = fixture();
    let fifo = root.path().join("recorded-fifo");
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .expect("the recorded FIFO fixture is created");
    let key = BlobObjectKey::try_from_recorded("recorded-fifo")
        .expect("the FIFO fixture key is lexically safe");

    let error = store
        .open(&key)
        .await
        .expect_err("a FIFO must be rejected without waiting for a writer");

    assert_eq!(error.kind(), BlobStoreFailureKind::Unavailable);
}

#[tokio::test]
async fn filesystem_range_reverifies_the_generation_it_reads() {
    let (root, store) = fixture();
    let expected = signalbox_blob_store::conformance::expected_fixture();
    let key = BlobObjectKey::for_digest(expected.digest());
    store
        .put(
            expected,
            Box::new(std::io::Cursor::new(
                signalbox_blob_store::conformance::fixture_content().to_vec(),
            )),
        )
        .await
        .expect("the valid range fixture is published");
    let destination = root.path().join(key.as_str());
    std::fs::write(
        &destination,
        signalbox_blob_store::conformance::corrupt_fixture_content(),
    )
    .expect("the published generation is corrupted in place");

    let error = store
        .open_range(expected, &key, 0, NonZeroU64::MIN)
        .await
        .expect_err("the range must be retained only from a verified generation");

    assert_eq!(error.kind(), BlobStoreFailureKind::VerificationFailed);
}
