//! JavaScript isolate host for journaled Signalbox programs.
//!
//! The host deliberately owns no effect implementation. A caller supplies a
//! [`LiveDeliverySource`] that can answer already-durable live requests; replay
//! deliveries bypass that source and come only from the checked journal.

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    error::Error,
    fmt,
    future::{Future, poll_fn},
    pin::Pin,
    rc::Rc,
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
    DeliveryFrame, DeliveryKind, InlineFramePayload, NondeterminismError, ProgramFault,
    ProgramJournal, ProgramRunId, RejectReason, ReplayCursor, ReplayInstruction, ReplayedRequest,
    RequestFrame, RequestKind, RequestOrdinal,
};
use signalbox_persistence::program_journal::{
    ProgramJournalRepository, ProgramJournalRepositoryError,
};
use tokio::sync::{mpsc, oneshot};

/// Canonical module specifier exposed to frame-contract-v1 artifacts.
pub const PROGRAM_SDK_V1_SPECIFIER: &str = "@signalbox/program-sdk/v1";

const PROGRAM_SDK_INTERNAL_SPECIFIER: &str = "signalbox:program-sdk/v1";
const PROGRAM_SDK_PRELOAD_SPECIFIER: &str = "signalbox:program/sdk-preload";
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
    /// The module evaluation fulfilled. Durable run terminalization is a later slice.
    Completed,
    RunCancelled(InlineFramePayload),
    Faulted(ProgramFault),
}

/// A program execution attempt failed before producing an outcome.
#[derive(Debug)]
pub enum ProgramHostError {
    Journal(ProgramJournalRepositoryError),
    JournalMissing(ProgramRunId),
    Isolate(deno_core::error::CoreError),
    LiveDelivery(LiveDeliveryFailure),
    Nondeterminism {
        expected: Box<RequestFrame>,
        observed: Box<RequestFrame>,
        fault: DeliveryFrame,
    },
    Protocol(ProgramHostProtocolError),
}

impl fmt::Display for ProgramHostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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

impl Error for ProgramHostError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Journal(error) => Some(error),
            Self::Isolate(error) => Some(error),
            Self::LiveDelivery(error) => Some(error),
            Self::Protocol(error) => Some(error),
            Self::JournalMissing(_) | Self::Nondeterminism { .. } => None,
        }
    }
}

impl From<ProgramJournalRepositoryError> for ProgramHostError {
    fn from(error: ProgramJournalRepositoryError) -> Self {
        Self::Journal(error)
    }
}

impl From<deno_core::error::CoreError> for ProgramHostError {
    fn from(error: deno_core::error::CoreError) -> Self {
        Self::Isolate(error)
    }
}

impl From<LiveDeliveryFailure> for ProgramHostError {
    fn from(error: LiveDeliveryFailure) -> Self {
        Self::LiveDelivery(error)
    }
}

impl From<ProgramHostProtocolError> for ProgramHostError {
    fn from(error: ProgramHostProtocolError) -> Self {
        Self::Protocol(error)
    }
}

/// An invariant between the isolate, replay cursor, and delivery source failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgramHostProtocolError {
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

impl fmt::Display for ProgramHostProtocolError {
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

impl Error for ProgramHostProtocolError {}

/// Executes JavaScript modules against a durable journal and replay cursor.
#[derive(Clone, Debug)]
pub struct ProgramHost {
    journal: ProgramJournalRepository,
}

impl ProgramHost {
    pub const fn new(journal: ProgramJournalRepository) -> Self {
        Self { journal }
    }

    pub async fn execute(
        &self,
        run: ProgramRunId,
        artifact: &ProgramArtifact,
        live_deliveries: &mut impl LiveDeliverySource,
    ) -> Result<ProgramExecutionOutcome, ProgramHostError> {
        let journal = self
            .journal
            .load(run)
            .await?
            .ok_or(ProgramHostError::JournalMissing(run))?;
        self.execute_loaded(run, journal, artifact, live_deliveries)
            .await
    }

