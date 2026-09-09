//! Client input arrival and write-progress deadlines for the local process protocol.

use std::{
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, Interest, ReadBuf},
    net::{UnixStream, unix::OwnedReadHalf},
    sync::watch,
    time::{Instant, Sleep, sleep},
};

/// Retains socket readiness times across request handling and buffer refills.
pub(super) struct ArrivalReader {
    reader: Arc<OwnedReadHalf>,
    unread_since: watch::Sender<Option<UnreadInput>>,
    pub(super) received_at: Instant,
}

#[derive(Clone, Copy)]
struct UnreadInput {
    received_at: Instant,
    remaining_bytes: u64,
}

impl ArrivalReader {
    pub(super) fn new(reader: OwnedReadHalf) -> Self {
        Self {
            reader: Arc::new(reader),
            unread_since: watch::channel(None).0,
            received_at: Instant::now(),
        }
    }

    pub(super) fn get_ref(&self) -> &OwnedReadHalf {
        &self.reader
    }

    /// Poll alongside all connection work, including admission and request handling.
    pub(super) fn watch_readiness(&self) -> impl Future<Output = io::Result<()>> + use<> {
        let reader = self.reader.clone();
        let unread_since = self.unread_since.clone();
        let mut changes = unread_since.subscribe();
        async move {
            loop {
                if changes.borrow_and_update().is_some() {
                    // Do not spin on level readiness while these bytes remain unread.
                    let _ = changes.changed().await;
                    continue;
                }
                let socket = reader.as_ref().as_ref();
                let available = socket
                    .async_io(Interest::READABLE, || peek_input(socket))
                    .await?;
                if available == 0 {
                    // A write-half close does not end a follow stream or pending receipt.
                    return std::future::pending().await;
                }
                record_input_arrival(socket, &unread_since)?;
            }
        }
    }

    fn try_read_batch(
        &mut self,
        capacity: usize,
        read: impl FnOnce(&UnixStream, usize) -> io::Result<usize>,
    ) -> io::Result<usize> {
        let socket = self.reader.as_ref().as_ref();
        let mut result = Ok(0);
        self.unread_since.send_modify(|input| {
            result = (|| {
                if input.is_none() {
                    *input = queued_input(socket)?;
                }
                let received_at = input.map_or_else(Instant::now, |input| input.received_at);
                let limit = input.map_or(capacity, |input| {
                    capacity.min(usize::try_from(input.remaining_bytes).unwrap_or(usize::MAX))
                });
                match read(socket, limit) {
                    Ok(read) => {
                        if read > 0 {
                            self.received_at = received_at;
                        }
                        if let Some(batch) = input {
                            batch.remaining_bytes -= read as u64;
                            if read == 0 || batch.remaining_bytes == 0 {
                                *input = None;
                            }
                        }
                        Ok(read)
                    }
                    Err(error) => {
                        if error.kind() == io::ErrorKind::WouldBlock {
                            *input = None;
                        }
                        Err(error)
                    }
                }
            })();
        });
        result
    }
}

fn queued_input(socket: &UnixStream) -> io::Result<Option<UnreadInput>> {
    let remaining_bytes = rustix::io::ioctl_fionread(socket)?;
    Ok((remaining_bytes > 0).then(|| UnreadInput {
        received_at: Instant::now(),
        remaining_bytes,
    }))
}

fn record_input_arrival(
    socket: &UnixStream,
    unread_since: &watch::Sender<Option<UnreadInput>>,
) -> io::Result<()> {
    let mut result = Ok(());
    unread_since.send_if_modified(|since| {
        if since.is_some() {
            return false;
        }
        // Snapshot the byte count under the same lock used to consume this batch.
        match queued_input(socket) {
            Ok(input) => {
                *since = input;
                since.is_some()
            }
            Err(error) => {
                result = Err(error);
                false
            }
        }
    });
    result
}

fn peek_input(socket: &UnixStream) -> io::Result<usize> {
    use rustix::net::{RecvFlags, recv};
    recv(
        socket,
        &mut [0_u8; 1],
        RecvFlags::PEEK | RecvFlags::DONTWAIT,
    )
    .map(|(read, _)| read)
    .map_err(Into::into)
}

