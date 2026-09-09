//! Client input arrival and write-progress deadlines for the local process protocol.

use futures_util::task::AtomicWaker;
use std::{
    collections::VecDeque,
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncRead, AsyncWrite, ReadBuf},
    net::{UnixStream, unix::OwnedReadHalf},
    sync::watch,
    time::{Instant, Sleep, sleep},
};

use super::INBOUND_READ_AHEAD_BYTES;

struct InputChunk {
    bytes: Box<[u8]>,
    received_at: Instant,
}

#[derive(Default)]
struct ReadAhead {
    chunks: VecDeque<Arc<InputChunk>>,
    bytes: usize,
    eof: bool,
}

/// Shares bounded, timestamped read-ahead with the connection's input collector.
pub(super) struct ArrivalReader {
    reader: Arc<OwnedReadHalf>,
    input: watch::Sender<ReadAhead>,
    readable: Arc<AtomicWaker>,
    current: Option<Arc<InputChunk>>,
    offset: usize,
    pub(super) received_at: Instant,
}

impl ArrivalReader {
    pub(super) fn new(reader: OwnedReadHalf) -> Self {
        Self {
            reader: Arc::new(reader),
            input: watch::channel(ReadAhead::default()).0,
            readable: Arc::new(AtomicWaker::new()),
            current: None,
            offset: 0,
            received_at: Instant::now(),
        }
    }

    pub(super) fn get_ref(&self) -> &OwnedReadHalf {
        &self.reader
    }

    pub(super) fn buffer(&self) -> &[u8] {
        self.current
            .as_ref()
            .map_or(&[], |chunk| &chunk.bytes[self.offset..])
    }

    /// Collect while requests execute; buffered admission waits keep their deadlines.
    pub(super) fn collect_input(
        &self,
        bound: Option<Duration>,
    ) -> impl Future<Output = io::Result<()>> + use<> {
        let reader = self.reader.clone();
        let input = self.input.clone();
        let readable = self.readable.clone();
        let mut changes = input.subscribe();
        async move {
            loop {
                let (can_read, deadline) = {
                    let queued = changes.borrow_and_update();
                    (
                        !queued.eof && queued.bytes < INBOUND_READ_AHEAD_BYTES,
                        bound.and_then(|bound| {
                            queued.chunks.front().map(|chunk| chunk.received_at + bound)
                        }),
                    )
                };
                tokio::select! {
                    biased;
                    _ = changes.changed() => {}
                    () = super::wait_for_deadline(deadline) => {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "client frame deadline elapsed"));
                    }
                    ready = reader.readable(), if can_read => {
                        ready?;
                        match collect_ready_input(reader.as_ref().as_ref(), &input, &readable) {
                            Ok(()) => {}
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                            Err(error) => return Err(error),
                        }
                    }
                }
            }
        }
    }
}

fn collect_ready_input(
    socket: &UnixStream,
    input: &watch::Sender<ReadAhead>,
    readable: &AtomicWaker,
) -> io::Result<()> {
    let mut result = Ok(());
    let changed = input.send_if_modified(|queued| {
        if queued.eof || queued.bytes == INBOUND_READ_AHEAD_BYTES {
            return false;
        }
        let mut bytes = vec![0; INBOUND_READ_AHEAD_BYTES - queued.bytes];
        match socket.try_read(&mut bytes) {
            Ok(0) => queued.eof = true,
            Ok(read) => {
                bytes.truncate(read);
                queued.bytes += read;
                queued.chunks.push_back(Arc::new(InputChunk {
                    bytes: bytes.into_boxed_slice(),
                    received_at: Instant::now(),
                }));
            }
            Err(error) => {
                result = Err(error);
                return false;
            }
        }
        true
    });
    if changed {
        readable.wake();
    }
    result
}

impl AsyncBufRead for ArrivalReader {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        while this.current.is_none() {
            this.readable.register(cx.waker());
            let (front, eof) = {
                let queued = this.input.borrow();
                (queued.chunks.front().cloned(), queued.eof)
            };
            if let Some(chunk) = front {
                this.received_at = chunk.received_at;
                this.current = Some(chunk);
                break;
            }
            if eof {
                return Poll::Ready(Ok(&[]));
            }
            std::task::ready!(this.reader.as_ref().as_ref().poll_read_ready(cx))?;
            match collect_ready_input(this.reader.as_ref().as_ref(), &this.input, &this.readable) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
        Poll::Ready(Ok(this.buffer()))
    }

    fn consume(self: Pin<&mut Self>, amount: usize) {
        let this = self.get_mut();
        let Some(chunk) = this.current.as_ref() else {
            return;
        };
        this.offset += amount.min(chunk.bytes.len() - this.offset);
        if this.offset == chunk.bytes.len() {
            this.current = None;
            this.offset = 0;
            this.input.send_modify(|queued| {
                let consumed = queued
                    .chunks
                    .pop_front()
                    .expect("current input stays queued until consumed");
                queued.bytes -= consumed.bytes.len();
            });
        }
    }
}