    async fn execute_loaded(
        &self,
        run: ProgramRunId,
        journal: ProgramJournal,
        artifact: &ProgramArtifact,
        live_deliveries: &mut impl LiveDeliverySource,
    ) -> Result<ProgramExecutionOutcome, ProgramHostError> {
        // A run that already ended has its outcome in the journal, whatever
        // frames precede the terminal delivery, so this asks before anything
        // about the attempt exists. Replaying to rediscover a recorded outcome
        // would need the artifact, and an artifact that is malformed or imports
        // outside the contract fails the module load below — masking a
        // `run_cancel` or `fault` that is already durable behind an isolate
        // error.
        if let Some(outcome) = journal.terminal_delivery().and_then(terminal_outcome) {
            return Ok(outcome);
        }
        let durable_tail = journal
            .entries()
            .last()
            .map_or(0, |entry| entry.position().as_u64());
        let mut execution = ExecutionState::new(ReplayCursor::new(journal), durable_tail);

        let (request_sender, mut request_receiver) = mpsc::unbounded_channel();
        let (mut runtime, module_loader) = isolate(request_sender)?;
        let sdk_specifier = ModuleSpecifier::parse(PROGRAM_SDK_PRELOAD_SPECIFIER)
            .map_err(JsErrorBox::from_err)
            .map_err(deno_core::error::CoreError::from)?;
        module_loader.preload_admitted.set(true);
        let sdk_module = runtime
            .load_side_es_module_from_code(
                &sdk_specifier,
                format!("import {PROGRAM_SDK_V1_SPECIFIER:?};"),
            )
            .await?;
        module_loader.preload_admitted.set(false);
        let sdk_evaluation = runtime.mod_evaluate(sdk_module);
        runtime
            .run_event_loop(PollEventLoopOptions::default())
            .await?;
        sdk_evaluation.await?;
        let main_specifier = ModuleSpecifier::parse(PROGRAM_MAIN_SPECIFIER)
            .map_err(JsErrorBox::from_err)
            .map_err(deno_core::error::CoreError::from)?;
        let module = runtime
            .load_main_es_module_from_code(&main_specifier, artifact.source().to_owned())
            .await?;
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
                        return Ok(ProgramExecutionOutcome::Completed);
                    }
                    true
                }
                ReplayInstruction::AwaitRequest => {
                    if let Some(result) = completed_evaluation.take() {
                        result?;
                        return Err(ProgramHostProtocolError::Stalled.into());
                    }
                    false
                }
            };

            match runtime_status {
                Poll::Ready(result) => {
                    if completed_evaluation.is_none() {
                        return Err(ProgramHostProtocolError::Stalled.into());
                    }
                    result?;
                    let Some(result) = completed_evaluation.take() else {
                        return Err(ProgramHostProtocolError::Stalled.into());
                    };
                    result?;
                    if at_live_tail {
                        return Ok(ProgramExecutionOutcome::Completed);
                    }
                    return Err(ProgramHostProtocolError::Stalled.into());
                }
                Poll::Pending => {
                    tokio::select! {
                        result = runtime.run_event_loop(PollEventLoopOptions::default()) => {
                            poll_evaluation_once(
                                &mut evaluation,
                                &mut completed_evaluation,
                            ).await;
                            if completed_evaluation.is_none() {
                                return Err(ProgramHostProtocolError::Stalled.into());
                            }
                            result?;
                            let Some(result) = completed_evaluation.take() else {
                                return Err(ProgramHostProtocolError::Stalled.into());
                            };
                            result?;
                            if at_live_tail {
                                return Ok(ProgramExecutionOutcome::Completed);
                            }
                            return Err(ProgramHostProtocolError::Stalled.into());
                        }
                        request = request_receiver.recv() => {
                            let request = request.ok_or(
                                ProgramHostProtocolError::IsolateRequestChannelClosed,
                            )?;
                            self.accept_request(run, &mut execution, request).await?;
                            continue;
                        }
                    }
                }
            }
        }
    }

    async fn accept_request(
        &self,
        run: ProgramRunId,
        execution: &mut ExecutionState,
        request: IsolateRequest,
    ) -> Result<(), ProgramHostError> {
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
                    .ok_or(ProgramHostProtocolError::JournalTailChanged)?;
                if persisted != frame {
                    return Err(ProgramHostProtocolError::LiveRequestWasNotAppendedExactly.into());
                }
                execution.advance_durable_tail()?;
                execution.insert_pending(persisted, request.reply)?;
            }
            Ok(ReplayedRequest::DeliveryPending) => {
                return Err(ProgramHostProtocolError::DeliveryPending.into());
            }
            Err(divergence) => {
                return Err(self
                    .persist_divergence(divergence, execution.durable_tail())
                    .await?);
            }
        }
        Ok(())
    }

    async fn persist_divergence(
        &self,
        divergence: NondeterminismError,
        durable_tail: u64,
    ) -> Result<ProgramHostError, ProgramHostError> {
        let expected = Box::new(divergence.expected().clone());
        let observed = Box::new(divergence.observed().clone());
        let fault = self
            .journal
            .append_nondeterminism_fault_if_tail(divergence, durable_tail)
            .await?
            .ok_or(ProgramHostProtocolError::JournalTailChanged)?;
        Ok(ProgramHostError::Nondeterminism {
            expected,
            observed,
            fault,
        })
    }

    async fn deliver_live(
        &self,
        run: ProgramRunId,
        execution: &mut ExecutionState,
        live_deliveries: &mut impl LiveDeliverySource,
    ) -> Result<Option<ProgramExecutionOutcome>, ProgramHostError> {
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
            .ok_or(ProgramHostProtocolError::JournalTailChanged)?;
        execution.advance_durable_tail()?;
        execution.apply_delivery(delivery).map_err(Into::into)
    }
}

