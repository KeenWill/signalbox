//! JavaScript and trusted native hosts for journaled Signalbox programs.
//!
//! Registered runs resolve their executable and grants from durable registration.
//! Host-side executors answer granted effects; replay uses the checked journal.

pub mod effects;
pub mod native;
pub mod primitives;
pub mod session_effects;

#[cfg(test)]
mod sdk_tests;

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    error::Error,
    fmt,
    future::{Future, poll_fn},
    pin::Pin,
    rc::Rc,
    sync::{Arc, Mutex, Weak},
    task::Poll,
};

use deno_core::{
    JsRuntime, ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse, ModuleLoader,
    ModuleResolveResponse, ModuleSpecifier, OpState, PollEventLoopOptions, ResolutionKind,
    RuntimeOptions, op2,
};
use deno_error::JsErrorBox;
use serde::{Deserialize, Serialize};
use signalbox_domain::{
    DeliveryFrame, DeliveryKind, EffectRequest, InlineFramePayload, NondeterminismError,
    ProgramFault, ProgramJournal, ProgramRunId, RejectReason, ReplayCursor, ReplayInstruction,
    ReplayedRequest, RequestFrame, RequestKind, RequestOrdinal,
};
use signalbox_persistence::program_journal::{
    ProgramJournalRepository, ProgramJournalRepositoryError,
};
use signalbox_persistence::program_registration::ProgramRegistrationError;
use tokio::sync::{mpsc, oneshot};

/// Canonical module specifier exposed to frame-contract-v1 artifacts.
pub const PROGRAM_SDK_V1_SPECIFIER: &str = "@signalbox/program-sdk/v1";

const PROGRAM_SDK_INTERNAL_SPECIFIER: &str = "signalbox:program-sdk/v1";
const PROGRAM_SDK_PRELOAD_SPECIFIER: &str = "signalbox:program/sdk-preload";
const PROGRAM_ENTRYPOINT_SPECIFIER: &str = "signalbox:program/entrypoint";
const PROGRAM_MAIN_SPECIFIER: &str = "signalbox:program/main";

deno_core::extension!(
    signalbox_program_sdk_v1,
    ops = [op_program_request],
    lazy_loaded_js = [dir "src", "program_sdk_v1.js"],
    synthetic_esm = [
        "signalbox:program-sdk/v1" = "ext:signalbox_program_sdk_v1/program_sdk_v1.js"
    ],
    js = [dir "src", "isolate_bootstrap.js"],
);

/// One already-stripped JavaScript artifact supplied by the registration layer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramArtifact(Box<str>);

impl ProgramArtifact {
    pub fn new(source: impl Into<Box<str>>) -> Self {
        Self(source.into())
    }

    pub fn source(&self) -> &str {
        &self.0
    }
}

/// A live request needs one durable delivery before execution can continue.
pub trait LiveDeliverySource {
    fn next_delivery<'a>(
        &'a mut self,
        outstanding: &'a [RequestFrame],
    ) -> Pin<Box<dyn Future<Output = Result<DeliveryKind, LiveDeliveryFailure>> + 'a>>;
}

/// The caller-provided live-delivery source could not produce a delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveDeliveryFailure(Box<str>);

impl LiveDeliveryFailure {
    pub fn new(message: impl Into<Box<str>>) -> Self {
        Self(message.into())
    }

    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LiveDeliveryFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for LiveDeliveryFailure {}

/// Terminal observation made by this execution attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProgramExecutionOutcome {
    /// Exact result bytes from the accepted terminal request.
    Completed(InlineFramePayload),
    RunCancelled(InlineFramePayload),
    Faulted(ProgramFault),
}

/// A program execution attempt failed before producing an outcome.
#[derive(Debug)]
pub enum WorkflowHostError {
    Journal(ProgramJournalRepositoryError),
    Registration(ProgramRegistrationError),
    JournalMissing(ProgramRunId),
    Isolate(deno_core::error::CoreError),
    LiveDelivery(LiveDeliveryFailure),
    Nondeterminism {
        expected: Box<RequestFrame>,
        observed: Box<RequestFrame>,
        fault: DeliveryFrame,
    },
    Protocol(WorkflowHostProtocolError),
}

