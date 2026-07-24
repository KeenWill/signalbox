use std::path::{Path, PathBuf};

use signalbox_process_protocol::{
    ClientFrame, ClientRequest, MAX_FRAME_BYTES, RequestId, ServerFrame, ServerMessage,
    decode_server_line, encode_client_line,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
};

use crate::error::ClientError;

#[derive(Debug)]
pub(crate) struct ProcessClient {
    socket: PathBuf,
    next_request_id: u64,
}

impl ProcessClient {
    pub(crate) fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            next_request_id: 1,
        }
    }

    pub(crate) async fn request(
        &mut self,
        request: ClientRequest,
    ) -> Result<Connection, ClientError> {
        self.open(request, RequestDelivery::ReadOnly).await
    }

    pub(crate) async fn mutation_request(
        &mut self,
        request: ClientRequest,
    ) -> Result<Connection, ClientError> {
        self.open(request, RequestDelivery::Mutation).await
    }

    async fn open(
        &mut self,
        request: ClientRequest,
        delivery: RequestDelivery,
    ) -> Result<Connection, ClientError> {
        let request_id = RequestId::try_new(self.next_request_id)
            .map_err(|_| ClientError::Protocol("request identity exhausted"))?;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or(ClientError::Protocol("request identity exhausted"))?;
        Connection::open(&self.socket, request_id, request, delivery).await
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestDelivery {
    ReadOnly,
    Mutation,
}

pub(crate) struct Connection {
    request_id: RequestId,
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Connection {
    async fn open(
        socket: &Path,
        request_id: RequestId,
        request: ClientRequest,
        delivery: RequestDelivery,
    ) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(socket).await?;
        let (reader, writer) = stream.into_split();
        let mut connection = Self {
            request_id,
            reader: BufReader::new(reader),
            writer,
        };
        let frame = ClientFrame::try_new(request_id, request)
            .map_err(signalbox_process_protocol::FrameEncodeError::Validation)?;
        let encoded = encode_client_line(&frame)?;
        connection
            .writer
            .write_all(&encoded)
            .await
            .map_err(|error| {
                let error = ClientError::Io(error);
                match delivery {
                    RequestDelivery::ReadOnly => error,
                    RequestDelivery::Mutation => error.mutation(),
                }
            })?;
        Ok(connection)
    }

    pub(crate) async fn message(&mut self) -> Result<ServerMessage, ClientError> {
        Ok(self.frame().await?.message().clone())
    }

    pub(crate) async fn frame(&mut self) -> Result<ServerFrame, ClientError> {
        let line = read_frame_line(&mut self.reader).await?;
        let frame: ServerFrame = decode_server_line(&line)?;
        if frame.request_id() != self.request_id {
            return Err(ClientError::Protocol("response request identity mismatch"));
        }
        Ok(frame)
    }
}

async fn read_frame_line(reader: &mut BufReader<OwnedReadHalf>) -> Result<Vec<u8>, ClientError> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(ClientError::Protocol(
                "connection closed before a complete frame",
            ));
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            let consumed = newline + 1;
            if line.len().saturating_add(consumed) > MAX_FRAME_BYTES {
                return Err(ClientError::Protocol("server frame exceeded its bound"));
            }
            line.extend_from_slice(&available[..consumed]);
            reader.consume(consumed);
            return Ok(line);
        }
        if line.len().saturating_add(available.len()) >= MAX_FRAME_BYTES {
            return Err(ClientError::Protocol(
                "unterminated server frame reached its bound",
            ));
        }
        line.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
    }
}
