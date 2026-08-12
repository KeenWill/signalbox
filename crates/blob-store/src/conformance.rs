//! Reusable behavior checks for every blob-store adapter.

#![allow(
    clippy::expect_used,
    reason = "shared conformance assertions use fixed non-secret test fixtures"
)]

use std::num::NonZeroU64;

use signalbox_domain::BlobDigest;
use tokio::io::AsyncReadExt;

use crate::{
    BlobObjectKey, BlobPutOutcome, BlobReader, BlobStore, BlobStoreFailureKind, ExpectedBlob,
    MAX_BLOB_RANGE_BYTES,
};

const FIRST_CONTENT: &[u8] = b"shared blob-store conformance fixture";
const SAME_LENGTH_DIFFERENT_CONTENT: &[u8] = b"shared blob-store conformance fixturf";
const RANGE_OFFSET: usize = 7;
const RANGE_LENGTH: usize = 10;
const FIRST_CONTENT_LENGTH: NonZeroU64 = match NonZeroU64::new(FIRST_CONTENT.len() as u64) {
    Some(length) => length,
    None => NonZeroU64::MIN,
};

fn reader(bytes: &[u8]) -> BlobReader {
    Box::new(std::io::Cursor::new(bytes.to_vec()))
}

/// Returns the shared valid publication fixture.
pub fn fixture_content() -> &'static [u8] {
    FIRST_CONTENT
}

/// Returns bytes that do not match the shared expected fixture.
pub fn corrupt_fixture_content() -> &'static [u8] {
    SAME_LENGTH_DIFFERENT_CONTENT
}

/// Returns the shared expected fixture identity.
pub fn expected_fixture() -> ExpectedBlob {
    ExpectedBlob::new(BlobDigest::digest(FIRST_CONTENT), FIRST_CONTENT_LENGTH)
}

fn expected() -> ExpectedBlob {
    expected_fixture()
}

/// Proves new publication and exact-byte streaming read-back.
pub async fn assert_put_and_exact_read_back(store: &dyn BlobStore) {
    let expected = expected();
    let outcome = store
        .put(expected, reader(FIRST_CONTENT))
        .await
        .expect("the conformance store publishes valid fixture bytes");
    assert_eq!(
        outcome,
        BlobPutOutcome::Published {
            key: BlobObjectKey::for_digest(expected.digest())
        }
    );

    let opened = store
        .open(outcome.key())
        .await
        .expect("the published conformance object opens");
    assert_eq!(opened.byte_length(), expected.byte_length());
    let mut actual = Vec::new();
    opened
        .into_reader()
        .read_to_end(&mut actual)
        .await
        .expect("the bounded conformance fixture reads completely");
    assert_eq!(actual, FIRST_CONTENT);
}

/// Proves an exact bounded range reads without materializing the whole object.
pub async fn assert_exact_range_read_back(store: &dyn BlobStore) {
    let expected = expected();
    let outcome = store
        .put(expected, reader(FIRST_CONTENT))
        .await
        .expect("the conformance store publishes valid fixture bytes");
    let offset = u64::try_from(RANGE_OFFSET).expect("the fixture offset fits u64");
    let byte_length =
        NonZeroU64::new(u64::try_from(RANGE_LENGTH).expect("the fixture range length fits u64"))
            .expect("the fixture range is nonempty");
    let opened = store
        .open_range(expected, outcome.key(), offset, byte_length)
        .await
        .expect("the published conformance range opens");
    assert_eq!(opened.byte_length(), byte_length.get());
    let mut actual = Vec::new();
    opened
        .into_reader()
        .read_to_end(&mut actual)
        .await
        .expect("the bounded conformance range reads completely");
    assert_eq!(
        actual,
        FIRST_CONTENT[RANGE_OFFSET..RANGE_OFFSET + RANGE_LENGTH]
    );
}