impl AsyncRead for ArrivalReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let available = std::task::ready!(self.as_mut().poll_fill_buf(cx))?;
        let read = available.len().min(buffer.remaining());
        buffer.put_slice(&available[..read]);
        self.consume(read);
        Poll::Ready(Ok(()))
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
    use tokio::sync::Semaphore;
    use tokio::time::timeout;

    async fn wait_for_chunks(changes: &mut watch::Receiver<ReadAhead>, count: usize) {
        loop {
            if changes.borrow().chunks.len() >= count {
                return;
            }
            changes.changed().await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn later_frame_keeps_its_arrival_while_an_earlier_frame_is_unread() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (server, _writer) = server.into_split();
        let mut reader = ArrivalReader::new(server);
        let bound = Duration::from_secs(1);
        let collector = reader.collect_input(Some(bound));
        let mut arrivals = reader.input.subscribe();
        tokio::select! {
            biased;
            result = collector => panic!("collector ended before frames were read: {result:?}"),
            () = async {
                // Drive real socket readiness with a running clock, then control only the waits.
                tokio::time::resume();
                client.write_all(b"{}\n").await.unwrap();
                wait_for_chunks(&mut arrivals, 1).await;
                tokio::time::pause();
                sleep(Duration::from_millis(400)).await;
                tokio::time::resume();
                client.write_all(b"{").await.unwrap();
                wait_for_chunks(&mut arrivals, 2).await;
                tokio::time::pause();
                let later_arrival = arrivals.borrow().chunks.back().unwrap().received_at;
                // The first frame stays unread throughout the later byte's arrival.
                sleep(Duration::from_millis(400)).await;
                let budget = Arc::new(Semaphore::new(1));
                let (_shutdown, mut shutdown) = watch::channel(false);
                let first = read_admitted_frame(&mut reader, budget.clone(), &mut shutdown, Some(bound))
                    .await.unwrap().unwrap();
                assert!(matches!(&first.1, IncomingLine::Complete(bytes) if bytes == b"{}\n"));
                drop(first);
                let error = read_admitted_frame(&mut reader, budget, &mut shutdown, Some(bound))
                    .await.err().expect("the later partial frame expires");
                assert!(matches!(error, crate::process_runtime::ProcessConnectionError::PeerIo(error)
                    if error.kind() == io::ErrorKind::TimedOut));
                assert!(later_arrival.elapsed() >= bound);
                assert!(later_arrival.elapsed() < bound + Duration::from_millis(2));
            } => {}
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_drained_buffer_cancels_its_queued_deadline() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (server, _writer) = server.into_split();
        let mut reader = ArrivalReader::new(server);
        let collector = reader.collect_input(Some(Duration::from_secs(1)));
        tokio::pin!(collector);
        client.write_all(b"a").await.unwrap();
        assert!(
            timeout(Duration::from_millis(900), &mut collector)
                .await
                .is_err()
        );
        assert_eq!(reader.read_u8().await.unwrap(), b'a');
        tokio::time::advance(Duration::from_millis(200)).await;
        assert!(
            timeout(Duration::from_secs(60), &mut collector)
                .await
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn full_read_ahead_preserves_expiry_while_the_consumer_waits() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (server, _writer) = server.into_split();
        let reader = ArrivalReader::new(server);
        client
            .write_all(&vec![b'x'; INBOUND_READ_AHEAD_BYTES * 2])
            .await
            .unwrap();
        let bound = Duration::from_secs(1);
        let started = Instant::now();
        let error = reader.collect_input(Some(bound)).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(started.elapsed(), bound);
        assert_eq!(reader.input.borrow().bytes, INBOUND_READ_AHEAD_BYTES);
    }

    #[tokio::test(start_paused = true)]
    async fn a_later_chunk_does_not_inherit_the_drained_chunk_deadline() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (server, _writer) = server.into_split();
        let mut reader = ArrivalReader::new(server);
        let collector = reader.collect_input(None);
        let mut arrivals = reader.input.subscribe();
        tokio::select! {
            biased;
            result = collector => panic!("collector ended: {result:?}"),
            () = async {
                let budget = Arc::new(Semaphore::new(1));
                let (_shutdown, mut shutdown) = watch::channel(false);
                let bound = Duration::from_secs(1);
                client.write_all(b"{}\n").await.unwrap();
                wait_for_chunks(&mut arrivals, 1).await;
                sleep(Duration::from_millis(900)).await;
                drop(read_admitted_frame(&mut reader, budget.clone(), &mut shutdown, Some(bound))
                    .await.unwrap().unwrap());
                client.write_all(b"{}\n").await.unwrap();
                wait_for_chunks(&mut arrivals, 1).await;
                sleep(Duration::from_millis(200)).await;
                let (_, frame) = read_admitted_frame(&mut reader, budget, &mut shutdown, Some(bound))
                    .await.expect("the later frame has its own deadline").unwrap();
                assert!(matches!(frame, IncomingLine::Complete(bytes) if bytes == b"{}\n"));
            } => {}
        }
    }

    #[tokio::test(start_paused = true)]
    async fn input_chunk_retains_its_timestamp_across_short_reads() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (server, _writer) = server.into_split();
        let mut reader = ArrivalReader::new(server);
        let readiness = reader.collect_input(None);
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