impl fmt::Display for WorkflowHostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registration(error) => write!(formatter, "program registration failed: {error}"),
            Self::Journal(error) => write!(formatter, "program journal failed: {error}"),
            Self::JournalMissing(run) => {
                write!(
                    formatter,
                    "program journal is missing for {:?}",
                    run.into_uuid()
                )
            }
            Self::Isolate(error) => write!(formatter, "program isolate failed: {error}"),
            Self::LiveDelivery(error) => write!(formatter, "live delivery failed: {error}"),
            Self::Nondeterminism {
                expected, observed, ..
            } => write!(
                formatter,
                "program request diverged: expected {expected:?}, observed {observed:?}"
            ),
            Self::Protocol(error) => write!(formatter, "program host protocol failed: {error}"),
        }
    }
}

impl Error for WorkflowHostError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Registration(error) => Some(error),
            Self::Journal(error) => Some(error),
            Self::Isolate(error) => Some(error),
            Self::LiveDelivery(error) => Some(error),
            Self::Protocol(error) => Some(error),
            Self::JournalMissing(_) | Self::Nondeterminism { .. } => None,
        }
    }
}

impl From<ProgramRegistrationError> for WorkflowHostError {
    fn from(error: ProgramRegistrationError) -> Self {
        Self::Registration(error)
    }
}

impl From<ProgramJournalRepositoryError> for WorkflowHostError {
    fn from(error: ProgramJournalRepositoryError) -> Self {
        Self::Journal(error)
    }
}

impl From<deno_core::error::CoreError> for WorkflowHostError {
    fn from(error: deno_core::error::CoreError) -> Self {
        Self::Isolate(error)
    }
}

impl From<LiveDeliveryFailure> for WorkflowHostError {
    fn from(error: LiveDeliveryFailure) -> Self {
        Self::LiveDelivery(error)
    }
}

impl From<WorkflowHostProtocolError> for WorkflowHostError {
    fn from(error: WorkflowHostProtocolError) -> Self {
        Self::Protocol(error)
    }
}

/// An invariant between the program adapter, replay cursor, and delivery source failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkflowHostProtocolError {
    RequestOrdinalExhausted,
    DeliveryPending,
    DuplicateOutstandingRequest,
    UnknownResolvedRequest,
    DeliveryReceiverClosed,
    IsolateRequestChannelClosed,
    LiveRequestWasNotAppendedExactly,
    JournalTailChanged,
    JournalPositionExhausted,
    Stalled,
}

impl fmt::Display for WorkflowHostProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::RequestOrdinalExhausted => "request ordinal exhausted",
            Self::DeliveryPending => "isolate emitted a request before a recorded delivery",
            Self::DuplicateOutstandingRequest => "request ordinal is already outstanding",
            Self::UnknownResolvedRequest => "delivery resolves no outstanding isolate request",
            Self::DeliveryReceiverClosed => "isolate dropped an outstanding request promise",
            Self::IsolateRequestChannelClosed => "isolate request channel closed",
            Self::LiveRequestWasNotAppendedExactly => {
                "durable append changed the live request frame"
            }
            Self::JournalTailChanged => "program journal tail changed during execution",
            Self::JournalPositionExhausted => "journal position exhausted",
            Self::Stalled => "isolate is pending with no request the host can advance",
        };
        formatter.write_str(message)
    }
}

impl Error for WorkflowHostProtocolError {}

/// Executes JavaScript modules and compiled native programs against one durable journal.
#[derive(Clone, Debug)]
pub struct WorkflowHost {
    journal: ProgramJournalRepository,
    native_catalog: Option<Arc<native::NativeCatalog>>,
    interrupts: Arc<Mutex<IsolateInterrupts>>,
}

#[derive(Debug, Default)]
struct IsolateInterrupts {
    stopped: bool,
    handles: Vec<Weak<deno_core::v8::IsolateHandle>>,
}

impl WorkflowHost {
    pub fn new(journal: ProgramJournalRepository) -> Self {
        Self {
            journal,
            native_catalog: None,
            interrupts: Arc::default(),
        }
    }

    /// Permanently interrupts JavaScript execution on this host and its clones.
    /// May be called from another thread, including while an artifact does not yield.
    pub fn interrupt(&self) {
        let mut interrupts = self
            .interrupts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        interrupts.stopped = true;
        for handle in interrupts.handles.iter().filter_map(Weak::upgrade) {
            handle.terminate_execution();
        }
    }