/// The outcome a delivery carries when it ends the run instead of resolving a
/// request. Terminal kinds resolve nothing, which is what lets the journal name
/// the outcome without replaying the artifact that produced it.
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

fn isolate(
    sender: mpsc::UnboundedSender<IsolateRequest>,
) -> Result<(JsRuntime, Rc<ProgramModuleLoader>), deno_core::error::CoreError> {
    let module_loader = Rc::new(ProgramModuleLoader {
        preload_admitted: Cell::new(false),
    });
    let runtime = JsRuntime::new(RuntimeOptions {
        module_loader: Some(module_loader.clone()),
        extensions: vec![signalbox_program_sdk_v1::init()],
        ..Default::default()
    });
    runtime
        .op_state()
        .borrow_mut()
        .put(IsolateRequestSender(sender));
    Ok((runtime, module_loader))
}

#[derive(Clone)]
struct IsolateRequestSender(mpsc::UnboundedSender<IsolateRequest>);

struct IsolateRequest {
    kind: IsolateRequestKind,
    reply: oneshot::Sender<IsolateDelivery>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum IsolateRequestKind {
    Now { payload: Vec<u8> },
    Random { payload: Vec<u8> },
    Sleep { payload: Vec<u8> },
    AwaitEvent { payload: Vec<u8> },
}

impl IsolateRequestKind {
    fn into_domain(self) -> RequestKind {
        match self {
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
}

#[op2]
#[serde]
async fn op_program_request(
    state: Rc<RefCell<OpState>>,
    #[serde] request: IsolateRequestKind,
) -> Result<IsolateDelivery, JsErrorBox> {
    let sender = {
        let state = state.borrow();
        state.borrow::<IsolateRequestSender>().0.clone()
    };
    let (reply, delivery) = oneshot::channel();
    sender
        .send(IsolateRequest {
            kind: request,
            reply,
        })
        .map_err(|_| JsErrorBox::generic("program host request channel closed"))?;
    delivery
        .await
        .map_err(|_| JsErrorBox::generic("program host delivery channel closed"))
}

struct PendingRequest {
    frame: RequestFrame,
    reply: oneshot::Sender<IsolateDelivery>,
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

    fn advance_durable_tail(&mut self) -> Result<(), ProgramHostProtocolError> {
        self.durable_tail = self
            .durable_tail
            .checked_add(1)
            .ok_or(ProgramHostProtocolError::JournalPositionExhausted)?;
        Ok(())
    }

    fn frame_for(
        &mut self,
        kind: IsolateRequestKind,
    ) -> Result<RequestFrame, ProgramHostProtocolError> {
        let ordinal = RequestOrdinal::try_from_u64(self.next_request_ordinal)
            .ok_or(ProgramHostProtocolError::RequestOrdinalExhausted)?;
        self.next_request_ordinal = self
            .next_request_ordinal
            .checked_add(1)
            .ok_or(ProgramHostProtocolError::RequestOrdinalExhausted)?;
        Ok(RequestFrame::new(ordinal, None, kind.into_domain()))
    }

    fn insert_pending(
        &mut self,
        frame: RequestFrame,
        reply: oneshot::Sender<IsolateDelivery>,
    ) -> Result<(), ProgramHostProtocolError> {
        let ordinal = frame.ordinal();
        if self
            .pending
            .insert(ordinal, PendingRequest { frame, reply })
            .is_some()
        {
            return Err(ProgramHostProtocolError::DuplicateOutstandingRequest);
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

    fn validate_delivery(&self, kind: &DeliveryKind) -> Result<(), ProgramHostProtocolError> {
        match kind.resolves() {
            Some(ordinal) if !self.pending.contains_key(&ordinal) => {
                Err(ProgramHostProtocolError::UnknownResolvedRequest)
            }
            Some(_) | None => Ok(()),
        }
    }

    fn apply_delivery(
        &mut self,
        delivery: DeliveryFrame,
    ) -> Result<Option<ProgramExecutionOutcome>, ProgramHostProtocolError> {
        match delivery.kind() {
            DeliveryKind::Answer { resolves, payload } => {
                self.resolve(
                    *resolves,
                    IsolateDelivery::Answer {
                        payload: payload.as_bytes().to_vec(),
                    },
                )?;
                Ok(None)
            }
            DeliveryKind::Wake { resolves, payload } => {
                self.resolve(
                    *resolves,
                    IsolateDelivery::Wake {
                        payload: payload.as_bytes().to_vec(),
                    },
                )?;
                Ok(None)
            }
            DeliveryKind::Reject { resolves, reason } => {
                let reason = match reason {
                    RejectReason::OutstandingRequests => IsolateRejectReason::OutstandingRequests,
                };
                self.resolve(*resolves, IsolateDelivery::Reject { reason })?;
                Ok(None)
            }
            DeliveryKind::Cancel { resolves, payload } => {
                self.resolve(
                    *resolves,
                    IsolateDelivery::Cancel {
                        payload: payload.as_bytes().to_vec(),
                    },
                )?;
                Ok(None)
            }
            DeliveryKind::RunCancel(_) | DeliveryKind::Fault(_) => Ok(terminal_outcome(&delivery)),
        }
    }

    fn resolve(
        &mut self,
        ordinal: RequestOrdinal,
        delivery: IsolateDelivery,
    ) -> Result<(), ProgramHostProtocolError> {
        let pending = self
            .pending
            .remove(&ordinal)
            .ok_or(ProgramHostProtocolError::UnknownResolvedRequest)?;
        pending
            .reply
            .send(delivery)
            .map_err(|_| ProgramHostProtocolError::DeliveryReceiverClosed)
    }
}

struct ProgramModuleLoader {
    preload_admitted: Cell<bool>,
}

impl ModuleLoader for ProgramModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        _referrer: &str,
        kind: ResolutionKind,
    ) -> ModuleResolveResponse {
        if matches!(kind, ResolutionKind::MainModule) && specifier == PROGRAM_MAIN_SPECIFIER {
            return ModuleSpecifier::parse(PROGRAM_MAIN_SPECIFIER).map_err(JsErrorBox::from_err);
        }
        if self.preload_admitted.get() && specifier == PROGRAM_SDK_PRELOAD_SPECIFIER {
            return ModuleSpecifier::parse(PROGRAM_SDK_PRELOAD_SPECIFIER)
                .map_err(JsErrorBox::from_err);
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
            preload_admitted: Cell::new(false),
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
            preload_admitted: Cell::new(false),
        }
        .resolve(
            super::PROGRAM_SDK_V1_SPECIFIER,
            PROGRAM_MAIN_SPECIFIER,
            ResolutionKind::Import,
        )
        .expect("the canonical SDK import is admitted");

        assert_eq!(resolved.as_str(), PROGRAM_SDK_INTERNAL_SPECIFIER);
    }
}
