//! Opt-in live S3 conformance coverage.
//!
//! No local S3 stand-in is part of the repository environment. The ignored
//! suite therefore requires explicit `SIGNALBOX_S3_TEST_*` configuration and
//! exercises the same adapter assertions as the filesystem store.

use std::{env, error::Error, io, path::PathBuf};

use signalbox_blob_store::{
    BlobObjectKey, BlobStore, BlobStoreFailureKind,
    conformance::{
        assert_concurrent_publication_deduplicates, assert_corrupt_destination_is_repaired,
        assert_exact_range_read_back, assert_existing_destination_deduplicates,
        assert_oversized_range_is_rejected, assert_put_and_exact_read_back,
        assert_verification_failure, corrupt_fixture_content, expected_fixture, fixture_content,
    },
};
use signalbox_blob_store_s3::S3BlobStore;
use tokio::{io::AsyncReadExt, sync::Mutex};
use url::Url;

static LIVE_FIXTURE: Mutex<()> = Mutex::const_new(());

/// Bytes that share no prefix bound with the shared fixture's declared length.
const OVERLONG_FIXTURE_CONTENT: &[u8] =
    b"shared blob-store conformance fixture with unexpected trailing bytes";

fn required(name: &'static str) -> Result<String, io::Error> {
    env::var(name).map_err(|_| io::Error::new(io::ErrorKind::NotFound, name))
}

fn live_store() -> Result<S3BlobStore, Box<dyn Error>> {
    let endpoint = Url::parse(&required("SIGNALBOX_S3_TEST_ENDPOINT")?)?;
    let region = required("SIGNALBOX_S3_TEST_REGION")?;
    let bucket = required("SIGNALBOX_S3_TEST_BUCKET")?;
    let credentials_file = PathBuf::from(required("SIGNALBOX_S3_TEST_CREDENTIALS_FILE")?);
    Ok(S3BlobStore::try_new(
        endpoint,
        region,
        bucket,
        credentials_file,
    )?)
}

