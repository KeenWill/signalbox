//! Daemon admission and compiled workflow catalog; governed by docs/spec/workflows.md.

pub mod runtime;

#[cfg(target_os = "linux")]
use signalbox_domain::InlineFramePayload;
use signalbox_domain::{
    ProgramRegistrationId, ProgramRunId,
    program_registration::{
        NativeProgramRegistrationRequest, ProgramExecutable, ProgramRegistration,
        ProgramRegistrationRequest,
    },
};
use signalbox_persistence::program_registration::ProgramRegistrationRepository;
#[cfg(target_os = "linux")]
use signalbox_workflow_runtime::native::{NativeCatalog, NativeProgram, WorkflowContext};
use signalbox_workflow_runtime::native::{NativeProgramError, NativeValue};
use tokio::sync::mpsc;

pub use runtime::{WorkflowRuntime, WorkflowRuntimeError};

/// Compiled pilot catalog identity.
pub const CLOCK_ENTRY: &str = "clock";
/// Revision of the pilot's big-endian u64 input and input/time result.
pub const CLOCK_REVISION: &str = "1";

/// User-authorized admission to the daemon-owned runner.
#[derive(Clone, Debug)]
pub struct WorkflowService {
    registrations: ProgramRegistrationRepository,
    wake: mpsc::UnboundedSender<ProgramRunId>,
    clock_executable: Option<ProgramExecutable>,
}

impl WorkflowService {
    pub fn clock_executable(&self) -> Option<&ProgramExecutable> {
        self.clock_executable.as_ref()
    }

    pub async fn register_javascript(
        &self,
        id: ProgramRegistrationId,
        request: ProgramRegistrationRequest,
    ) -> Result<ProgramRegistration, WorkflowRuntimeError> {
        Ok(self.registrations.register_user(id, request).await?)
    }

    /// Accepts only the exact compiled catalog identity in this daemon.
    pub async fn register_native(
        &self,
        id: ProgramRegistrationId,
        request: NativeProgramRegistrationRequest,
    ) -> Result<ProgramRegistration, WorkflowRuntimeError> {
        if Some(&request.clone().into_content().executable) != self.clock_executable.as_ref() {
            return Err(WorkflowRuntimeError::NativeUnavailable);
        }
        Ok(self.registrations.register_native_user(id, request).await?)
    }

    /// Commits admission before waking the runner; a lost wake is recovered on startup.
    pub async fn start(
        &self,
        run: ProgramRunId,
        registration: ProgramRegistrationId,
        input: &[u8],
    ) -> Result<ProgramRunId, WorkflowRuntimeError> {
        let run = self
            .registrations
            .start_run(run, registration, input)
            .await?;
        self.wake
            .send(run)
            .map_err(|_| WorkflowRuntimeError::Stopped)?;
        Ok(run)
    }
}

#[cfg(target_os = "linux")]
fn compiled_catalog() -> Result<NativeCatalog, WorkflowRuntimeError> {
    let mut catalog = NativeCatalog::new().map_err(WorkflowRuntimeError::Runtime)?;
    catalog
        .insert::<ClockProgram>(CLOCK_ENTRY.into(), CLOCK_REVISION.into())
        .map_err(WorkflowRuntimeError::Catalog)?;
    Ok(catalog)
}

/// Pilot input and clock answer: exactly one big-endian u64.
#[derive(Debug, PartialEq, Eq)]
pub struct ClockInput(pub u64);

impl NativeValue for ClockInput {
    fn decode(bytes: &[u8]) -> Result<Self, NativeProgramError> {
        let bytes = bytes.try_into().map_err(|error| {
            NativeProgramError::new(format!("expected one big-endian u64: {error}"))
        })?;
        Ok(Self(u64::from_be_bytes(bytes)))
    }
    fn encode(&self) -> Result<Vec<u8>, NativeProgramError> {
        Ok(self.0.to_be_bytes().to_vec())
    }
}

/// Pilot result retains its input and the journaled Unix time in seconds.
#[derive(Debug, PartialEq, Eq)]
pub struct ClockResult {
    pub input: ClockInput,
    pub time: ClockInput,
}

impl NativeValue for ClockResult {
    fn decode(bytes: &[u8]) -> Result<Self, NativeProgramError> {
        let (input, time) = bytes
            .split_at_checked(size_of::<u64>())
            .ok_or_else(|| NativeProgramError::new("missing clock input"))?;
        Ok(Self {
            input: ClockInput::decode(input)?,
            time: ClockInput::decode(time)?,
        })
    }
    fn encode(&self) -> Result<Vec<u8>, NativeProgramError> {
        let mut bytes = self.input.encode()?;
        bytes.extend(self.time.encode()?);
        Ok(bytes)
    }
}

#[cfg(target_os = "linux")]
struct ClockProgram;
#[cfg(target_os = "linux")]
impl NativeProgram for ClockProgram {
    type Input = ClockInput;
    type Output = ClockResult;
    async fn run(
        mut context: WorkflowContext,
        input: ClockInput,
    ) -> Result<ClockResult, NativeProgramError> {
        let answer = context
            .now(InlineFramePayload::new(input.encode()?))
            .await?;
        Ok(ClockResult {
            input,
            time: ClockInput::decode(answer.as_bytes())?,
        })
    }
}
