//! Operator transport for workflow evaluation launches and sealed scorecards.

use signalbox_process_protocol::*;
use std::{error::Error, io, path::Path};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

pub type EvalClientError = Box<dyn Error + Send + Sync>;

struct Connection {
    stream: BufReader<UnixStream>,
    next_id: u64,
}

impl Connection {
    async fn open(socket: &Path) -> Result<Self, EvalClientError> {
        Ok(Self {
            stream: BufReader::new(UnixStream::connect(socket).await?),
            next_id: 1,
        })
    }
    async fn request(&mut self, request: ClientRequest) -> Result<ServerMessage, EvalClientError> {
        let id = RequestId::try_new(self.next_id)?;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("request identity exhausted"))?;
        let frame = ClientFrame::try_new_for_version(ProtocolVersion::One, id, request)?;
        self.stream
            .get_mut()
            .write_all(&encode_client_line(&frame)?)
            .await?;
        let mut line = Vec::new();
        (&mut self.stream)
            .take((MAX_FRAME_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
            .await?;
        let frame = decode_server_line(&line)?;
        if frame.request_id() != id || frame.version() != ProtocolVersion::One {
            return Err(io::Error::other("uncorrelated evaluation response").into());
        }
        match frame.message() {
            ServerMessage::Error { code, message, .. } => {
                Err(io::Error::other(format!("{code:?}: {message}")).into())
            }
            message => Ok(message.clone()),
        }
    }
}

async fn upload(socket: &Path, bytes: &[u8]) -> Result<CanonicalBlobDigest, EvalClientError> {
    let digest = CanonicalBlobDigest::from_digest(signalbox_domain::BlobDigest::digest(bytes));
    let length = bytes.len() as u64;
    let mut connection = Connection::open(socket).await?;
    match connection
        .request(ClientRequest::BeginBlobUpload {
            expected_digest: digest,
            expected_length_bytes: CanonicalU64::new(length),
        })
        .await?
    {
        ServerMessage::BlobUploadAlreadyPresent {
            digest: observed,
            byte_length,
        } if observed == digest && byte_length.value() == length => return Ok(digest),
        ServerMessage::BlobUploadBegun {
            expected_digest,
            expected_length_bytes,
        } if expected_digest == digest && expected_length_bytes.value() == length => {}
        _ => return Err(io::Error::other("unexpected blob upload receipt").into()),
    }
    let mut assembled = 0;
    for bytes in bytes.chunks(MAX_BLOB_CHUNK_BYTES) {
        assembled += bytes.len() as u64;
        let chunk = BlobChunk::new(bytes.to_vec());
        match connection
            .request(ClientRequest::AppendBlobUpload { chunk })
            .await?
        {
            ServerMessage::BlobUploadAppended {
                assembled_length_bytes,
            } if assembled_length_bytes.value() == assembled => {}
            _ => return Err(io::Error::other("unexpected blob append receipt").into()),
        }
    }
    match connection
        .request(ClientRequest::CommitBlobUpload {})
        .await?
    {
        ServerMessage::BlobUploadCommitted {
            digest: observed,
            byte_length,
        } if observed == digest && byte_length.value() == length => Ok(digest),
        _ => Err(io::Error::other("unexpected blob commit receipt").into()),
    }
}

/// Upload immutable inputs, launch through the daemon, and read its sealed scorecard.
pub async fn launch_and_report(
    socket: &Path,
    corpus: &[u8],
    responses: Option<&[u8]>,
    format: EvaluationCorpusFormat,
    cases: Vec<u32>,
    repeats: u32,
) -> Result<serde_json::Value, EvalClientError> {
    let corpus = upload(socket, corpus).await?;
    let recorded_responses = match responses {
        Some(bytes) => Some(upload(socket, bytes).await?),
        None => None,
    };
    let run_id = CanonicalUuid::from_uuid(uuid::Uuid::new_v4());
    let registration_id = CanonicalUuid::from_uuid(uuid::Uuid::new_v4());
    eprintln!("run_id={run_id} registration_id={registration_id}");
    let input = EvaluationInput {
        corpus,
        format,
        cases,
        repeats,
        recorded_responses,
    };
    match Connection::open(socket)
        .await?
        .request(ClientRequest::LaunchEvaluation {
            run_id,
            registration_id,
            input,
        })
        .await?
    {
        ServerMessage::ProgramRunStarted {
            run_id: observed,
            registration_id: registered,
        } if observed == run_id && registered == registration_id => {}
        _ => return Err(io::Error::other("unexpected evaluation launch receipt").into()),
    }
    loop {
        match Connection::open(socket)
            .await?
            .request(ClientRequest::ReadProgramRun { run_id })
            .await?
        {
            ServerMessage::ProgramRunRead {
                run_id: observed,
                run,
            } if observed == run_id => match run.outcome {
                ProgramRunState::Succeeded { .. } => break,
                ProgramRunState::Running {} => {}
                outcome => {
                    return Err(io::Error::other(format!("evaluation ended: {outcome:?}")).into());
                }
            },
            _ => return Err(io::Error::other("unexpected workflow read response").into()),
        }
        // Operator polling cadence; durable progress is owned by the daemon.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let mut scorecard = Vec::new();
    loop {
        let offset = scorecard.len() as u64;
        match Connection::open(socket)
            .await?
            .request(ClientRequest::ReadEvaluationScorecard { run_id, offset })
            .await?
        {
            ServerMessage::EvaluationScorecardRead {
                run_id: observed,
                offset: start,
                bytes,
                total_bytes,
            } if observed == run_id && start == offset => {
                if bytes.is_empty() && offset < total_bytes {
                    return Err(io::Error::other("empty scorecard range").into());
                }
                scorecard.extend(bytes);
                if scorecard.len() as u64 == total_bytes {
                    return Ok(serde_json::from_slice(&scorecard)?);
                }
                if scorecard.len() as u64 > total_bytes {
                    return Err(io::Error::other("scorecard range exceeds total length").into());
                }
            }
            _ => return Err(io::Error::other("unexpected sealed scorecard response").into()),
        }
    }
}