#[tokio::test]
#[ignore = "requires an explicit live S3-compatible test bucket"]
async fn live_s3_rejects_publication_verification_failure() -> Result<(), Box<dyn Error>> {
    let _fixture = LIVE_FIXTURE.lock().await;
    let store = live_store()?;
    let expected = expected_fixture();

    store.delete_for_conformance(expected).await?;
    assert_verification_failure(&store).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit live S3-compatible test bucket"]
async fn live_s3_puts_and_reads_exact_bytes() -> Result<(), Box<dyn Error>> {
    let _fixture = LIVE_FIXTURE.lock().await;
    let store = live_store()?;
    let expected = expected_fixture();

    store.delete_for_conformance(expected).await?;
    assert_put_and_exact_read_back(&store).await;
    store.delete_for_conformance(expected).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit live S3-compatible test bucket"]
async fn live_s3_reads_exact_bounded_ranges() -> Result<(), Box<dyn Error>> {
    let _fixture = LIVE_FIXTURE.lock().await;
    let store = live_store()?;
    let expected = expected_fixture();

    store.delete_for_conformance(expected).await?;
    assert_exact_range_read_back(&store).await;
    store.delete_for_conformance(expected).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit live S3-compatible test bucket"]
async fn live_s3_rejects_oversized_ranges() -> Result<(), Box<dyn Error>> {
    let _fixture = LIVE_FIXTURE.lock().await;
    let store = live_store()?;
    let expected = expected_fixture();

    store.delete_for_conformance(expected).await?;
    assert_oversized_range_is_rejected(&store).await;
    store.delete_for_conformance(expected).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit live S3-compatible test bucket"]
async fn live_s3_deduplicates_an_existing_destination() -> Result<(), Box<dyn Error>> {
    let _fixture = LIVE_FIXTURE.lock().await;
    let store = live_store()?;
    let expected = expected_fixture();

    store.delete_for_conformance(expected).await?;
    assert_existing_destination_deduplicates(&store).await;
    store.delete_for_conformance(expected).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit live S3-compatible test bucket"]
async fn live_s3_concurrent_publication_is_no_clobber() -> Result<(), Box<dyn Error>> {
    let _fixture = LIVE_FIXTURE.lock().await;
    let store = live_store()?;
    let expected = expected_fixture();

    store.delete_for_conformance(expected).await?;
    assert_concurrent_publication_deduplicates(&store).await;
    store.delete_for_conformance(expected).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit live S3-compatible test bucket"]
async fn live_s3_repairs_a_corrupt_existing_destination() -> Result<(), Box<dyn Error>> {
    let _fixture = LIVE_FIXTURE.lock().await;
    let store = live_store()?;
    let expected = expected_fixture();

    store.delete_for_conformance(expected).await?;
    store
        .corrupt_for_conformance(expected, corrupt_fixture_content())
        .await?;
    assert_corrupt_destination_is_repaired(&store).await;

    store.delete_for_conformance(expected).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit live S3-compatible test bucket"]
async fn live_s3_verified_stream_returns_the_verified_generation() -> Result<(), Box<dyn Error>> {
    let _fixture = LIVE_FIXTURE.lock().await;
    let store = live_store()?;
    let expected = expected_fixture();
    let key = BlobObjectKey::for_digest(expected.digest());

    store.delete_for_conformance(expected).await?;
    assert_put_and_exact_read_back(&store).await;
    let opened = store.open_verified(expected, &key).await?;
    assert_eq!(opened.byte_length(), expected.byte_length());
    let mut actual = Vec::new();
    opened.into_reader().read_to_end(&mut actual).await?;

    assert_eq!(actual, fixture_content());
    store.delete_for_conformance(expected).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit live S3-compatible test bucket"]
async fn live_s3_verified_stream_rejects_a_digest_mismatch() -> Result<(), Box<dyn Error>> {
    let _fixture = LIVE_FIXTURE.lock().await;
    let store = live_store()?;
    let expected = expected_fixture();
    let key = BlobObjectKey::for_digest(expected.digest());

    store.delete_for_conformance(expected).await?;
    store
        .corrupt_for_conformance(expected, corrupt_fixture_content())
        .await?;
    let error = store
        .open_verified(expected, &key)
        .await
        .expect_err("a same-length digest mismatch must never be delivered");

    assert_eq!(error.kind(), BlobStoreFailureKind::VerificationFailed);
    store.delete_for_conformance(expected).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit live S3-compatible test bucket"]
async fn live_s3_verified_stream_rejects_an_overlong_generation() -> Result<(), Box<dyn Error>> {
    let _fixture = LIVE_FIXTURE.lock().await;
    let store = live_store()?;
    let expected = expected_fixture();
    let key = BlobObjectKey::for_digest(expected.digest());
    assert!(OVERLONG_FIXTURE_CONTENT.len() > fixture_content().len());

    store.delete_for_conformance(expected).await?;
    store
        .corrupt_for_conformance(expected, OVERLONG_FIXTURE_CONTENT)
        .await?;
    let error = store
        .open_verified(expected, &key)
        .await
        .expect_err("a generation longer than its declared length must never be delivered");

    assert_eq!(error.kind(), BlobStoreFailureKind::VerificationFailed);
    store.delete_for_conformance(expected).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit live S3-compatible test bucket"]
async fn live_s3_verified_stream_reports_a_proved_absence() -> Result<(), Box<dyn Error>> {
    let _fixture = LIVE_FIXTURE.lock().await;
    let store = live_store()?;
    let expected = expected_fixture();
    let key = BlobObjectKey::for_digest(expected.digest());

    store.delete_for_conformance(expected).await?;
    let error = store
        .open_verified(expected, &key)
        .await
        .expect_err("an absent object cannot be verified");

    assert_eq!(error.kind(), BlobStoreFailureKind::NotFound);
    Ok(())
}
