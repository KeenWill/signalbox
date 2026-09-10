//! User-authorized evaluation launch and sealed scorecard reads.

use super::*;
use crate::workflows::eval::{self, EVAL_ENTRY, EVAL_REVISION};
use signalbox_domain::{
    ProgramCapability, ProgramRegistrationId, ProgramRunId,
    program_registration::{NativeProgramRegistrationRequest, ProgramExecutable, ProgramGrants},
};
use signalbox_workflow_runtime::native::NativeValue;

pub(super) async fn launch<Writer: AsyncWrite + Unpin>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    run_id: CanonicalUuid,
    registration_id: CanonicalUuid,
    input: signalbox_process_protocol::EvaluationInput,
    services: &ConnectionServices,
) -> Result<(), ProcessConnectionError> {
    let result = async {
        let unavailable = || ProtocolError::without_detail(ErrorCode::Unavailable);
        let service = services.workflows.as_ref().ok_or_else(unavailable)?;
        let stores = services
            .blob_store_registry
            .clone()
            .ok_or_else(unavailable)?;
        let Some(ProgramExecutable::Native { binary_digest, .. }) = service.eval_executable()
        else {
            return Err(unavailable());
        };
        let manifest = eval::launch::prepare(
            services.pool.clone(),
            stores,
            &services.model_configuration,
            input,
        )
        .await
        .map_err(|error| {
            ProtocolError::without_detail(match error {
                eval::EvalFailure::Rejected(_) => ErrorCode::InvalidRequest,
                eval::EvalFailure::Infrastructure(_) => ErrorCode::Unavailable,
            })
        })?;
        let input = manifest
            .encode()
            .map_err(|_| ProtocolError::without_detail(ErrorCode::InvalidRequest))?;
        let registration = ProgramRegistrationId::from_uuid(registration_id.into_uuid());
        service
            .register_native(
                registration,
                NativeProgramRegistrationRequest {
                    name: format!("evaluation-{registration_id}"),
                    revision: EVAL_REVISION.into(),
                    entry: EVAL_ENTRY.into(),
                    native_revision: EVAL_REVISION.into(),
                    binary_digest: *binary_digest,
                    grants: ProgramGrants::new([
                        ProgramCapability::Corpus,
                        ProgramCapability::Judge,
                        ProgramCapability::Blob,
                        ProgramCapability::EvalRecord,
                    ]),
                },
            )
            .await
            .map_err(program::workflow_error)?;
        service
            .start(
                ProgramRunId::from_uuid(run_id.into_uuid()),
                registration,
                &input,
            )
            .await
            .map_err(|error| match error {
                crate::workflows::WorkflowRuntimeError::Stopped => {
                    ProtocolError::mutation_commit_ambiguous()
                }
                error => program::workflow_error(error),
            })?;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        return write_error(writer, version, request_id, error).await;
    }
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ProgramRunStarted {
            run_id,
            registration_id,
        },
    )
    .await
}

pub(super) async fn read<Writer: AsyncWrite + Unpin>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    run_id: CanonicalUuid,
    offset: u64,
    services: &ConnectionServices,
) -> Result<(), ProcessConnectionError> {
    let result = async {
        let snapshot =
            signalbox_persistence::evaluation::EvaluationRepository::new(services.pool.clone())
                .load(ProgramRunId::from_uuid(run_id.into_uuid()))
                .await
                .map_err(|_| ProtocolError::without_detail(ErrorCode::Unavailable))?
                .ok_or_else(|| ProtocolError::without_detail(ErrorCode::NotFound))?;
        let bytes = serde_json::to_vec(&snapshot.scorecard)
            .map_err(|_| ProtocolError::without_detail(ErrorCode::Internal))?;
        let total_bytes = bytes.len() as u64;
        let start = usize::try_from(offset)
            .ok()
            .filter(|start| *start <= bytes.len())
            .ok_or_else(|| ProtocolError::without_detail(ErrorCode::InvalidRequest))?;
        let frame = ServerFrame::try_new_for_version(
            version,
            request_id,
            ServerMessage::EvaluationScorecardRead {
                run_id,
                offset,
                bytes: Vec::new(),
                total_bytes,
            },
        )
        .map_err(|_| ProtocolError::without_detail(ErrorCode::Internal))?;
        let envelope = encode_server_line(&frame)
            .map_err(|_| ProtocolError::without_detail(ErrorCode::Internal))?;
        let (bytes, _) =
            program::program_byte_prefix(&bytes[start..], &mut (MAX_FRAME_BYTES - envelope.len()));
        Ok(ServerMessage::EvaluationScorecardRead {
            run_id,
            offset,
            bytes,
            total_bytes,
        })
    }
    .await;
    match result {
        Ok(message) => write_message(writer, version, request_id, message).await,
        Err(error) => write_error(writer, version, request_id, error).await,
    }
}
