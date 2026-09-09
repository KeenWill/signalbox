//! Compiled, trusted programs with sequential journaled context requests.

use std::{
    any::TypeId,
    collections::BTreeMap,
    error::Error,
    fmt,
    future::Future,
    pin::Pin,
    sync::{Mutex, OnceLock},
    task::Poll,
};

use signalbox_domain::{
    DeliveryKind, EffectRequest, InlineFramePayload, ProgramFault, ProgramJournal, ProgramRunId,
    ReplayCursor, ReplayInstruction, RequestKind,
    program_registration::{ProgramContentDigest, ProgramExecutable},
};
use tokio::sync::{mpsc, oneshot};

use crate::{
    ExecutionState, HostRequest, LiveDeliverySource, ProgramExecutionOutcome, WorkflowHost,
    WorkflowHostError, WorkflowHostProtocolError,
};

/// Checked exact-byte encoding and decoding at the native run boundary.
pub trait NativeValue: Sized {
    fn decode(bytes: &[u8]) -> Result<Self, NativeProgramError>;
    fn encode(&self) -> Result<Vec<u8>, NativeProgramError>;
}

/// Trusted program logic obtains nondeterminism only through the context.
/// Direct I/O, ambient time/randomness, task spawning and concurrent awaits are
/// forbidden by the authoring contract; native code is not sandboxed.
pub trait NativeProgram: 'static {
    type Input: NativeValue + 'static;
    type Output: NativeValue + 'static;

    fn run(
        context: WorkflowContext,
        input: Self::Input,
    ) -> impl Future<Output = Result<Self::Output, NativeProgramError>> + 'static;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeProgramError(Box<str>);

impl NativeProgramError {
    pub fn new(message: impl Into<Box<str>>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for NativeProgramError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl Error for NativeProgramError {}

type NativeFuture = Pin<Box<dyn Future<Output = Result<(), NativeProgramError>>>>;
type NativeEntry = fn(WorkflowContext, &[u8]) -> Result<NativeFuture, NativeProgramError>;

/// Entries compiled into this exact executing binary; no code loading occurs.
#[derive(Debug)]
pub struct NativeCatalog {
    binary_digest: ProgramContentDigest,
    entries: BTreeMap<(String, String), NativeEntry>,
}

impl NativeCatalog {
    /// Hashes the running image once per process, on the host side.
    /// Returns `Unsupported` on platforms without a running-image handle.
    pub fn new() -> Result<Self, std::io::Error> {
        static DIGEST: OnceLock<Result<ProgramContentDigest, std::io::Error>> = OnceLock::new();
        let digest = DIGEST
            .get_or_init(|| {
                let mut image = running_image()?;
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut image, &mut bytes)?;
                Ok(ProgramContentDigest::of(&bytes))
            })
            .as_ref()
            .map_err(|error| std::io::Error::new(error.kind(), error))?;
        Ok(Self {
            binary_digest: *digest,
            entries: BTreeMap::new(),
        })
    }

    /// Admits one compiled entry/revision, rejecting duplicate catalog keys.
    /// Each key remains bound to one program type for the process lifetime.
    pub fn insert<P: NativeProgram>(
        &mut self,
        entry: String,
        revision: String,
    ) -> Result<(), NativeProgramError> {
        static IMPLEMENTATIONS: Mutex<BTreeMap<(String, String), TypeId>> =
            Mutex::new(BTreeMap::new());
        match self.entries.entry((entry, revision)) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                let mut implementations = IMPLEMENTATIONS.lock().map_err(|_| {
                    NativeProgramError::new("native implementation bindings poisoned")
                })?;
                let implementation = implementations
                    .entry(slot.key().clone())
                    .or_insert(TypeId::of::<P>());
                if *implementation != TypeId::of::<P>() {
                    return Err(NativeProgramError::new(
                        "native entry/revision is bound to a different program type",
                    ));
                }
                slot.insert(start::<P>);
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(_) => Err(NativeProgramError::new(
                "duplicate native catalog entry/revision",
            )),
        }
    }

    /// Selects the executable identity to retain in a user registration.
    pub fn executable(&self, entry: &str, revision: &str) -> Option<ProgramExecutable> {
        self.entries
            .contains_key(&(entry.to_owned(), revision.to_owned()))
            .then(|| ProgramExecutable::Native {
                entry: entry.to_owned(),
                revision: revision.to_owned(),
                binary_digest: self.binary_digest,
            })
    }

