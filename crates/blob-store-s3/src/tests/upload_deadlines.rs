use super::*;
use bytes::Bytes;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

type UploadTask = tokio::task::JoinHandle<Result<reqwest::Response, ()>>;

async fn started_upload(
    length: u64,
) -> Result<
    (
        UploadTask,
        tokio::sync::mpsc::Sender<Bytes>,
        BufReader<tokio::net::TcpStream>,
    ),
    Box<dyn Error>,
> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = Url::parse(&format!("http://{}/", listener.local_addr()?))?;
    let store = S3BlobStore::try_new(
        endpoint.clone(),
        "fixture-region",
        BUCKET,
        PathBuf::from("/fixture/credentials"),
    )?;
    let (sender, receiver) = tokio::sync::mpsc::channel::<Bytes>(1);
    let (progress, observed) = tokio::sync::watch::channel(0_u64);
    let stream =
        futures_util::stream::unfold((receiver, progress), |(mut receiver, progress)| async {
            let bytes = receiver.recv().await?;
            progress.send_modify(|count| *count += bytes.len() as u64);
            Some((Ok::<_, std::io::Error>(bytes), (receiver, progress)))
        });
    sender.send(Bytes::from_static(b"a")).await?;
    let request = store
        .upload_client
        .put(endpoint)
        .header(reqwest::header::CONTENT_LENGTH, length)
        .body(reqwest::Body::wrap_stream(stream));
    let task = tokio::spawn(super::super::send_with_upload_idle_timeout(
        request, observed, length,
    ));
    let (socket, _) = listener.accept().await?;
    let mut reader = BufReader::new(socket);
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        if line == "\r\n" {
            break;
        }
    }
    let mut first = [0_u8; 1];
    reader.read_exact(&mut first).await?;
    assert_eq!(first, *b"a");
    Ok((task, sender, reader))
}

async fn advance_and_transfer(
    sender: &tokio::sync::mpsc::Sender<Bytes>,
    reader: &mut BufReader<tokio::net::TcpStream>,
    byte: &'static [u8; 1],
) -> Result<(), Box<dyn Error>> {
    tokio::time::advance(super::super::IDLE_TIMEOUT * 2 / 3).await;
    sender.send(Bytes::from_static(byte)).await?;
    let mut received = [0_u8; 1];
    let read = reader.read_exact(&mut received);
    tokio::pin!(read);
    // Keep the paused clock fixed while the kernel delivers this byte.
    loop {
        tokio::select! {
            biased;
            result = &mut read => { result?; break; }
            () = tokio::task::yield_now() => {}
        }
    }
    assert_eq!(&received, byte);
    Ok(())
}

#[tokio::test]
async fn an_upload_with_continuing_progress_outlives_one_idle_interval()
-> Result<(), Box<dyn Error>> {
    let (task, sender, mut reader) = started_upload(3).await?;
    tokio::time::pause();
    advance_and_transfer(&sender, &mut reader, b"b").await?;
    advance_and_transfer(&sender, &mut reader, b"c").await?;
    reader
        .get_mut()
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
        .await?;
    tokio::time::resume();
    let response = task
        .await?
        .expect("progressing upload receives its response");
    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn a_stalled_upload_expires_at_the_no_progress_bound() -> Result<(), Box<dyn Error>> {
    let (task, _sender, _reader) = started_upload(3).await?;
    tokio::time::pause();
    tokio::time::advance(super::super::IDLE_TIMEOUT).await;
    assert!(task.await?.is_err());
    Ok(())
}

#[tokio::test]
async fn a_completed_upload_bounds_the_wait_for_response_headers() -> Result<(), Box<dyn Error>> {
    let (task, _sender, _reader) = started_upload(1).await?;
    tokio::time::pause();
    tokio::time::advance(super::super::IDLE_TIMEOUT).await;
    assert!(task.await?.is_err());
    Ok(())
}

#[tokio::test]
async fn a_stalled_completion_body_has_its_own_no_progress_bound() -> Result<(), Box<dyn Error>> {
    let (task, _sender, mut reader) = started_upload(1).await?;
    reader
        .get_mut()
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\na")
        .await?;
    let response = task.await?.expect("response headers arrived");
    tokio::time::pause();
    let error = super::super::bounded_response(response, 2, "fixture completion")
        .await
        .expect_err("stalled response body is unavailable");
    assert_eq!(error.kind(), BlobStoreFailureKind::Unavailable);
    Ok(())
}