    /// Whether this host's JavaScript execution has been interrupted for shutdown.
    pub fn is_interrupted(&self) -> bool {
        self.interrupts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stopped
    }

    /// Supplies the compiled entries available in this executing binary.
    pub fn with_native_catalog(mut self, catalog: native::NativeCatalog) -> Self {
        self.native_catalog = Some(Arc::new(catalog));
        self
    }

    /// Verifies the retained run before fixing host-side session input attribution.
    pub async fn session_capability(
        &self,
        run: ProgramRunId,
    ) -> Result<
        Option<signalbox_persistence::program_journal::ProgramSessionCapability>,
        signalbox_persistence::program_journal::ProgramSessionCapabilityError,
    > {
        signalbox_persistence::program_journal::ProgramSessionHost::new(self.journal.clone())
            .session_capability(run)
            .await
    }

    /// Executes isolate fixtures without a registration.
    #[cfg(feature = "postgres-integration")]
    #[allow(
        clippy::result_large_err,
        reason = "The host retains its replay fault inline."
    )]
    pub async fn execute_unregistered(
        &self,
        run: ProgramRunId,
        artifact: &ProgramArtifact,
        live_deliveries: &mut impl LiveDeliverySource,
    ) -> Result<ProgramExecutionOutcome, WorkflowHostError> {
        let journal = self
            .journal
            .load(run)
            .await?
            .ok_or(WorkflowHostError::JournalMissing(run))?;
        self.execute_loaded(
            run,
            journal,
            artifact,
            &[],
            &mut effects::NoEffects(live_deliveries),
        )
        .await
    }

    #[allow(
        clippy::result_large_err,
        reason = "The host retains its replay fault inline."
    )]
    async fn execute_loaded(
        &self,
        run: ProgramRunId,
        journal: ProgramJournal,
        artifact: &ProgramArtifact,
        input: &[u8],
        live_deliveries: &mut impl LiveDeliverySource,
    ) -> Result<ProgramExecutionOutcome, WorkflowHostError> {
        if let Some(outcome) = journal_outcome(&journal) {
            return Ok(outcome);
        }
        let durable_tail = journal
            .entries()
            .last()
            .map_or(0, |entry| entry.position().as_u64());
        let mut execution = ExecutionState::new(ReplayCursor::new(journal), durable_tail);

        let (request_sender, mut request_receiver) = mpsc::unbounded_channel();
        let (mut runtime, module_loader) = isolate(request_sender)?;
        // Publish before user code; the stop flag also covers concurrent creation.
        let _interrupt = {
            let handle = Arc::new(runtime.v8_isolate().thread_safe_handle());
            let mut interrupts = self
                .interrupts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            interrupts
                .handles
                .retain(|handle| handle.strong_count() > 0);
            if interrupts.stopped {
                handle.terminate_execution();
            }
            interrupts.handles.push(Arc::downgrade(&handle));
            handle
        };
        let sdk_specifier = ModuleSpecifier::parse(PROGRAM_SDK_PRELOAD_SPECIFIER)
            .map_err(JsErrorBox::from_err)
            .map_err(deno_core::error::CoreError::from)?;
        module_loader.host_module_admitted.set(true);
        let sdk_module = runtime
            .load_side_es_module_from_code(
                &sdk_specifier,
                format!(
                    "import {PROGRAM_SDK_V1_SPECIFIER:?};\nconst input = new Uint8Array({input:?});\n{}",
                    include_str!("program_entrypoint.js"),
                ),
            )
            .await?;
        module_loader.host_module_admitted.set(false);
        let sdk_evaluation = runtime.mod_evaluate(sdk_module);
        runtime
            .run_event_loop(PollEventLoopOptions::default())
            .await?;
        sdk_evaluation.await?;
        let main_specifier = ModuleSpecifier::parse(PROGRAM_MAIN_SPECIFIER)
            .map_err(JsErrorBox::from_err)
            .map_err(deno_core::error::CoreError::from)?;
        runtime
            .load_main_es_module_from_code(&main_specifier, artifact.source().to_owned())
            .await?;
        let entrypoint_specifier = ModuleSpecifier::parse(PROGRAM_ENTRYPOINT_SPECIFIER)
            .map_err(JsErrorBox::from_err)
            .map_err(deno_core::error::CoreError::from)?;
        module_loader.host_module_admitted.set(true);
        let module = runtime
            .load_side_es_module_from_code(
                &entrypoint_specifier,
                format!(
                    "import invoke from {PROGRAM_SDK_PRELOAD_SPECIFIER:?};\n\
                     import * as program from {PROGRAM_MAIN_SPECIFIER:?};\n\
                     export default await invoke(program);"
                ),
            )
            .await?;
        module_loader.host_module_admitted.set(false);
        let mut evaluation = Box::pin(runtime.mod_evaluate(module));
        let mut completed_evaluation = None;

        loop {
            let runtime_status = poll_runtime_once(&mut runtime).await;
            poll_evaluation_once(&mut evaluation, &mut completed_evaluation).await;
            // A module that throws reports the exception through the event loop
            // while its `mod_evaluate` future still fulfills with `Ok`, so the
            // engine result is the only record that the artifact failed. Take
            // it before any path below reads that fulfilled evaluation and
            // calls the attempt complete. A ready engine error with no
            // fulfilled evaluation is left to those paths, which name a
            // never-resolved top-level await `Stalled` rather than an engine
            // failure.
            let runtime_status = match runtime_status {
                Poll::Ready(Err(error)) if completed_evaluation.is_some() => {
                    return Err(error.into());
                }
                status => status,
            };
            while let Ok(request) = request_receiver.try_recv() {
                self.accept_request(run, &mut execution, request).await?;
            }

            let at_live_tail = match execution.cursor.next_instruction() {
                ReplayInstruction::Deliver(delivery) => {
                    if let Some(outcome) = execution.apply_delivery(delivery)? {
                        return Ok(outcome);
                    }
                    continue;
                }
                ReplayInstruction::Live => {
                    let had_outstanding = execution.has_outstanding();
                    if let Some(outcome) = self
                        .deliver_live(run, &mut execution, live_deliveries)
                        .await?
                    {
                        return Ok(outcome);
                    }
                    if had_outstanding {
                        continue;
                    }
                    if let Some(result) = completed_evaluation.take() {
                        result?;
                        let result = entrypoint_result(&mut runtime, module)?;
                        return self.complete(run, execution.durable_tail(), result).await;
                    }
                    true
                }
                ReplayInstruction::AwaitRequest => {
                    if let Some(result) = completed_evaluation.take() {
                        result?;
                        return Err(WorkflowHostProtocolError::Stalled.into());
                    }
                    false
                }
            };

            match runtime_status {
                Poll::Ready(result) => {
                    if completed_evaluation.is_none() {
                        return Err(WorkflowHostProtocolError::Stalled.into());
                    }
                    result?;
                    let Some(result) = completed_evaluation.take() else {
                        return Err(WorkflowHostProtocolError::Stalled.into());
                    };
                    result?;
                    if at_live_tail {
                        let result = entrypoint_result(&mut runtime, module)?;
                        return self.complete(run, execution.durable_tail(), result).await;
                    }
                    return Err(WorkflowHostProtocolError::Stalled.into());
                }
                Poll::Pending => {
                    tokio::select! {
                        result = runtime.run_event_loop(PollEventLoopOptions::default()) => {
                            poll_evaluation_once(
                                &mut evaluation,
                                &mut completed_evaluation,
                            ).await;
                            if completed_evaluation.is_none() {
                                return Err(WorkflowHostProtocolError::Stalled.into());
                            }
                            result?;
                            let Some(result) = completed_evaluation.take() else {
                                return Err(WorkflowHostProtocolError::Stalled.into());
                            };
                            result?;
                            if at_live_tail {
                                let result = entrypoint_result(&mut runtime, module)?;
                                return self.complete(run, execution.durable_tail(), result).await;
                            }
                            return Err(WorkflowHostProtocolError::Stalled.into());
                        }
                        request = request_receiver.recv() => {
                            let request = request.ok_or(
                                WorkflowHostProtocolError::IsolateRequestChannelClosed,
                            )?;
                            self.accept_request(run, &mut execution, request).await?;
                            continue;
                        }
                    }
                }
            }
        }
    }

    #[allow(
        clippy::result_large_err,
        reason = "The host retains its replay fault inline."
    )]
    async fn complete(
        &self,
        run: ProgramRunId,
        durable_tail: u64,
        result: InlineFramePayload,
    ) -> Result<ProgramExecutionOutcome, WorkflowHostError> {
        self.journal
            .complete_if_tail(run, durable_tail, result)
            .await?;
        let journal = self
            .journal
            .load(run)
            .await?
            .ok_or(WorkflowHostError::JournalMissing(run))?;
        journal_outcome(&journal)
            .ok_or_else(|| WorkflowHostProtocolError::JournalTailChanged.into())
    }

    #[allow(
        clippy::result_large_err,
        reason = "The host retains its replay fault inline."
    )]
    async fn accept_request(
        &self,
        run: ProgramRunId,
        execution: &mut ExecutionState,
        request: HostRequest,
    ) -> Result<(), WorkflowHostError> {
        let frame = execution.frame_for(request.kind)?;
        match execution.cursor.submit_request(frame.clone()) {
            Ok(ReplayedRequest::Matched) => execution.insert_pending(frame, request.reply)?,
            Ok(ReplayedRequest::Live) => {
                let persisted = self
                    .journal
                    .append_request_if_tail(
                        run,
                        execution.durable_tail(),
                        frame.scope(),
                        frame.kind().clone(),
                    )
                    .await?
                    .ok_or(WorkflowHostProtocolError::JournalTailChanged)?;
                if persisted != frame {
                    return Err(WorkflowHostProtocolError::LiveRequestWasNotAppendedExactly.into());
                }
                execution.advance_durable_tail()?;
                execution.insert_pending(persisted, request.reply)?;
            }
            Ok(ReplayedRequest::DeliveryPending) => {
                return Err(WorkflowHostProtocolError::DeliveryPending.into());
            }
            Err(divergence) => {
                return Err(self
                    .persist_divergence(divergence, execution.durable_tail())
                    .await?);
            }
        }
        Ok(())
    }

    #[allow(
        clippy::result_large_err,
        reason = "The host retains its replay fault inline."
    )]
    async fn persist_divergence(
        &self,
        divergence: NondeterminismError,
        durable_tail: u64,
    ) -> Result<WorkflowHostError, WorkflowHostError> {
        let expected = Box::new(divergence.expected().clone());
        let observed = Box::new(divergence.observed().clone());
        let fault = self
            .journal
            .append_nondeterminism_fault_if_tail(divergence, durable_tail)
            .await?
            .ok_or(WorkflowHostProtocolError::JournalTailChanged)?;
        Ok(WorkflowHostError::Nondeterminism {
            expected,
            observed,
            fault,
        })
    }

    #[allow(
        clippy::result_large_err,
        reason = "The host retains its replay fault inline."
    )]
    async fn deliver_live(
        &self,
        run: ProgramRunId,
        execution: &mut ExecutionState,
        live_deliveries: &mut impl LiveDeliverySource,
    ) -> Result<Option<ProgramExecutionOutcome>, WorkflowHostError> {
        let outstanding = execution.outstanding_frames();
        if outstanding.is_empty() {
            return Ok(None);
        }
        let kind = live_deliveries.next_delivery(&outstanding).await?;
        execution.validate_delivery(&kind)?;
        let delivery = self
            .journal
            .append_delivery_if_tail(run, execution.durable_tail(), kind)
            .await?
            .ok_or(WorkflowHostProtocolError::JournalTailChanged)?;
        execution.advance_durable_tail()?;
        execution.apply_delivery(delivery).map_err(Into::into)
    }
}