    pub(crate) fn resolve(&self, executable: &ProgramExecutable) -> Option<NativeEntry> {
        match executable {
            ProgramExecutable::Native {
                entry,
                revision,
                binary_digest,
            } if *binary_digest == self.binary_digest => self
                .entries
                .get(&(entry.clone(), revision.clone()))
                .copied(),
            _ => None,
        }
    }
}

#[cfg(target_os = "linux")]
fn running_image() -> std::io::Result<std::fs::File> {
    // Opening this kernel handle retains the executing inode after replacement
    // or unlink; resolving its pathname first would lose that identity.
    std::fs::File::open("/proc/self/exe")
}

#[cfg(not(target_os = "linux"))]
fn running_image() -> std::io::Result<std::fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "native executable pinning requires a running-image handle",
    ))
}

fn start<P: NativeProgram>(
    context: WorkflowContext,
    bytes: &[u8],
) -> Result<NativeFuture, NativeProgramError> {
    let input = P::Input::decode(bytes)?;
    let mut completion = WorkflowContext {
        sender: context.sender.clone(),
    };
    Ok(Box::pin(async move {
        let output = P::run(context, input).await?;
        completion
            .request(RequestKind::Terminal(InlineFramePayload::new(
                output.encode()?,
            )))
            .await?;
        Ok(())
    }))
}

/// Program-side requests only; the host retains all external service handles.
pub struct WorkflowContext {
    sender: mpsc::UnboundedSender<HostRequest>,
}

impl WorkflowContext {
    pub async fn now(
        &mut self,
        payload: InlineFramePayload,
    ) -> Result<InlineFramePayload, NativeProgramError> {
        self.request(RequestKind::Now(payload)).await
    }
    pub async fn random(
        &mut self,
        payload: InlineFramePayload,
    ) -> Result<InlineFramePayload, NativeProgramError> {
        self.request(RequestKind::Random(payload)).await
    }
    pub async fn sleep(
        &mut self,
        payload: InlineFramePayload,
    ) -> Result<InlineFramePayload, NativeProgramError> {
        self.request(RequestKind::Sleep(payload)).await
    }
    pub async fn await_event(
        &mut self,
        payload: InlineFramePayload,
    ) -> Result<InlineFramePayload, NativeProgramError> {
        self.request(RequestKind::AwaitEvent(payload)).await
    }
    pub async fn effect(
        &mut self,
        request: EffectRequest,
    ) -> Result<InlineFramePayload, NativeProgramError> {
        self.request(RequestKind::Effect(request)).await
    }
    async fn request(
        &mut self,
        kind: RequestKind,
    ) -> Result<InlineFramePayload, NativeProgramError> {
        let (reply, delivery) = oneshot::channel();
        self.sender
            .send(HostRequest { kind, reply })
            .map_err(|_| NativeProgramError::new("host request channel closed"))?;
        match delivery
            .await
            .map_err(|_| NativeProgramError::new("host delivery channel closed"))?
        {
            DeliveryKind::Answer { payload, .. } | DeliveryKind::Wake { payload, .. } => {
                Ok(payload)
            }
            DeliveryKind::Reject { reason, .. } => Err(NativeProgramError::new(format!(
                "request rejected: {reason:?}"
            ))),
            DeliveryKind::Cancel { .. } | DeliveryKind::RunCancel(_) => {
                Err(NativeProgramError::new("request cancelled"))
            }
            DeliveryKind::Fault(_) => Err(NativeProgramError::new("program faulted")),
        }
    }
}

