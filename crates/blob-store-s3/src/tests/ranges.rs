use super::*;
use signalbox_blob_store::{BlobStoreError, OpenedBlob};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn a_large_s3_blob_uses_one_range_request_for_its_short_tail() -> Result<(), Box<dyn Error>> {
    let opened = read_tail(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 10737418237-10737418239/10737418240\r\nConnection: close\r\n\r\nend").await?;
    assert_eq!(opened.byte_length(), 3);
    let mut bytes = Vec::new();
    opened.into_reader().read_to_end(&mut bytes).await?;
    assert_eq!(bytes, b"end");
    Ok(())
}

#[tokio::test]
async fn an_unsatisfied_range_proves_a_truncated_s3_replica() -> Result<(), Box<dyn Error>> {
    let error = read_tail(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nContent-Range: bytes */3\r\nConnection: close\r\n\r\n").await.expect_err("truncated replica");
    let error = error
        .downcast_ref::<BlobStoreError>()
        .expect("store failure");
    assert_eq!(error.kind(), BlobStoreFailureKind::VerificationFailed);
    assert_eq!(
        error
            .verification_failure()
            .expect("length mismatch")
            .observed_length(),
        3
    );
    Ok(())
}

#[tokio::test]
async fn a_shortened_partial_range_proves_a_truncated_s3_replica() -> Result<(), Box<dyn Error>> {
    let error = read_tail(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nContent-Range: bytes 10737418237-10737418238/10737418239\r\nConnection: close\r\n\r\nen").await.expect_err("truncated replica");
    let error = error
        .downcast_ref::<BlobStoreError>()
        .expect("store failure");
    assert_eq!(error.kind(), BlobStoreFailureKind::VerificationFailed);
    assert_eq!(
        error
            .verification_failure()
            .expect("length mismatch")
            .observed_length(),
        10 * 1024 * 1024 * 1024 - 1
    );
    Ok(())
}

#[tokio::test]
async fn an_s3_server_error_does_not_prove_a_length_mismatch() -> Result<(), Box<dyn Error>> {
    let error = read_tail(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nContent-Range: bytes */3\r\nConnection: close\r\n\r\n").await.expect_err("server error");
    let error = error
        .downcast_ref::<BlobStoreError>()
        .expect("store failure");
    assert_eq!(error.kind(), BlobStoreFailureKind::Unavailable);
    Ok(())
}

async fn read_tail(response: &[u8]) -> Result<OpenedBlob, Box<dyn Error>> {
    const BLOB_LENGTH: u64 = 10 * 1024 * 1024 * 1024;
    let (_directory, credentials) = credential_fixture(&credential_body())?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let store = S3BlobStore::try_new(
        Url::parse(&format!("http://{}/", listener.local_addr()?))?,
        "fixture-region",
        BUCKET,
        credentials,
    )?;
    let expected =
        ExpectedBlob::try_new(BlobDigest::digest(b"range fixture catalog"), BLOB_LENGTH)?;
    let key = signalbox_blob_store::BlobObjectKey::for_digest(expected.digest());
    let serve = async {
        let (socket, _) = listener.accept().await?;
        let mut reader = BufReader::new(socket);
        let mut headers = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            if line == "\r\n" {
                break;
            }
            headers.push_str(&line);
        }
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("range: bytes=10737418237-10737418239\r\n")
        );
        reader.get_mut().write_all(response).await?;
        Ok::<(), std::io::Error>(())
    };
    let read = store.open_range_inner(&key, expected, BLOB_LENGTH - 3, 524_288);
    let (served, opened) = tokio::join!(serve, read);
    served?;
    Ok(opened?)
}
