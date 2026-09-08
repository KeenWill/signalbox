//! `ProcessClient`: the framed Unix-socket connection to the local
//! `signalboxd` process, tracking request IDs and read-only, mutation, and
//! setup request delivery.

use std::path::{Path, PathBuf};

use signalbox_process_protocol::{
    ClientFrame, ClientRequest, FrameEncodeError, MAX_FRAME_BYTES, ProtocolVersion, RequestId,
    ServerFrame, ServerMessage, decode_server_line, encode_client_line,
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

    pub(crate) async fn setup_request(
        &mut self,
        request: ClientRequest,
    ) -> Result<Connection, ClientError> {
        self.open(request, RequestDelivery::Setup).await
    }

    pub(crate) async fn continue_read_request(
        &mut self,
        connection: &mut Connection,
        request: ClientRequest,
    ) -> Result<(), ClientError> {
        let request_id = self.next_request_id()?;
        connection
            .send(request_id, request, RequestDelivery::ReadOnly)
            .await
    }

    pub(crate) async fn continue_setup_request(
        &mut self,
        connection: &mut Connection,
        request: ClientRequest,
    ) -> Result<(), ClientError> {
        let request_id = self.next_request_id()?;
        connection
            .send(request_id, request, RequestDelivery::Setup)
            .await
    }

    pub(crate) async fn continue_mutation_request(
        &mut self,
        connection: &mut Connection,
        request: ClientRequest,
    ) -> Result<(), ClientError> {
        let request_id = self.next_request_id()?;
        connection
            .send(request_id, request, RequestDelivery::Mutation)
            .await
    }

    pub(crate) fn pending_request_id(&self) -> Result<RequestId, ClientError> {
        RequestId::try_new(self.next_request_id)
            .map_err(|_| ClientError::Protocol("request identity exhausted"))
    }

    fn next_request_id(&mut self) -> Result<RequestId, ClientError> {
        let request_id = self.pending_request_id()?;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or(ClientError::Protocol("request identity exhausted"))?;
        Ok(request_id)
    }

    async fn open(
        &mut self,
        request: ClientRequest,
        delivery: RequestDelivery,
    ) -> Result<Connection, ClientError> {
        let request_id = self.next_request_id()?;
        Connection::open(&self.socket, request_id, request, delivery).await
    }
}

/// A follow invocation reconnects at most three times, preserving its selected
/// session and accepted turn. Mutations never pass through this retry boundary.
#[derive(Default)]
pub(crate) struct FollowRetry {
    failures: usize,
}