/// Proves an adapter rejects a range larger than its named memory bound.
pub async fn assert_oversized_range_is_rejected(store: &dyn BlobStore) {
    let object_length = MAX_BLOB_RANGE_BYTES + 2;
    let content = vec![
        b'x';
        usize::try_from(object_length)
            .expect("the conformance object length fits usize")
    ];
    let expected = ExpectedBlob::try_new(BlobDigest::digest(&content), object_length)
        .expect("the oversized-range fixture is nonempty");
    let outcome = store
        .put(expected, reader(&content))
        .await
        .expect("the conformance store publishes valid fixture bytes");
    let oversized = NonZeroU64::new(MAX_BLOB_RANGE_BYTES + 1)
        .expect("one beyond the positive range bound remains nonzero");

    let error = store
        .open_range(expected, outcome.key(), 1, oversized)
        .await
        .expect_err("an oversized adapter range must be rejected before allocation");

    assert_eq!(error.kind(), BlobStoreFailureKind::Unavailable);
}

/// Proves repeated publication verifies and deduplicates the final destination.
pub async fn assert_existing_destination_deduplicates(store: &dyn BlobStore) {
    let expected = expected();
    store
        .put(expected, reader(FIRST_CONTENT))
        .await
        .expect("the first conformance publication succeeds");

    let repeated = store
        .put(expected, reader(FIRST_CONTENT))
        .await
        .expect("the repeated conformance publication verifies");

    assert_eq!(
        repeated,
        BlobPutOutcome::AlreadyPresent {
            key: BlobObjectKey::for_digest(expected.digest())
        }
    );
}

/// Proves concurrent publication creates once and verifies the winning object.
pub async fn assert_concurrent_publication_deduplicates(store: &dyn BlobStore) {
    let expected = expected();
    let (first, second) = tokio::join!(
        store.put(expected, reader(FIRST_CONTENT)),
        store.put(expected, reader(FIRST_CONTENT)),
    );
    let first = first.expect("the first concurrent publication succeeds");
    let second = second.expect("the second concurrent publication succeeds");
    let published = BlobPutOutcome::Published {
        key: BlobObjectKey::for_digest(expected.digest()),
    };
    let already_present = BlobPutOutcome::AlreadyPresent {
        key: BlobObjectKey::for_digest(expected.digest()),
    };

    assert!(
        (first == published && second == already_present)
            || (first == already_present && second == published)
    );
}

/// Proves publication refuses bytes that do not match their declared identity.
pub async fn assert_verification_failure(store: &dyn BlobStore) {
    assert_eq!(SAME_LENGTH_DIFFERENT_CONTENT.len(), FIRST_CONTENT.len());
    let error = store
        .put(expected(), reader(SAME_LENGTH_DIFFERENT_CONTENT))
        .await
        .expect_err("mismatching conformance bytes must fail verification");

    assert_eq!(error.kind(), BlobStoreFailureKind::VerificationFailed);

    let missing = store
        .open(&BlobObjectKey::for_digest(expected().digest()))
        .await
        .expect_err("failed verification must not leave a final object");
    assert_eq!(missing.kind(), BlobStoreFailureKind::NotFound);
}

/// Proves a valid re-ingest atomically repairs an injected corrupt destination.
pub async fn assert_corrupt_destination_is_repaired(store: &dyn BlobStore) {
    let expected = expected();
    let outcome = store
        .put(expected, reader(FIRST_CONTENT))
        .await
        .expect("the valid fixture repairs the corrupt destination");
    assert_eq!(
        outcome,
        BlobPutOutcome::Repaired {
            key: BlobObjectKey::for_digest(expected.digest())
        }
    );
    let opened = store
        .open(outcome.key())
        .await
        .expect("the repaired conformance object opens");
    let mut actual = Vec::new();
    opened
        .into_reader()
        .read_to_end(&mut actual)
        .await
        .expect("the repaired conformance object reads completely");
    assert_eq!(actual, FIRST_CONTENT);
}
