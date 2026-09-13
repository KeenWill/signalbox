//! Explicit mapping of durable domain lease authority to the runner wire.

use signalbox_domain::{
    RunnerGeneration, RunnerLease, RunnerLeaseCorrelation, RunnerSandboxProfile,
    RunnerToolEffectClass, RunnerWorkingDirectory, ToolAttemptDispatchCorrelation,
    ToolAttemptDispatchCorrelationReconstitutionInput, ToolDispatchGeneration,
};
use signalbox_runner_wire::{
    CanonicalUuid, Dispatch, EffectClass, LeaseCorrelation, LeaseOffer, PositiveU64, ProfileName,
    ResultBounds, SandboxProfile, ValueError, WireToolName, WorkingDirectory,
};

pub(crate) fn offer(lease: &RunnerLease) -> Result<LeaseOffer, ValueError> {
    let authorization = lease.credential_authorization();
    Ok(LeaseOffer {
        correlation: wire_correlation(&lease.correlation())?,
        effect_class: match lease.effect() {
            RunnerToolEffectClass::Pure => EffectClass::Pure,
            RunnerToolEffectClass::Idempotent => EffectClass::Idempotent,
            RunnerToolEffectClass::SideEffecting => EffectClass::SideEffecting,
        },
        credential_profile: authorization
            .map(|grant| ProfileName::try_new(grant.profile.as_str().to_owned()))
            .transpose()?,
        grant_revision: authorization
            .map(|grant| positive(grant.grant_revision.get()))
            .transpose()?,
        normalized_arguments: serde_json::from_str(lease.arguments().as_str())
            .map_err(|_| ValueError::Correlation)?,
        result_bounds: ResultBounds::version_one(),
    })
}

pub(crate) fn dispatch(lease: &RunnerLease) -> Result<Dispatch, ValueError> {
    Ok(Dispatch {
        correlation: wire_correlation(&lease.correlation())?,
        normalized_arguments: serde_json::from_str(lease.arguments().as_str())
            .map_err(|_| ValueError::Correlation)?,
    })
}

pub(crate) fn wire_correlation(
    value: &RunnerLeaseCorrelation,
) -> Result<LeaseCorrelation, ValueError> {
    Ok(LeaseCorrelation {
        registration_revision: positive(value.registration_revision.get())?,
        placement_revision: positive(value.placement_revision.get())?,
        working_directory: WorkingDirectory::try_new(value.working_directory.as_str().to_owned())?,
        sandbox_profile: match value.sandbox {
            RunnerSandboxProfile::Ambient => SandboxProfile::Ambient,
            RunnerSandboxProfile::WorkspaceRestricted => SandboxProfile::WorkspaceRestricted,
        },
        lease_id: CanonicalUuid::from_uuid(value.lease.into_uuid()),
        lease_generation: positive(value.generation.get())?,
        runner_id: CanonicalUuid::from_uuid(value.runner.into_uuid()),
        tool_name: WireToolName::try_new(value.tool.as_str().to_owned())?,
        session_id: CanonicalUuid::from_uuid(value.dispatch.session().into_uuid()),
        turn_id: CanonicalUuid::from_uuid(value.dispatch.turn().into_uuid()),
        tool_request_id: CanonicalUuid::from_uuid(value.dispatch.request().into_uuid()),
        tool_attempt_id: CanonicalUuid::from_uuid(value.dispatch.attempt().into_uuid()),
        issuing_turn_attempt_id: CanonicalUuid::from_uuid(
            value.dispatch.issuing_attempt().into_uuid(),
        ),
        tool_dispatch_generation: positive(value.dispatch.generation().as_u64())?,
    })
}

pub(crate) fn domain_correlation(
    value: LeaseCorrelation,
) -> Result<RunnerLeaseCorrelation, ValueError> {
    use signalbox_domain::{
        RunnerId, RunnerLeaseId, SessionId, ToolAttemptId, ToolName, ToolRequestId, TurnAttemptId,
        TurnId,
    };
    Ok(RunnerLeaseCorrelation {
        lease: RunnerLeaseId::from_uuid(value.lease_id.into_uuid()),
        runner: RunnerId::from_uuid(value.runner_id.into_uuid()),
        registration_revision: generation(value.registration_revision)?,
        placement_revision: generation(value.placement_revision)?,
        working_directory: RunnerWorkingDirectory::try_new(
            value.working_directory.as_str().to_owned(),
        )
        .map_err(|_| ValueError::WorkingDirectory)?,
        sandbox: match value.sandbox_profile {
            SandboxProfile::Ambient => RunnerSandboxProfile::Ambient,
            SandboxProfile::WorkspaceRestricted => RunnerSandboxProfile::WorkspaceRestricted,
        },
        tool: ToolName::try_new(value.tool_name.as_str().to_owned())
            .map_err(|_| ValueError::PortableName)?,
        dispatch: ToolAttemptDispatchCorrelation::reconstitute(
            ToolAttemptDispatchCorrelationReconstitutionInput {
                session: SessionId::from_uuid(value.session_id.into_uuid()),
                turn: TurnId::from_uuid(value.turn_id.into_uuid()),
                issuing_attempt: TurnAttemptId::from_uuid(
                    value.issuing_turn_attempt_id.into_uuid(),
                ),
                request: ToolRequestId::from_uuid(value.tool_request_id.into_uuid()),
                attempt: ToolAttemptId::from_uuid(value.tool_attempt_id.into_uuid()),
                generation: ToolDispatchGeneration::try_from_u64(
                    value.tool_dispatch_generation.get(),
                )
                .ok_or(ValueError::Correlation)?,
            },
        ),
        generation: generation(value.lease_generation)?,
    })
}

fn positive(value: u64) -> Result<PositiveU64, ValueError> {
    PositiveU64::try_new(value)
}
fn generation(value: PositiveU64) -> Result<RunnerGeneration, ValueError> {
    RunnerGeneration::try_from_u64(value.get()).ok_or(ValueError::Correlation)
}
