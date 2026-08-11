//! Shared blob-store contract applied to the filesystem adapter.

#![allow(
    clippy::expect_used,
    reason = "filesystem conformance tests use explicit fixture expectations"
)]

use signalbox_blob_store::BlobObjectKey;
use signalbox_blob_store_filesystem::FilesystemBlobStore;
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
    std::fs::create_dir_all(
        destination
            .parent()
            .expect("the deterministic key has a parent"),
    )
    .expect("the fixture creates the destination parent");
    std::fs::write(
        &destination,
        signalbox_blob_store::conformance::corrupt_fixture_content(),
    )
    .expect("the fixture injects a corrupt destination");

    signalbox_blob_store::conformance::assert_corrupt_destination_is_repaired(&store).await;
}