impl WorkflowHost {
    #[allow(
        clippy::result_large_err,
        reason = "The host retains its replay fault inline."
    )]
    pub(crate) async fn execute_native_loaded(
        &self,
        run: ProgramRunId,
        journal: ProgramJournal,
        entry: NativeEntry,
        input: &[u8],
        live: &mut impl LiveDeliverySource,
    ) -> Result<ProgramExecutionOutcome, WorkflowHostError> {
        let tail = journal
            .entries()
            .last()
            .map_or(0, |entry| entry.position().as_u64());
        let mut execution = ExecutionState::new(ReplayCursor::new(journal), tail);
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut root = match entry(WorkflowContext { sender }, input) {
            Ok(root) => root,
            Err(error) => {
                return self
                    .native_fault(
                        run,
                        tail,
                        ProgramFault::ProgramError(InlineFramePayload::new(
                            error.to_string().into_bytes(),
                        )),
                    )
                    .await;
            }
        };
        loop {
            let status =
                std::future::poll_fn(|context| Poll::Ready(root.as_mut().poll(context))).await;
            if let Poll::Ready(result) = status {
                let error = result.err().unwrap_or_else(|| {
                    NativeProgramError::new("native root ended without a terminal delivery")
                });
                return self
                    .native_fault(
                        run,
                        execution.durable_tail(),
                        ProgramFault::ProgramError(InlineFramePayload::new(
                            error.to_string().into_bytes(),
                        )),
                    )
                    .await;
            }
            while let Ok(request) = receiver.try_recv() {
                self.accept_request(run, &mut execution, request).await?;
            }
            match execution.cursor.next_instruction() {
                ReplayInstruction::Deliver(delivery) => {
                    if let Some(outcome) = execution.apply_delivery(delivery)? {
                        return Ok(outcome);
                    }
                }
                ReplayInstruction::Live if execution.has_outstanding() => {
                    if let Some(outcome) = self.deliver_live(run, &mut execution, live).await? {
                        return Ok(outcome);
                    }
                }
                ReplayInstruction::Live | ReplayInstruction::AwaitRequest => {
                    return self
                        .native_fault(
                            run,
                            execution.durable_tail(),
                            ProgramFault::ProgramError(InlineFramePayload::new(
                                b"native root stalled without a context request".as_slice(),
                            )),
                        )
                        .await;
                }
            }
        }
    }

    #[allow(
        clippy::result_large_err,
        reason = "The host retains its replay fault inline."
    )]
    pub(crate) async fn native_fault(
        &self,
        run: ProgramRunId,
        tail: u64,
        fault: ProgramFault,
    ) -> Result<ProgramExecutionOutcome, WorkflowHostError> {
        self.journal
            .append_delivery_if_tail(run, tail, DeliveryKind::Fault(fault))
            .await?;
        self.journal
            .load(run)
            .await?
            .and_then(|journal| crate::journal_outcome(&journal))
            .ok_or_else(|| WorkflowHostProtocolError::JournalTailChanged.into())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{fs, path::Path, process::Command};

    use super::*;

    #[test]
    fn catalog_digest_survives_executable_path_replacement() -> Result<(), Box<dyn Error>> {
        in_disposable_executable(|executable| {
            let executing_bytes = fs::read(executable)?;
            let replacement = executable.with_extension("replacement");
            const REPLACEMENT_BYTES: &[u8] = b"different executable contents";
            fs::write(&replacement, REPLACEMENT_BYTES)?;
            fs::rename(&replacement, executable)?;

            let catalog = NativeCatalog::new()?;
            assert_eq!(
                catalog.binary_digest,
                ProgramContentDigest::of(&executing_bytes),
                "the first catalog must pin the executing image after path replacement"
            );

            fs::remove_file(executable)?;
            assert_eq!(
                NativeCatalog::new()?.binary_digest,
                catalog.binary_digest,
                "subsequent catalogs must retain the same executing-image digest"
            );
            Ok(())
        })
    }

    /// Reexecutes only this test in a disposable copy before touching its path.
    fn in_disposable_executable(
        check: impl FnOnce(&Path) -> Result<(), Box<dyn Error>>,
    ) -> Result<(), Box<dyn Error>> {
        const CHILD_EXECUTABLE: &str = "SIGNALBOX_NATIVE_REPLACEMENT_EXECUTABLE";
        if let Some(executable) = std::env::var_os(CHILD_EXECUTABLE) {
            return check(Path::new(&executable));
        }
        let directory = std::env::temp_dir().join(format!(
            "signalbox-native-executable-replacement-{}",
            std::process::id()
        ));
        fs::create_dir(&directory)?;
        let executable = directory.join("test-executable");
        fs::copy(std::env::current_exe()?, &executable)?;
        let output = Command::new(&executable)
            .args([
                "--exact",
                "native::tests::catalog_digest_survives_executable_path_replacement",
                "--nocapture",
            ])
            .env(CHILD_EXECUTABLE, &executable)
            .output();
        fs::remove_dir_all(directory)?;
        let output = output?;
        assert!(
            output.status.success(),
            "replacement subprocess failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
}
