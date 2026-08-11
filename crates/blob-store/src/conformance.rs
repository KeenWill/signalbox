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
};

const FIRST_CONTENT: &[u8] = b"shared blob-store conformance fixture";
const DIFFERENT_CONTENT: &[u8] = b"different bytes";
const FIRST_CONTENT_LENGTH: NonZeroU64 = match NonZeroU64::new(FIRST_CONTENT.len() as u64) {
    Some(length) => length,
    None => NonZeroU64::MIN,
};

fn reader(bytes: &[u8]) -> BlobReader {
    Box::new(std::io::Cursor::new(bytes.to_vec()))
}

fn expected() -> ExpectedBlob {
    ExpectedBlob::new(BlobDigest::digest(FIRST_CONTENT), FIRST_CONTENT_LENGTH)
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
    let error = store
        .put(expected(), reader(DIFFERENT_CONTENT))
        .await
        .expect_err("mismatching conformance bytes must fail verification");

    assert_eq!(error.kind(), BlobStoreFailureKind::VerificationFailed);

    let missing = store
        .open(&BlobObjectKey::for_digest(expected().digest()))
        .await
        .expect_err("failed verification must not leave a final object");
    assert_eq!(missing.kind(), BlobStoreFailureKind::NotFound);
}
