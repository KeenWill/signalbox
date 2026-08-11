//! Shared blob-store contract applied to the filesystem adapter.

#![allow(
    clippy::expect_used,
    reason = "filesystem conformance tests use explicit fixture expectations"
)]

use signalbox_blob_store::{BlobObjectKey, BlobStore, BlobStoreFailureKind, ExpectedBlob};
use signalbox_blob_store_filesystem::FilesystemBlobStore;
use signalbox_domain::BlobDigest;
use tempfile::TempDir;

fn fixture() -> (TempDir, FilesystemBlobStore) {
    let root = TempDir::new().expect("the fixture creates a temporary store root");
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
async fn inv059_filesystem_deduplicates_an_existing_destination() {
    let (_root, store) = fixture();

    signalbox_blob_store::conformance::assert_existing_destination_deduplicates(&store).await;
}

#[tokio::test]
async fn inv059_filesystem_rejects_publication_verification_failure() {
    let (_root, store) = fixture();

    signalbox_blob_store::conformance::assert_verification_failure(&store).await;
}

#[tokio::test]
async fn inv059_filesystem_rejects_a_corrupt_existing_destination() {
    const EXPECTED_CONTENT: &[u8] = b"expected content";
    const CORRUPT_CONTENT: &[u8] = b"corrupt content!";
    let (root, store) = fixture();
    let expected = ExpectedBlob::try_new(
        BlobDigest::digest(EXPECTED_CONTENT),
        u64::try_from(EXPECTED_CONTENT.len()).expect("the fixture length fits u64"),
    )
    .expect("the fixture is nonempty");
    let key = BlobObjectKey::for_digest(expected.digest());
    let destination = root.path().join(key.as_str());
    std::fs::create_dir_all(
        destination
            .parent()
            .expect("the deterministic key has a parent"),
    )
    .expect("the fixture creates the destination parent");
    std::fs::write(&destination, CORRUPT_CONTENT)
        .expect("the fixture injects a corrupt destination");

    let error = store
        .put(
            expected,
            Box::new(std::io::Cursor::new(EXPECTED_CONTENT.to_vec())),
        )
        .await
        .expect_err("a corrupt existing destination must fail verification");

    assert_eq!(error.kind(), BlobStoreFailureKind::VerificationFailed);
}