fn journal_outcome(journal: &ProgramJournal) -> Option<ProgramExecutionOutcome> {
    if let Some(result) = journal.result() {
        return Some(ProgramExecutionOutcome::Completed(result.clone()));
    }
    journal.terminal_delivery().and_then(terminal_outcome)
}

/// A cancellation or fault carried by an unsolicited delivery.
fn terminal_outcome(delivery: &DeliveryFrame) -> Option<ProgramExecutionOutcome> {
    match delivery.kind() {
        DeliveryKind::RunCancel(payload) => {
            Some(ProgramExecutionOutcome::RunCancelled(payload.clone()))
        }
        DeliveryKind::Fault(fault) => Some(ProgramExecutionOutcome::Faulted(fault.clone())),
        DeliveryKind::Answer { .. }
        | DeliveryKind::Wake { .. }
        | DeliveryKind::Reject { .. }
        | DeliveryKind::Cancel { .. } => None,
    }
}

async fn poll_evaluation_once<F>(evaluation: &mut Pin<Box<F>>, completed: &mut Option<F::Output>)
where
    F: Future,
{
    if completed.is_none() {
        let result = poll_fn(|context| Poll::Ready(evaluation.as_mut().poll(context))).await;
        if let Poll::Ready(result) = result {
            *completed = Some(result);
        }
    }
}