impl AsyncRead for ArrivalReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            std::task::ready!(this.reader.as_ref().as_ref().poll_read_ready(cx))?;
            match this.try_read_batch(buffer.remaining(), |socket, limit| {
                socket.try_read(buffer.initialize_unfilled_to(limit))
            }) {
                Ok(read) => {
                    buffer.advance(read);
                    return Poll::Ready(Ok(()));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
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
    use crate::process_runtime::{IncomingLine, connection::read_admitted_frame};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, duplex};
    use tokio::sync::Semaphore;
    use tokio::time::timeout;

    #[tokio::test(start_paused = true)]
    async fn input_arriving_after_a_read_does_not_inherit_the_drained_batch_deadline() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (server, _writer) = server.into_split();
        let mut reader = ArrivalReader::new(server);
        let observer = reader.reader.clone();
        let socket = observer.as_ref().as_ref();
        client.write_all(b"b").await.unwrap();
        socket.readable().await.unwrap();
        record_input_arrival(socket, &reader.unread_since).unwrap();
        sleep(Duration::from_millis(900)).await;

        let mut consumed = [0; 1];
        reader
            .try_read_batch(consumed.len(), |socket, limit| {
                let read = socket.try_read(&mut consumed[..limit])?;
                // C arrives after B drains, before the reader updates its marker.
                assert_eq!(client.try_write(b"{}\n")?, 3);
                Ok(read)
            })
            .unwrap();
        assert_eq!(&consumed, b"b");
        record_input_arrival(socket, &reader.unread_since).unwrap();
        sleep(Duration::from_millis(200)).await;

        let mut reader = BufReader::new(reader);
        let (_shutdown, mut shutdown) = watch::channel(false);
        let (_, frame) = read_admitted_frame(
            &mut reader,
            Arc::new(Semaphore::new(1)),
            &mut shutdown,
            Some(Duration::from_secs(1)),
        )
        .await
        .expect("C has its own deadline despite B's expired arrival timestamp")
        .unwrap();
        assert!(matches!(frame, IncomingLine::Complete(bytes) if bytes == b"{}\n"));
    }

    #[tokio::test(start_paused = true)]
    async fn drained_peek_does_not_age_input_arriving_after_an_idle_gap() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (server, _writer) = server.into_split();
        let mut reader = ArrivalReader::new(server);
        let observer = reader.reader.clone();
        let socket = observer.as_ref().as_ref();
        client.write_all(b"a").await.unwrap();
        assert_eq!(
            socket
                .async_io(Interest::READABLE, || peek_input(socket))
                .await
                .unwrap(),
            1
        );

        // The read wins between the watcher's successful peek and marker publication.
        assert_eq!(reader.read_u8().await.unwrap(), b'a');
        record_input_arrival(socket, &reader.unread_since).unwrap();
        sleep(Duration::from_secs(60)).await;
        client.write_all(b"b").await.unwrap();
        assert_eq!(reader.read_u8().await.unwrap(), b'b');
        assert_eq!(reader.received_at, Instant::now());
    }

    #[tokio::test(start_paused = true)]
    async fn socket_arrival_survives_short_reads_until_the_receive_buffer_drains() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (server, _writer) = server.into_split();
        let mut reader = ArrivalReader::new(server);
        let readiness = reader.watch_readiness();
        tokio::select! {
            biased;
            result = readiness => panic!("readiness watcher ended: {result:?}"),
            () = async {
                client.write_all(b"ab").await.unwrap();
                assert_eq!(reader.read_u8().await.unwrap(), b'a');
                let first_arrival = reader.received_at;
                sleep(Duration::from_secs(2)).await;
                assert_eq!(reader.read_u8().await.unwrap(), b'b');
                assert_eq!(reader.received_at, first_arrival);

                sleep(Duration::from_secs(60)).await;
                client.write_all(b"c").await.unwrap();
                assert_eq!(reader.read_u8().await.unwrap(), b'c');
                assert_eq!(reader.received_at, Instant::now());
            } => {}
        }
    }

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
