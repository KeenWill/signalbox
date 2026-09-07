//! Process-protocol configuration reload request and receipt mapping.

use super::*;
use crate::configuration_reload::ConfigurationReloadError;
use signalbox_persistence::reload_configuration::{
    ReloadConfiguration, ReloadLookup, ReloadPhase, ReloadRepositoryError, ReloadResult,
};
use signalbox_process_protocol::{CommandId, ConfigurationReloadPhase, ReloadedSection};

pub(super) async fn handle_reload<Writer: AsyncWrite + Unpin>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: CommandId,
    services: &ConnectionServices,
) -> Result<(), ProcessConnectionError> {
    let Some(reload) = &services.configuration_reload else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::Unavailable),
        )
        .await;
    };
    let request = ReloadConfiguration {
        command_id: DurableCommandId::from_uuid(command_id.into_uuid()),
    };
    match reload.reload(request).await {
        Ok(ReloadLookup::Recorded(result)) => {
            let message = match result {
                ReloadResult::Reloaded => ServerMessage::ConfigurationReloaded {
                    command_id,
                    reloaded_sections: ReloadedSection::ALL.to_vec(),
                },
                ReloadResult::Failed { phase, reason } => {
                    ServerMessage::ConfigurationReloadFailed {
                        command_id,
                        phase: match phase {
                            ReloadPhase::Read => ConfigurationReloadPhase::Read,
                            ReloadPhase::Validate => ConfigurationReloadPhase::Validate,
                            ReloadPhase::Activate => ConfigurationReloadPhase::Activate,
                            ReloadPhase::Reconcile => ConfigurationReloadPhase::Reconcile,
                            ReloadPhase::Install => ConfigurationReloadPhase::Install,
                        },
                        reason,
                    }
                }
            };
            write_message(writer, version, request_id, message).await
        }
        Ok(ReloadLookup::ConflictingReuse) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Ok(ReloadLookup::Pending) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError {
                    code: ErrorCode::Unavailable,
                    message: "configuration reload is busy",
                    detail: ErrorDetail::none(),
                },
            )
            .await
        }
        outcome @ (Ok(ReloadLookup::Unclaimed) | Err(_)) => {
            let (error, recovery_required) = match outcome {
                Err(ConfigurationReloadError::BeforeEffect(error)) => (Some(error), false),
                Err(ConfigurationReloadError::RecoveryRequired(error)) => (Some(error), true),
                _ => (None, false),
            };
            let code = match error {
                Some(ReloadRepositoryError::CommitAmbiguous(_)) => ErrorCode::CommitAmbiguous,
                Some(ReloadRepositoryError::Database(_)) => ErrorCode::Unavailable,
                _ => ErrorCode::Internal,
            };
            tracing::error!(
                cause = "configuration_reload_persistence_failed",
                recovery_required,
                "configuration reload persistence failed"
            );
            if recovery_required && let Some(reporter) = &services.recovery_reporter {
                reporter.report_recovery_required();
            }
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(code),
            )
            .await
        }
    }
}