async fn poll_runtime_once(
    runtime: &mut JsRuntime,
) -> Poll<Result<(), deno_core::error::CoreError>> {
    poll_fn(|context| {
        Poll::Ready(runtime.poll_event_loop(context, PollEventLoopOptions::default()))
    })
    .await
}

fn entrypoint_result(
    runtime: &mut JsRuntime,
    module: deno_core::ModuleId,
) -> Result<InlineFramePayload, deno_core::error::CoreError> {
    let namespace = runtime.get_module_namespace(module)?;
    deno_core::scope!(scope, runtime);
    let namespace = deno_core::v8::Local::new(scope, namespace);
    let name = deno_core::v8::String::new(scope, "default")
        .ok_or_else(|| JsErrorBox::generic("entrypoint export name allocation failed"))?;
    let value = namespace
        .get(scope, name.into())
        .ok_or_else(|| JsErrorBox::generic("entrypoint result is missing"))?;
    let value = deno_core::v8::Local::<deno_core::v8::Uint8Array>::try_from(value)
        .map_err(|_| JsErrorBox::type_error("program entrypoint must return a Uint8Array"))?;
    let mut bytes = vec![0; value.byte_length()];
    value.copy_contents(&mut bytes);
    Ok(InlineFramePayload::new(bytes))
}

