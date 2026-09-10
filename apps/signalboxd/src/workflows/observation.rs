//! Internal admission of finite repository observations to the daemon runner.
//! Governed by docs/spec/workflows.md and docs/spec/repo-watch.md.

use super::repo_watch::observe::{OBSERVE_ENTRY, OBSERVE_REVISION};
use super::{WorkflowRuntimeError, WorkflowService};
use signalbox_domain::{
    DurableCommandId, ProgramCapability, ProgramRegistrationId, ProgramRunId,
    program_registration::{
        NativeProgramRegistrationRequest, ProgramContentDigest, ProgramExecutable, ProgramGrants,
    },
};
use signalbox_persistence::program_cancellation::{self, CancelProgramRun};
use uuid::Uuid;

impl WorkflowService {
    pub(crate) async fn observation_registration(
        &self,
    ) -> Result<ProgramRegistrationId, WorkflowRuntimeError> {
        let Some(ProgramExecutable::Native { binary_digest, .. }) = self.observation_executable
        else {
            return Err(WorkflowRuntimeError::NativeUnavailable);
        };
        let revision = format!(
            "{OBSERVE_REVISION}:{}",
            hex::encode(binary_digest.as_bytes())
        );
        let identity = ProgramContentDigest::of(format!("{OBSERVE_ENTRY}:{revision}").as_bytes());
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&identity.as_bytes()[..16]);
        let id = ProgramRegistrationId::from_uuid(Uuid::from_bytes(bytes));
        self.register_native(
            id,
            NativeProgramRegistrationRequest {
                name: OBSERVE_ENTRY.into(),
                revision,
                entry: OBSERVE_ENTRY.into(),
                native_revision: OBSERVE_REVISION.into(),
                binary_digest,
                grants: ProgramGrants::new([ProgramCapability::RepoWatch]),
            },
        )
        .await?;
        Ok(id)
    }

    pub(crate) async fn cancel_observation(
        &self,
        pool: &sqlx::PgPool,
        run: ProgramRunId,
    ) -> Result<(), program_cancellation::ProgramCancellationError> {
        let command = CancelProgramRun {
            command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
            run_id: run,
        };
        let result = program_cancellation::cancel(pool, command.clone()).await;
        self.complete_cancellation(pool, command, result).await?;
        Ok(())
    }
}
