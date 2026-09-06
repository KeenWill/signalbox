//! Shared blob-store contract applied to the filesystem adapter.

#![allow(
    clippy::expect_used,
    reason = "filesystem conformance tests use explicit fixture expectations"
)]

use std::{num::NonZeroU64, path::Path};

use signalbox_blob_store::{BlobObjectKey, BlobStore, BlobStoreFailureKind};
use signalbox_blob_store_filesystem::FilesystemBlobStore;
use tempfile::TempDir;
use tokio::io::AsyncReadExt as _;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

fn try_fixture_in(parent: &Path) -> Option<(TempDir, FilesystemBlobStore)> {
    let root = tempfile::Builder::new()
        .prefix("signalbox-blob-store-")
        .tempdir_in(parent)
        .ok()?;
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).ok()?;
    let store = FilesystemBlobStore::try_new_for_conformance(root.path().to_path_buf()).ok()?;
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
async fn filesystem_puts_and_reads_exact_bytes() {
    let (_root, store) = fixture();

    signalbox_blob_store::conformance::assert_put_and_exact_read_back(&store).await;
}

#[tokio::test]
async fn filesystem_reads_exact_bounded_ranges() {
    let (_root, store) = fixture();

    signalbox_blob_store::conformance::assert_exact_range_read_back(&store).await;
}

#[tokio::test]
async fn filesystem_rejects_oversized_ranges() {
    let (_root, store) = fixture();

    signalbox_blob_store::conformance::assert_oversized_range_is_rejected(&store).await;
}

#[tokio::test]
async fn filesystem_deduplicates_an_existing_destination() {
    let (_root, store) = fixture();

    signalbox_blob_store::conformance::assert_existing_destination_deduplicates(&store).await;
}

#[tokio::test]
async fn filesystem_concurrent_publication_is_no_clobber() {
    let (_root, store) = fixture();

    signalbox_blob_store::conformance::assert_concurrent_publication_deduplicates(&store).await;
}

#[tokio::test]
async fn filesystem_rejects_publication_verification_failure() {
    let (_root, store) = fixture();

    signalbox_blob_store::conformance::assert_verification_failure(&store).await;
}

#[tokio::test]
async fn filesystem_repairs_a_corrupt_existing_destination() {
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

    let _store = FilesystemBlobStore::try_new_for_conformance(root.path().to_path_buf())
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

#[tokio::test]
async fn filesystem_rejects_reserved_namespace_marker_key() {
    let (root, store) = fixture();
    let marker = root.path().join(".signalbox-blob-namespace-v1");
    std::fs::write(&marker, b"namespace marker").expect("the namespace marker fixture is created");
    std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o600))
        .expect("the namespace marker fixture is private");
    let key = BlobObjectKey::try_from_recorded(".signalbox-blob-namespace-v1")
        .expect("the backend-reserved fixture key is lexically safe");

    let error = store
        .open(&key)
        .await
        .expect_err("the reserved namespace marker must not be readable");

    assert_eq!(error.kind(), BlobStoreFailureKind::Unavailable);
}

#[tokio::test]
async fn filesystem_rejects_multiply_linked_blob_candidates() {
    let (root, store) = fixture();
    let outside = tempfile::NamedTempFile::new_in(
        root.path()
            .parent()
            .expect("the store root has a parent directory"),
    )
    .expect("the outside fixture file is created on the same filesystem");
    std::fs::write(
        outside.path(),
        signalbox_blob_store::conformance::fixture_content(),
    )
    .expect("the outside fixture contains valid blob bytes");
    std::fs::set_permissions(outside.path(), std::fs::Permissions::from_mode(0o600))
        .expect("the outside fixture is private");
    let key = BlobObjectKey::try_from_recorded("recorded-hard-link")
        .expect("the hard-link fixture key is lexically safe");
    std::fs::hard_link(outside.path(), root.path().join(key.as_str()))
        .expect("the fixture links the external inode into the store");

    let error = store
        .open(&key)
        .await
        .expect_err("a multiply linked blob candidate must not be readable");

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

#[tokio::test]
async fn filesystem_verified_stream_pins_the_verified_bytes() {
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
        .expect("the valid stream fixture is published");
    let opened = store
        .open_verified(expected, &key)
        .await
        .expect("the verified bytes are pinned before delivery");
    std::fs::write(
        root.path().join(key.as_str()),
        signalbox_blob_store::conformance::corrupt_fixture_content(),
    )
    .expect("the published inode is mutated after verification");
    let mut reader = opened.into_reader();
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .await
        .expect("the pinned verified stream remains readable");

    assert_eq!(bytes, signalbox_blob_store::conformance::fixture_content());
}

#[tokio::test]
async fn filesystem_pins_the_validated_root_namespace() {
    let (root, store) = fixture();
    let configured_root = root.path().to_path_buf();
    let moved_root = configured_root.with_extension("moved");
    std::fs::rename(&configured_root, &moved_root)
        .expect("the validated root is renamed after construction");
    std::fs::create_dir(&configured_root).expect("a replacement root is created");
    std::fs::set_permissions(&configured_root, std::fs::Permissions::from_mode(0o700))
        .expect("the replacement root is private");
    let replacement_store = FilesystemBlobStore::try_new_for_conformance(configured_root.clone())
        .expect("the replacement root is independently usable");
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
        .expect("publication stays in the validated namespace");

    assert!(moved_root.join(key.as_str()).is_file());
    assert!(!configured_root.join(key.as_str()).exists());
    drop(replacement_store);
    std::fs::remove_dir_all(&configured_root).expect("the replacement root is removed");
    std::fs::rename(&moved_root, &configured_root)
        .expect("the fixture root is restored for automatic cleanup");
}