fn isolate(
    sender: mpsc::UnboundedSender<HostRequest>,
) -> Result<(JsRuntime, Rc<ProgramModuleLoader>), deno_core::error::CoreError> {
    let module_loader = Rc::new(ProgramModuleLoader {
        host_module_admitted: Cell::new(false),
    });
    let runtime = JsRuntime::new(RuntimeOptions {
        module_loader: Some(module_loader.clone()),
        extensions: vec![signalbox_program_sdk_v1::init()],
        ..Default::default()
    });
    runtime
        .op_state()
        .borrow_mut()
        .put(HostRequestSender(sender));
    Ok((runtime, module_loader))
}

#[derive(Clone)]
struct HostRequestSender(mpsc::UnboundedSender<HostRequest>);

struct HostRequest {
    kind: RequestKind,
    reply: oneshot::Sender<DeliveryKind>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum IsolateRequestKind {
    Now {
        payload: Vec<u8>,
    },
    Random {
        payload: Vec<u8>,
    },
    Sleep {
        payload: Vec<u8>,
    },
    AwaitEvent {
        payload: Vec<u8>,
    },
    Effect {
        capability: effects::IsolateCapability,
        method: String,
        payload: Vec<u8>,
    },
}

impl IsolateRequestKind {
    fn into_domain(self) -> RequestKind {
        match self {
            Self::Effect {
                capability,
                method,
                payload,
            } => RequestKind::Effect(EffectRequest::new(
                capability.into(),
                method,
                InlineFramePayload::new(payload),
            )),
            Self::Now { payload } => RequestKind::Now(InlineFramePayload::new(payload)),
            Self::Random { payload } => RequestKind::Random(InlineFramePayload::new(payload)),
            Self::Sleep { payload } => RequestKind::Sleep(InlineFramePayload::new(payload)),
            Self::AwaitEvent { payload } => {
                RequestKind::AwaitEvent(InlineFramePayload::new(payload))
            }
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum IsolateDelivery {
    Answer { payload: Vec<u8> },
    Wake { payload: Vec<u8> },
    Reject { reason: IsolateRejectReason },
    Cancel { payload: Vec<u8> },
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum IsolateRejectReason {
    OutstandingRequests,
    CapabilityDenied,
    UnsupportedOperation,
}

#[op2]
#[serde]
async fn op_program_request(
    state: Rc<RefCell<OpState>>,
    #[serde] request: IsolateRequestKind,
) -> Result<IsolateDelivery, JsErrorBox> {
    let sender = {
        let state = state.borrow();
        state.borrow::<HostRequestSender>().0.clone()
    };
    let (reply, delivery) = oneshot::channel();
    sender
        .send(HostRequest {
            kind: request.into_domain(),
            reply,
        })
        .map_err(|_| JsErrorBox::generic("program host request channel closed"))?;
    let delivery = delivery
        .await
        .map_err(|_| JsErrorBox::generic("program host delivery channel closed"))?;
    match delivery {
        DeliveryKind::Answer { payload, .. } => Ok(IsolateDelivery::Answer {
            payload: payload.as_bytes().to_vec(),
        }),
        DeliveryKind::Wake { payload, .. } => Ok(IsolateDelivery::Wake {
            payload: payload.as_bytes().to_vec(),
        }),
        DeliveryKind::Cancel { payload, .. } => Ok(IsolateDelivery::Cancel {
            payload: payload.as_bytes().to_vec(),
        }),
        DeliveryKind::Reject { reason, .. } => Ok(IsolateDelivery::Reject {
            reason: match reason {
                RejectReason::OutstandingRequests => IsolateRejectReason::OutstandingRequests,
                RejectReason::CapabilityDenied => IsolateRejectReason::CapabilityDenied,
                RejectReason::UnsupportedOperation => IsolateRejectReason::UnsupportedOperation,
            },
        }),
        DeliveryKind::RunCancel(_) | DeliveryKind::Fault(_) => {
            Err(JsErrorBox::generic("program ended"))
        }
    }
}

struct PendingRequest {
    frame: RequestFrame,
    reply: oneshot::Sender<DeliveryKind>,
}

struct ExecutionState {
    cursor: ReplayCursor,
    durable_tail: u64,
    next_request_ordinal: u64,
    pending: BTreeMap<RequestOrdinal, PendingRequest>,
}

impl ExecutionState {
    fn new(cursor: ReplayCursor, durable_tail: u64) -> Self {
        Self {
            cursor,
            durable_tail,
            next_request_ordinal: 1,
            pending: BTreeMap::new(),
        }
    }

    const fn durable_tail(&self) -> u64 {
        self.durable_tail
    }

    fn advance_durable_tail(&mut self) -> Result<(), WorkflowHostProtocolError> {
        self.durable_tail = self
            .durable_tail
            .checked_add(1)
            .ok_or(WorkflowHostProtocolError::JournalPositionExhausted)?;
        Ok(())
    }

    fn frame_for(&mut self, kind: RequestKind) -> Result<RequestFrame, WorkflowHostProtocolError> {
        let ordinal = RequestOrdinal::try_from_u64(self.next_request_ordinal)
            .ok_or(WorkflowHostProtocolError::RequestOrdinalExhausted)?;
        self.next_request_ordinal = self
            .next_request_ordinal
            .checked_add(1)
            .ok_or(WorkflowHostProtocolError::RequestOrdinalExhausted)?;
        Ok(RequestFrame::new(ordinal, None, kind))
    }

    fn insert_pending(
        &mut self,
        frame: RequestFrame,
        reply: oneshot::Sender<DeliveryKind>,
    ) -> Result<(), WorkflowHostProtocolError> {
        let ordinal = frame.ordinal();
        if self
            .pending
            .insert(ordinal, PendingRequest { frame, reply })
            .is_some()
        {
            return Err(WorkflowHostProtocolError::DuplicateOutstandingRequest);
        }
        Ok(())
    }

    fn has_outstanding(&self) -> bool {
        !self.pending.is_empty()
    }

    fn outstanding_frames(&self) -> Vec<RequestFrame> {
        self.pending
            .values()
            .map(|request| request.frame.clone())
            .collect()
    }

    fn validate_delivery(&self, kind: &DeliveryKind) -> Result<(), WorkflowHostProtocolError> {
        match kind.resolves() {
            Some(ordinal) if !self.pending.contains_key(&ordinal) => {
                Err(WorkflowHostProtocolError::UnknownResolvedRequest)
            }
            Some(_) | None => Ok(()),
        }
    }

    fn apply_delivery(
        &mut self,
        delivery: DeliveryFrame,
    ) -> Result<Option<ProgramExecutionOutcome>, WorkflowHostProtocolError> {
        if let Some(outcome) = terminal_outcome(&delivery) {
            return Ok(Some(outcome));
        }
        let ordinal = delivery
            .kind()
            .resolves()
            .ok_or(WorkflowHostProtocolError::UnknownResolvedRequest)?;
        let pending = self
            .pending
            .remove(&ordinal)
            .ok_or(WorkflowHostProtocolError::UnknownResolvedRequest)?;
        let outcome = match (pending.frame.kind(), delivery.kind()) {
            (RequestKind::Terminal(result), DeliveryKind::Answer { .. }) => {
                Some(ProgramExecutionOutcome::Completed(result.clone()))
            }
            _ => None,
        };
        pending
            .reply
            .send(delivery.kind().clone())
            .map_err(|_| WorkflowHostProtocolError::DeliveryReceiverClosed)?;
        Ok(outcome)
    }
}

struct ProgramModuleLoader {
    host_module_admitted: Cell<bool>,
}

impl ModuleLoader for ProgramModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        kind: ResolutionKind,
    ) -> ModuleResolveResponse {
        if matches!(kind, ResolutionKind::MainModule) && specifier == PROGRAM_MAIN_SPECIFIER {
            return ModuleSpecifier::parse(PROGRAM_MAIN_SPECIFIER).map_err(JsErrorBox::from_err);
        }
        if self.host_module_admitted.get()
            && ((referrer == "."
                && matches!(
                    specifier,
                    PROGRAM_SDK_PRELOAD_SPECIFIER | PROGRAM_ENTRYPOINT_SPECIFIER
                ))
                || (referrer == PROGRAM_ENTRYPOINT_SPECIFIER
                    && matches!(
                        specifier,
                        PROGRAM_SDK_PRELOAD_SPECIFIER | PROGRAM_MAIN_SPECIFIER
                    )))
        {
            return ModuleSpecifier::parse(specifier).map_err(JsErrorBox::from_err);
        }
        if specifier == PROGRAM_SDK_V1_SPECIFIER {
            return ModuleSpecifier::parse(PROGRAM_SDK_INTERNAL_SPECIFIER)
                .map_err(JsErrorBox::from_err);
        }
        Err(JsErrorBox::generic(format!(
            "program import is not admitted: {specifier}"
        )))
    }

    fn load(
        &self,
        _module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<&ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        ModuleLoadResponse::Sync(Err(JsErrorBox::generic(
            "program module loader has no external sources",
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use deno_core::{ModuleLoader, ResolutionKind};

    use super::{PROGRAM_MAIN_SPECIFIER, PROGRAM_SDK_INTERNAL_SPECIFIER, ProgramModuleLoader};

    #[test]
    fn loader_rejects_a_relative_artifact_import() {
        let error = ProgramModuleLoader {
            host_module_admitted: Cell::new(false),
        }
        .resolve("./other.js", PROGRAM_MAIN_SPECIFIER, ResolutionKind::Import)
        .expect_err("relative imports are outside the program artifact contract");

        assert_eq!(
            error.to_string(),
            "program import is not admitted: ./other.js"
        );
    }

    #[test]
    fn loader_maps_only_the_canonical_sdk_import() {
        let resolved = ProgramModuleLoader {
            host_module_admitted: Cell::new(false),
        }
        .resolve(
            super::PROGRAM_SDK_V1_SPECIFIER,
            PROGRAM_MAIN_SPECIFIER,
            ResolutionKind::Import,
        )
        .expect("the canonical SDK import is admitted");

        assert_eq!(resolved.as_str(), PROGRAM_SDK_INTERNAL_SPECIFIER);
    }

    #[test]
    fn artifact_cannot_import_private_host_modules_during_preload() {
        let loader = ProgramModuleLoader {
            host_module_admitted: Cell::new(true),
        };
        for specifier in [
            PROGRAM_MAIN_SPECIFIER,
            super::PROGRAM_SDK_PRELOAD_SPECIFIER,
            super::PROGRAM_ENTRYPOINT_SPECIFIER,
        ] {
            assert!(
                loader
                    .resolve(specifier, PROGRAM_MAIN_SPECIFIER, ResolutionKind::Import)
                    .is_err()
            );
        }
    }
}
