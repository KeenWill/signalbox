use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn a_large_s3_blob_uses_one_range_request_for_its_short_tail() -> Result<(), Box<dyn Error>> {
    const BLOB_LENGTH: u64 = 20 * 1024 * 1024 * 1024;
    let (_directory, credentials) = credential_fixture(&credential_body())?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let store = S3BlobStore::try_new(
        Url::parse(&format!("http://{}/", listener.local_addr()?))?,
        "fixture-region", BUCKET, credentials,
    )?;
    let expected = ExpectedBlob::try_new(BlobDigest::digest(b"range fixture catalog"), BLOB_LENGTH)?;
    let key = signalbox_blob_store::BlobObjectKey::for_digest(expected.digest());
    let serve = async {
        let (socket, _) = listener.accept().await?;
        let mut reader = BufReader::new(socket);
        let mut headers = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            if line == "\r\n" { break; }
            headers.push_str(&line);
        }
        assert!(headers.to_ascii_lowercase().contains("range: bytes=21474836477-21474836479\r\n"));
        reader.get_mut().write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 21474836477-21474836479/21474836480\r\nConnection: close\r\n\r\nend").await?;
        Ok::<(), std::io::Error>(())
    };
    let read = store.open_range_inner(&key, expected, BLOB_LENGTH - 3, 524_288);
    let (served, opened) = tokio::join!(serve, read);
    served?;
    let opened = opened?;
    assert_eq!(opened.byte_length(), 3);
    let mut bytes = Vec::new();
    opened.into_reader().read_to_end(&mut bytes).await?;
    assert_eq!(bytes, b"end");
    Ok(())
}
