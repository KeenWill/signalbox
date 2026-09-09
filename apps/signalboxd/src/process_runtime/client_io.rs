//! Client input arrival and write-progress deadlines for the local process protocol.

use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    time::{Instant, Sleep, sleep},
};

/// Records when a read-ahead buffer receives bytes, including pipelined frames.
pub(super) struct ArrivalReader<Reader> {
    reader: Reader,
    pub(super) received_at: Instant,
}

impl<Reader> ArrivalReader<Reader> {
    pub(super) fn new(reader: Reader) -> Self {
        Self {
            reader,
            received_at: Instant::now(),
        }
    }
}

impl<Reader: AsyncRead + Unpin> AsyncRead for ArrivalReader<Reader> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let filled = buffer.filled().len();
        let result = Pin::new(&mut this.reader).poll_read(cx, buffer);
        if matches!(result, Poll::Ready(Ok(()))) && buffer.filled().len() > filled {
            this.received_at = Instant::now();
        }
        result
    }
}

/// Bounds a blocked socket write without timing the idle gaps between writes.
pub(super) struct ProgressWriter<Writer> {
    writer: Writer,
    bound: Option<Duration>,
    stalled: Option<Pin<Box<Sleep>>>,
}

impl<Writer> ProgressWriter<Writer> {
    pub(super) fn new(writer: Writer, bound: Option<Duration>) -> Self {
        Self {
            writer,
            bound,
            stalled: None,
        }
    }
}

impl<Writer: AsyncWrite + Unpin> AsyncWrite for ProgressWriter<Writer> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Some(timer) = &mut this.stalled
            && timer.as_mut().poll(cx).is_ready()
        {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "client write progress deadline elapsed",
            )));
        }
        match Pin::new(&mut this.writer).poll_write(cx, buffer) {
            Poll::Ready(result) => {
                this.stalled = None;
                Poll::Ready(result)
            }
            Poll::Pending => {
                if let Some(bound) = this.bound {
                    let timer = this.stalled.get_or_insert_with(|| Box::pin(sleep(bound)));
                    if timer.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "client write progress deadline elapsed",
                        )));
                    }
                }
                Poll::Pending
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
    use tokio::time::timeout;

    #[tokio::test(start_paused = true)]
    async fn stalled_reader_expires_the_write_progress_deadline() {
        let (server, _client) = duplex(1);
        let bound = Duration::from_secs(1);
        let mut writer = ProgressWriter::new(server, Some(bound));
        let started = tokio::time::Instant::now();
        let error = writer
            .write_all(b"ab")
            .await
            .expect_err("reader is stalled");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(started.elapsed(), bound);
    }

    #[tokio::test(start_paused = true)]
    async fn late_write_progress_does_not_revive_an_expired_write() {
        let (server, mut client) = duplex(1);
        let mut writer = ProgressWriter::new(server, Some(Duration::from_secs(1)));
        let writing = writer.write_all(b"ab");
        tokio::pin!(writing);
        assert!(
            timeout(Duration::from_millis(500), &mut writing)
                .await
                .is_err()
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(client.read_u8().await.unwrap(), b'a');
        assert_eq!(writing.await.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn write_progress_resets_the_deadline() {
        let (server, mut client) = duplex(1);
        let bound = Duration::from_secs(1);
        let mut writer = ProgressWriter::new(server, Some(bound));
        let writing = writer.write_all(b"abcd");
        tokio::pin!(writing);
        for expected in b"abc" {
            assert!(
                timeout(Duration::from_millis(750), &mut writing)
                    .await
                    .is_err()
            );
            assert_eq!(client.read_u8().await.unwrap(), *expected);
        }
        writing.await.expect("each write made timely progress");
        assert_eq!(client.read_u8().await.unwrap(), b'd');
    }

    #[tokio::test(start_paused = true)]
    async fn follow_output_survives_idle_gaps_between_events() {
        let (server, mut client) = duplex(32);
        let mut writer = ProgressWriter::new(server, Some(Duration::from_secs(1)));
        writer.write_all(b"snapshot\n").await.unwrap();
        tokio::time::advance(Duration::from_secs(60)).await;
        writer.write_all(b"event\n").await.unwrap();
        let mut received = [0; 15];
        client.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"snapshot\nevent\n");
    }
}