impl FollowRetry {
    pub(crate) fn next_delay(
        &mut self,
        error: ClientError,
    ) -> Result<std::time::Duration, ClientError> {
        use std::{io::ErrorKind, time::Duration};
        let disconnected = match &error {
            ClientError::ConnectionClosed => true,
            ClientError::Io(error) => matches!(
                error.kind(),
                ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
                    | ErrorKind::NotConnected
                    | ErrorKind::ConnectionRefused
                    | ErrorKind::NotFound
            ),
            _ => false,
        };
        // The native client's finite retry schedule, without a new policy knob.
        let delays = [
            Duration::from_millis(250),
            Duration::from_millis(750),
            Duration::from_secs(2),
        ];
        if !disconnected || self.failures == delays.len() {
            return Err(error);
        }
        let delay = delays[self.failures];
        self.failures += 1;
        Ok(delay)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestDelivery {
    ReadOnly,
    Setup,
    Mutation,
}

pub(crate) struct Connection {
    version: ProtocolVersion,
    request_id: RequestId,
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    partial_frame_line: Vec<u8>,
}

impl Connection {
    async fn open(
        socket: &Path,
        request_id: RequestId,
        request: ClientRequest,
        delivery: RequestDelivery,
    ) -> Result<Self, ClientError> {
        let version = ProtocolVersion::One;
        let encoded = encode_request(version, request_id, request)?;
        let stream = UnixStream::connect(socket).await?;
        let (reader, writer) = stream.into_split();
        let mut connection = Self {
            version,
            request_id,
            reader: BufReader::new(reader),
            writer,
            partial_frame_line: Vec::new(),
        };
        connection.write_request(&encoded, delivery).await?;
        Ok(connection)
    }

    async fn send(
        &mut self,
        request_id: RequestId,
        request: ClientRequest,
        delivery: RequestDelivery,
    ) -> Result<(), ClientError> {
        let encoded = encode_request(self.version, request_id, request)?;
        self.write_request(&encoded, delivery).await?;
        self.request_id = request_id;
        Ok(())
    }

    async fn write_request(
        &mut self,
        encoded: &[u8],
        delivery: RequestDelivery,
    ) -> Result<(), ClientError> {
        self.writer.write_all(encoded).await.map_err(|error| {
            let error = ClientError::Io(error);
            match delivery {
                RequestDelivery::ReadOnly | RequestDelivery::Setup => error,
                RequestDelivery::Mutation => error.mutation(),
            }
        })
    }

    pub(crate) async fn message(&mut self) -> Result<ServerMessage, ClientError> {
        Ok(self.frame().await?.message().clone())
    }

    pub(crate) async fn frame(&mut self) -> Result<ServerFrame, ClientError> {
        let line = read_frame_line(&mut self.reader, &mut self.partial_frame_line).await?;
        let frame: ServerFrame = decode_server_line(&line)?;
        if !response_version_is_admitted(self.version, &frame) {
            return Err(ClientError::Protocol("response protocol version mismatch"));
        }
        if frame.request_id() != self.request_id {
            return Err(ClientError::Protocol("response request identity mismatch"));
        }
        Ok(frame)
    }
}

fn encode_request(
    version: ProtocolVersion,
    request_id: RequestId,
    request: ClientRequest,
) -> Result<Vec<u8>, ClientError> {
    let import_request = oversized_frame_is_import_source(&request);
    let frame = ClientFrame::try_new_for_version(version, request_id, request)
        .map_err(FrameEncodeError::Validation)?;
    encode_client_line(&frame).map_err(|error| match error {
        FrameEncodeError::OversizedFrame if import_request => ClientError::SourceExceedsFrame,
        FrameEncodeError::OversizedFrame => ClientError::Encode(FrameEncodeError::OversizedFrame),
        FrameEncodeError::Validation(error) => {
            ClientError::Encode(FrameEncodeError::Validation(error))
        }
        FrameEncodeError::Json(error) => ClientError::Encode(FrameEncodeError::Json(error)),
    })
}

fn oversized_frame_is_import_source(request: &ClientRequest) -> bool {
    match request {
        ClientRequest::ImportConversation { .. } => true,
        ClientRequest::CreateSession { .. }
        | ClientRequest::ListCredentialExclusions { .. }
        | ClientRequest::CancelProgramRun { .. }
        | ClientRequest::ClearCredentialExclusion { .. }
        | ClientRequest::ReadDeploymentLimits {}
        | ClientRequest::CreateSessionFromTemplate { .. }
        | ClientRequest::CommissionSession { .. }
        | ClientRequest::ListTemplates {}
        | ClientRequest::ListSessions {}
        | ClientRequest::ReadOperatorStatus {}
        | ClientRequest::ReloadConfiguration { .. }
        | ClientRequest::UpdateSessionPlacement { .. }
        | ClientRequest::AttachGoal { .. }
        | ClientRequest::ReadGoal { .. }
        | ClientRequest::ResumeGoal { .. }
        | ClientRequest::StopGoal { .. }
        | ClientRequest::StopSession { .. }
        | ClientRequest::SupersedeSession { .. }
        | ClientRequest::AbandonSession { .. }
        | ClientRequest::CloseSessionFailed { .. }
        | ClientRequest::ResumeSession { .. }
        | ClientRequest::AdoptSession { .. }
        | ClientRequest::ReleaseSession { .. }
        | ClientRequest::ReleaseStart { .. }
        | ClientRequest::SupersedeGoal { .. }
        | ClientRequest::SubmitInput { .. }
        | ClientRequest::CompactSession { .. }
        | ClientRequest::ReadTranscript { .. }
        | ClientRequest::FollowSession { .. }
        | ClientRequest::SpawnSession { .. }
        | ClientRequest::AwaitSession { .. }
        | ClientRequest::SendSessionMessage { .. }
        | ClientRequest::ListSessionMetadata { .. }
        | ClientRequest::ListConversations { .. }
        | ClientRequest::ListModelAliases {}
        | ClientRequest::ListModelCapabilities {}
        | ClientRequest::ReadSessionMetadata { .. }
        | ClientRequest::ReplaceSessionMetadata { .. }
        | ClientRequest::ReplaceSessionDefaults { .. }
        | ClientRequest::ReadSessionDefaults { .. }
        | ClientRequest::BeginConversationImport { .. }
        | ClientRequest::AppendConversationImport { .. }
        | ClientRequest::CommitConversationImport {}
        | ClientRequest::AbortConversationImport {}
        | ClientRequest::BeginBlobUpload { .. }
        | ClientRequest::AppendBlobUpload { .. }
        | ClientRequest::CommitBlobUpload {}
        | ClientRequest::AbortBlobUpload {}
        | ClientRequest::ReadBlobMetadata { .. }
        | ClientRequest::ReadBlobChunk { .. }
        | ClientRequest::ReadImportedConversation { .. }
        | ClientRequest::CreateSessionFromImportedFrontier { .. }
        | ClientRequest::ReconcileTurn { .. }
        | ClientRequest::CreateReviewTarget { .. }
        | ClientRequest::StartReviewRun { .. }
        | ClientRequest::ActivateReviewPass { .. }
        | ClientRequest::CompleteReviewPass { .. }
        | ClientRequest::RecordReviewFindings { .. }
        | ClientRequest::RecordReviewFindingEvent { .. }
        | ClientRequest::ReserveReviewExternalLink { .. }
        | ClientRequest::AttachReviewExternalLink { .. }
        | ClientRequest::ReadReviewTarget { .. }
        | ClientRequest::ReadReviewRun { .. }
        | ClientRequest::ReadReviewFinding { .. }
        | ClientRequest::ListReviewFindings { .. }
        | ClientRequest::StartReviewOrchestration { .. }
        | ClientRequest::RecordReviewImportOutcome { .. }
        | ClientRequest::RecordReviewConcernOutcome { .. }
        | ClientRequest::RecordReviewJudgmentPlan { .. }
        | ClientRequest::RecordReviewJudgmentEffect { .. }
        | ClientRequest::RecordReviewRepairOutcomes { .. }
        | ClientRequest::RecordReviewPublicationOutcomes { .. }
        | ClientRequest::ReadReviewOrchestration { .. }
        | ClientRequest::StopTurn { .. }
        | ClientRequest::DecideToolRequest { .. }
        | ClientRequest::OverrideDeniedToolRequest { .. }
        | ClientRequest::ReplaceLostRunner { .. }
        | ClientRequest::AbandonLostRunner { .. }
        | ClientRequest::RegisterWorkspace { .. }
        | ClientRequest::MintGitRemote { .. }
        | ClientRequest::WithdrawGitRemote { .. }
        | ClientRequest::PromotePendingRunner { .. }
        | ClientRequest::ProvisionOauthCredential { .. }
        | ClientRequest::ReprovisionOauthCredential { .. }
        | ClientRequest::DeleteOauthCredential { .. } => false,
    }
}

fn response_version_is_admitted(expected: ProtocolVersion, frame: &ServerFrame) -> bool {
    frame.version() == expected
}

async fn read_frame_line(
    reader: &mut BufReader<OwnedReadHalf>,
    partial_frame_line: &mut Vec<u8>,
) -> Result<Vec<u8>, ClientError> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if partial_frame_line.is_empty() {
                return Err(ClientError::ConnectionClosed);
            }
            return Err(ClientError::Protocol(
                "connection closed before a complete frame",
            ));
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            let consumed = newline + 1;
            if partial_frame_line.len().saturating_add(consumed) > MAX_FRAME_BYTES {
                return Err(ClientError::Protocol("server frame exceeded its bound"));
            }
            partial_frame_line.extend_from_slice(&available[..consumed]);
            reader.consume(consumed);
            return Ok(std::mem::take(partial_frame_line));
        }
        if partial_frame_line.len().saturating_add(available.len()) >= MAX_FRAME_BYTES {
            return Err(ClientError::Protocol(
                "unterminated server frame reached its bound",
            ));
        }
        partial_frame_line.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, time::Duration};

    use signalbox_process_protocol::{ErrorCode, ErrorDetail, encode_server_line};
    use tokio::time::timeout;

    use super::*;

    const FRAME_SPLIT_POSITION: usize = 8;
    const CANCELLATION_DEADLINE: Duration = Duration::from_millis(10);

    #[tokio::test]
    async fn frame_read_is_cancellation_safe() -> Result<(), Box<dyn Error>> {
        let frame = error_frame(ProtocolVersion::One, ErrorCode::InvalidRequest)?;
        let expected = frame.message().clone();
        let encoded = encode_server_line(&frame)?;
        let (server, client) = UnixStream::pair()?;
        let (reader, writer) = client.into_split();
        let (_, mut server_writer) = server.into_split();
        let mut connection = Connection {
            version: ProtocolVersion::One,
            request_id: frame.request_id(),
            reader: BufReader::new(reader),
            writer,
            partial_frame_line: Vec::new(),
        };

        server_writer
            .write_all(&encoded[..FRAME_SPLIT_POSITION])
            .await?;
        assert!(
            timeout(CANCELLATION_DEADLINE, connection.message())
                .await
                .is_err()
        );
        assert_eq!(
            connection.partial_frame_line,
            encoded[..FRAME_SPLIT_POSITION]
        );
        server_writer
            .write_all(&encoded[FRAME_SPLIT_POSITION..])
            .await?;

        assert_eq!(connection.message().await?, expected);
        Ok(())
    }

    #[tokio::test]
    async fn empty_eof_is_a_clean_daemon_disconnect() -> Result<(), Box<dyn Error>> {
        let (server, client) = UnixStream::pair()?;
        drop(server);
        let (reader, _) = client.into_split();
        let error = read_frame_line(&mut BufReader::new(reader), &mut Vec::new())
            .await
            .expect_err("a closed socket cannot yield a frame");
        assert!(matches!(error, ClientError::ConnectionClosed));
        assert_eq!(error.to_string(), "the daemon connection closed");
        assert!(
            error.mutation().is_ambiguous_mutation(),
            "a lost mutation receipt must retain replay semantics"
        );
        Ok(())
    }

    #[tokio::test]
    async fn truncated_frame_is_not_retried_as_a_clean_disconnect() -> Result<(), Box<dyn Error>> {
        let (mut server, client) = UnixStream::pair()?;
        server.write_all(b"{").await?;
        drop(server);
        let (reader, _) = client.into_split();
        let error = read_frame_line(&mut BufReader::new(reader), &mut Vec::new())
            .await
            .expect_err("a truncated frame must fail protocol admission");
        assert!(matches!(
            error,
            ClientError::Protocol("connection closed before a complete frame")
        ));
        assert!(FollowRetry::default().next_delay(error).is_err());
        Ok(())
    }

    #[test]
    fn follow_reconnect_budget_exhausts_after_three_attempts() {
        let mut retry = FollowRetry::default();
        for _ in 0..3 {
            assert!(retry.next_delay(ClientError::ConnectionClosed).is_ok());
        }
        assert!(matches!(
            retry.next_delay(ClientError::ConnectionClosed),
            Err(ClientError::ConnectionClosed)
        ));
    }

    fn error_frame(
        version: ProtocolVersion,
        code: ErrorCode,
    ) -> Result<ServerFrame, Box<dyn Error>> {
        Ok(ServerFrame::try_new_for_version(
            version,
            RequestId::try_new(1)?,
            ServerMessage::Error {
                code,
                message: String::from("fixture"),
                detail: ErrorDetail::none(),
            },
        )?)
    }
}
