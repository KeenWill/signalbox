//! Typed frames and deterministic replay for the program execution journal.
//!
//! The normative cross-component contract is `docs/spec/program-substrate.md`.

use std::{collections::BTreeSet, error::Error, fmt, num::NonZeroU64};

use crate::ProgramRunId;

macro_rules! positive_position {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU64);

        impl $name {
            /// Constructs a positive ordinal.
            pub const fn try_from_u64(value: u64) -> Option<Self> {
                match NonZeroU64::new(value) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }

            /// Returns the positive integer value.
            pub const fn as_u64(self) -> u64 {
                self.0.get()
            }
        }
    };
}

positive_position!(
    /// Position of one frame in a run's complete append-only journal.
    JournalPosition
);
positive_position!(
    /// Program-order position of one request in a run.
    RequestOrdinal
);
positive_position!(
    /// Delivery-order position of one host delivery in a run.
    DeliveryOrdinal
);
positive_position!(
    /// Identity of one scope inside a run.
    ScopeOrdinal
);

/// Inline canonical bytes carried by one frame.
///
/// The bytes are deliberately isolated behind a type. A later payload-offload
/// slice can add a digest-backed representation at this boundary without
/// changing frame kinds or existing inline journal rows.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InlineFramePayload(Box<[u8]>);

impl InlineFramePayload {
    /// Owns exact canonical payload bytes.
    pub fn new(bytes: impl Into<Box<[u8]>>) -> Self {
        Self(bytes.into())
    }

    /// Borrows the exact canonical payload bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Capability named by a generic effect request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgramCapability {
    Time,
    Random,
    Sleep,
    Subscribe,
    Session,
    Judge,
    ExecStage,
    Corpus,
    EvalRecord,
    Blob,
    Register,
}

/// Structured-concurrency scope operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScopeOperation {
    Open,
    Close,
}

/// One scope-tree declaration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScopeRequest {
    operation: ScopeOperation,
    scope: ScopeOrdinal,
    parent: Option<ScopeOrdinal>,
}

impl ScopeRequest {
    pub const fn new(
        operation: ScopeOperation,
        scope: ScopeOrdinal,
        parent: Option<ScopeOrdinal>,
    ) -> Self {
        Self {
            operation,
            scope,
            parent,
        }
    }

    pub const fn operation(self) -> ScopeOperation {
        self.operation
    }

    pub const fn scope(self) -> ScopeOrdinal {
        self.scope
    }

    pub const fn parent(self) -> Option<ScopeOrdinal> {
        self.parent
    }
}

/// Generic capability call carried by an `effect` request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectRequest {
    capability: ProgramCapability,
    method: String,
    payload: InlineFramePayload,
}

impl EffectRequest {
    pub fn new(capability: ProgramCapability, method: String, payload: InlineFramePayload) -> Self {
        Self {
            capability,
            method,
            payload,
        }
    }

    pub const fn capability(&self) -> ProgramCapability {
        self.capability
    }

    pub fn method(&self) -> &str {
        &self.method
    }

    pub const fn payload(&self) -> &InlineFramePayload {
        &self.payload
    }
}

/// Closed request-frame vocabulary for frame-contract version one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequestKind {
    Now(InlineFramePayload),
    Random(InlineFramePayload),
    Sleep(InlineFramePayload),
    AwaitEvent(InlineFramePayload),
    Effect(EffectRequest),
    Scope(ScopeRequest),
    Terminal(InlineFramePayload),
}

/// One request exactly as observed at the executor seam.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestFrame {
    ordinal: RequestOrdinal,
    scope: Option<ScopeOrdinal>,
    kind: RequestKind,
}

impl RequestFrame {
    pub const fn new(
        ordinal: RequestOrdinal,
        scope: Option<ScopeOrdinal>,
        kind: RequestKind,
    ) -> Self {
        Self {
            ordinal,
            scope,
            kind,
        }
    }

    pub const fn ordinal(&self) -> RequestOrdinal {
        self.ordinal
    }

    pub const fn scope(&self) -> Option<ScopeOrdinal> {
        self.scope
    }

    pub const fn kind(&self) -> &RequestKind {
        &self.kind
    }
}

/// Closed rejection reasons admitted by the implemented request vocabulary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RejectReason {
    OutstandingRequests,
}

/// Closed terminal fault vocabulary for frame-contract version one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultCause {
    Timeout,
    Memory,
    Nondeterminism,
    ProgramError,
    ContractRetired,
    JournalBound,
    PayloadTooLarge,
}

/// One terminal fault with a cause/evidence shape that cannot disagree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProgramFault {
    Timeout(InlineFramePayload),
    Memory(InlineFramePayload),
    Nondeterminism {
        expected: RequestFrame,
        observed: RequestFrame,
    },
    ProgramError(InlineFramePayload),
    ContractRetired(InlineFramePayload),
    JournalBound(InlineFramePayload),
    PayloadTooLarge(InlineFramePayload),
}

impl ProgramFault {
    pub const fn cause(&self) -> FaultCause {
        match self {
            Self::Timeout(_) => FaultCause::Timeout,
            Self::Memory(_) => FaultCause::Memory,
            Self::Nondeterminism { .. } => FaultCause::Nondeterminism,
            Self::ProgramError(_) => FaultCause::ProgramError,
            Self::ContractRetired(_) => FaultCause::ContractRetired,
            Self::JournalBound(_) => FaultCause::JournalBound,
            Self::PayloadTooLarge(_) => FaultCause::PayloadTooLarge,
        }
    }

    pub const fn evidence(&self) -> FaultEvidenceRef<'_> {
        match self {
            Self::Timeout(payload)
            | Self::Memory(payload)
            | Self::ProgramError(payload)
            | Self::ContractRetired(payload)
            | Self::JournalBound(payload)
            | Self::PayloadTooLarge(payload) => FaultEvidenceRef::Ordinary(payload),
            Self::Nondeterminism { expected, observed } => {
                FaultEvidenceRef::Nondeterminism { expected, observed }
            }
        }
    }
}

/// Borrowed fault evidence without an allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultEvidenceRef<'a> {
    Ordinary(&'a InlineFramePayload),
    Nondeterminism {
        expected: &'a RequestFrame,
        observed: &'a RequestFrame,
    },
}

/// Closed delivery-frame vocabulary for frame-contract version one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeliveryKind {
    Answer {
        resolves: RequestOrdinal,
        payload: InlineFramePayload,
    },
    Wake {
        resolves: RequestOrdinal,
        payload: InlineFramePayload,
    },
    Reject {
        resolves: RequestOrdinal,
        reason: RejectReason,
    },
    Cancel {
        resolves: RequestOrdinal,
        payload: InlineFramePayload,
    },
    RunCancel(InlineFramePayload),
    Fault(ProgramFault),
}

impl DeliveryKind {
    pub const fn resolves(&self) -> Option<RequestOrdinal> {
        match self {
            Self::Answer { resolves, .. }
            | Self::Wake { resolves, .. }
            | Self::Reject { resolves, .. }
            | Self::Cancel { resolves, .. } => Some(*resolves),
            Self::RunCancel(_) | Self::Fault(_) => None,
        }
    }
}

/// One host delivery in its run-local delivery order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryFrame {
    ordinal: DeliveryOrdinal,
    kind: DeliveryKind,
}

impl DeliveryFrame {
    pub const fn new(ordinal: DeliveryOrdinal, kind: DeliveryKind) -> Self {
        Self { ordinal, kind }
    }

    pub const fn ordinal(&self) -> DeliveryOrdinal {
        self.ordinal
    }

    pub const fn kind(&self) -> &DeliveryKind {
        &self.kind
    }
}

/// One position in the complete request/delivery interleaving.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JournalFrame {
    Request(RequestFrame),
    Delivery(DeliveryFrame),
}

/// One positioned immutable journal frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalEntry {
    position: JournalPosition,
    frame: JournalFrame,
}

impl JournalEntry {
    pub const fn new(position: JournalPosition, frame: JournalFrame) -> Self {
        Self { position, frame }
    }

    pub const fn position(&self) -> JournalPosition {
        self.position
    }

    pub const fn frame(&self) -> &JournalFrame {
        &self.frame
    }
}

/// Complete checked journal for one run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramJournal {
    run: ProgramRunId,
    entries: Box<[JournalEntry]>,
}

impl ProgramJournal {
    /// The recorded terminal outcome, if this run already ended.
    ///
    /// A `run_cancel` or `fault` resolves no request and ends the attempt that
    /// recorded it, so every later frame is behind an outcome that is already
    /// durable. The first such delivery is therefore the run's outcome, and it
    /// is knowable from the journal alone — a resumed run cannot produce a
    /// different one, whatever its artifact does or whether it loads at all.
    pub fn terminal_delivery(&self) -> Option<&DeliveryFrame> {
        self.entries.iter().find_map(|entry| match entry.frame() {
            JournalFrame::Delivery(delivery) if delivery.kind().resolves().is_none() => {
                Some(delivery)
            }
            JournalFrame::Delivery(_) | JournalFrame::Request(_) => None,
        })
    }

    /// Validates all three contiguous orders and resolution correlations.
    pub fn try_new(
        run: ProgramRunId,
        entries: Vec<JournalEntry>,
    ) -> Result<Self, ProgramJournalError> {
        let mut next_position = 1_u64;
        let mut next_request = 1_u64;
        let mut next_delivery = 1_u64;
        let mut answerable = BTreeSet::new();
        let mut resolved = BTreeSet::new();

        for entry in &entries {
            if entry.position().as_u64() != next_position {
                return Err(ProgramJournalError::NoncontiguousPosition);
            }
            next_position = next_position
                .checked_add(1)
                .ok_or(ProgramJournalError::OrdinalExhausted)?;

            match entry.frame() {
                JournalFrame::Request(request) => {
                    if request.ordinal().as_u64() != next_request {
                        return Err(ProgramJournalError::NoncontiguousRequestOrdinal);
                    }
                    next_request = next_request
                        .checked_add(1)
                        .ok_or(ProgramJournalError::OrdinalExhausted)?;
                    if !matches!(request.kind(), RequestKind::Scope(_)) {
                        answerable.insert(request.ordinal());
                    }
                }
                JournalFrame::Delivery(delivery) => {
                    if delivery.ordinal().as_u64() != next_delivery {
                        return Err(ProgramJournalError::NoncontiguousDeliveryOrdinal);
                    }
                    next_delivery = next_delivery
                        .checked_add(1)
                        .ok_or(ProgramJournalError::OrdinalExhausted)?;
                    if let Some(request) = delivery.kind().resolves() {
                        if !answerable.contains(&request) {
                            return Err(ProgramJournalError::UnknownResolvedRequest);
                        }
                        if !resolved.insert(request) {
                            return Err(ProgramJournalError::RequestResolvedTwice);
                        }
                    }
                }
            }
        }

        Ok(Self {
            run,
            entries: entries.into_boxed_slice(),
        })
    }

    pub const fn run(&self) -> ProgramRunId {
        self.run
    }

    pub fn entries(&self) -> &[JournalEntry] {
        &self.entries
    }
}

/// A durable journal could not be reconstituted as one legal frame stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgramJournalError {
    NoncontiguousPosition,
    NoncontiguousRequestOrdinal,
    NoncontiguousDeliveryOrdinal,
    UnknownResolvedRequest,
    RequestResolvedTwice,
    OrdinalExhausted,
}

impl fmt::Display for ProgramJournalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::NoncontiguousPosition => "journal positions are not contiguous",
            Self::NoncontiguousRequestOrdinal => "request ordinals are not contiguous",
            Self::NoncontiguousDeliveryOrdinal => "delivery ordinals are not contiguous",
            Self::UnknownResolvedRequest => "delivery resolves no earlier answerable request",
            Self::RequestResolvedTwice => "request has more than one resolving delivery",
            Self::OrdinalExhausted => "journal ordinal is exhausted",
        };
        formatter.write_str(message)
    }
}

impl Error for ProgramJournalError {}

/// What the executor-facing host must do next.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplayInstruction {
    AwaitRequest,
    Deliver(DeliveryFrame),
    Live,
}

/// Whether an emitted request matched history or belongs to live execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayedRequest {
    Matched,
    DeliveryPending,
    Live,
}

/// Typed nondeterminism failure retaining both complete frames.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NondeterminismError {
    run: ProgramRunId,
    expected: Box<RequestFrame>,
    observed: Box<RequestFrame>,
}

impl NondeterminismError {
    pub const fn run(&self) -> ProgramRunId {
        self.run
    }

    pub const fn expected(&self) -> &RequestFrame {
        &self.expected
    }

    pub const fn observed(&self) -> &RequestFrame {
        &self.observed
    }

    pub fn into_fault(self) -> ProgramFault {
        ProgramFault::Nondeterminism {
            expected: *self.expected,
            observed: *self.observed,
        }
    }
}

impl fmt::Display for NondeterminismError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "program request diverged at request ordinal {}: expected {:?}, observed {:?}",
            self.expected.ordinal().as_u64(),
            self.expected,
            self.observed
        )
    }
}

impl Error for NondeterminismError {}

/// Deterministic executor seam over one immutable journal.
///
/// The future isolate host must alternate `next_instruction` with executor
/// request emission. Recorded deliveries are applied one at a time in journal
/// order, which gives the host a quiescence point after every delivery. A
/// journal that already records a terminal outcome never reaches this seam:
/// [`ProgramJournal::terminal_delivery`] answers it without replay. Once `Live`
/// is returned the cursor never consults history again.
#[derive(Clone, Debug)]
pub struct ReplayCursor {
    run: ProgramRunId,
    entries: Box<[JournalEntry]>,
    next: usize,
    live: bool,
}

impl ReplayCursor {
    pub fn new(journal: ProgramJournal) -> Self {
        Self {
            run: journal.run,
            entries: journal.entries,
            next: 0,
            live: false,
        }
    }

    pub fn next_instruction(&mut self) -> ReplayInstruction {
        if self.live {
            return ReplayInstruction::Live;
        }
        match self.entries.get(self.next).map(JournalEntry::frame) {
            Some(JournalFrame::Request(_)) => ReplayInstruction::AwaitRequest,
            Some(JournalFrame::Delivery(delivery)) => {
                self.next += 1;
                ReplayInstruction::Deliver(delivery.clone())
            }
            None => {
                self.live = true;
                ReplayInstruction::Live
            }
        }
    }

    pub fn submit_request(
        &mut self,
        observed: RequestFrame,
    ) -> Result<ReplayedRequest, NondeterminismError> {
        if self.live || self.next == self.entries.len() {
            self.live = true;
            return Ok(ReplayedRequest::Live);
        }
        let Some(JournalFrame::Request(expected)) =
            self.entries.get(self.next).map(JournalEntry::frame)
        else {
            // The host owns sequencing and must drain the recorded delivery
            // before accepting another executor request. Treating that host
            // misuse as program nondeterminism would misdiagnose the program.
            return Ok(ReplayedRequest::DeliveryPending);
        };
        if expected != &observed {
            if let Some((fault_index, _)) = self
                .entries
                .iter()
                .enumerate()
                .skip(self.next + 1)
                .find(|(_, entry)| {
                    let JournalFrame::Delivery(delivery) = entry.frame() else {
                        return false;
                    };
                    let DeliveryKind::Fault(ProgramFault::Nondeterminism {
                        expected: persisted_expected,
                        ..
                    }) = delivery.kind()
                    else {
                        return false;
                    };
                    persisted_expected == expected
                })
            {
                self.next = fault_index;
                return Ok(ReplayedRequest::Matched);
            }
            return Err(NondeterminismError {
                run: self.run,
                expected: Box::new(expected.clone()),
                observed: Box::new(observed),
            });
        }
        self.next += 1;
        Ok(ReplayedRequest::Matched)
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    const RUN_ID: u128 = 0x51_0001;

    fn request(ordinal: u64, payload: &[u8]) -> RequestFrame {
        RequestFrame::new(
            RequestOrdinal::try_from_u64(ordinal).expect("fixture ordinal is positive"),
            None,
            RequestKind::Now(InlineFramePayload::new(payload)),
        )
    }

    fn delivery(ordinal: u64, resolves: u64, payload: &[u8]) -> DeliveryFrame {
        DeliveryFrame::new(
            DeliveryOrdinal::try_from_u64(ordinal).expect("fixture ordinal is positive"),
            DeliveryKind::Answer {
                resolves: RequestOrdinal::try_from_u64(resolves)
                    .expect("fixture resolution is positive"),
                payload: InlineFramePayload::new(payload),
            },
        )
    }

    fn run_cancel(ordinal: u64, payload: &[u8]) -> DeliveryFrame {
        DeliveryFrame::new(
            DeliveryOrdinal::try_from_u64(ordinal).expect("fixture ordinal is positive"),
            DeliveryKind::RunCancel(InlineFramePayload::new(payload)),
        )
    }

    fn entry(position: u64, frame: JournalFrame) -> JournalEntry {
        JournalEntry::new(
            JournalPosition::try_from_u64(position).expect("fixture position is positive"),
            frame,
        )
    }

    fn journal(entries: Vec<JournalEntry>) -> ProgramJournal {
        ProgramJournal::try_new(ProgramRunId::from_uuid(Uuid::from_u128(RUN_ID)), entries)
            .expect("fixture journal is valid")
    }

    /// replay delivers concurrent answers in durable delivery order.
    #[test]
    fn replay_preserves_delivery_order_for_concurrent_requests() {
        let first_request = request(1, b"first");
        let second_request = request(2, b"second");
        let second_answer = delivery(1, 2, b"second-answer");
        let first_answer = delivery(2, 1, b"first-answer");
        let mut replay = ReplayCursor::new(journal(vec![
            entry(1, JournalFrame::Request(first_request.clone())),
            entry(2, JournalFrame::Request(second_request.clone())),
            entry(3, JournalFrame::Delivery(second_answer.clone())),
            entry(4, JournalFrame::Delivery(first_answer.clone())),
        ]));

        assert_eq!(replay.next_instruction(), ReplayInstruction::AwaitRequest);
        assert_eq!(
            replay.submit_request(first_request),
            Ok(ReplayedRequest::Matched)
        );
        assert_eq!(replay.next_instruction(), ReplayInstruction::AwaitRequest);
        assert_eq!(
            replay.submit_request(second_request),
            Ok(ReplayedRequest::Matched)
        );
        assert_eq!(
            replay.next_instruction(),
            ReplayInstruction::Deliver(second_answer)
        );
        assert_eq!(
            replay.next_instruction(),
            ReplayInstruction::Deliver(first_answer)
        );
        assert_eq!(replay.next_instruction(), ReplayInstruction::Live);
    }

    /// replay switches to live execution exactly at a partial journal tail.
    #[test]
    fn partial_journal_resumes_then_switches_to_live_execution() {
        let recorded = request(1, b"recorded");
        let live = request(2, b"new");
        let mut replay = ReplayCursor::new(journal(vec![entry(
            1,
            JournalFrame::Request(recorded.clone()),
        )]));

        assert_eq!(replay.next_instruction(), ReplayInstruction::AwaitRequest);
        assert_eq!(
            replay.submit_request(recorded),
            Ok(ReplayedRequest::Matched)
        );
        assert_eq!(replay.next_instruction(), ReplayInstruction::Live);
        assert_eq!(replay.submit_request(live), Ok(ReplayedRequest::Live));
    }

    #[test]
    fn a_journal_opening_with_a_run_cancel_reports_it_as_the_terminal_outcome() {
        let run_cancel = run_cancel(1, b"cancelled");
        let ended = journal(vec![entry(1, JournalFrame::Delivery(run_cancel.clone()))]);

        assert_eq!(ended.terminal_delivery(), Some(&run_cancel));
    }

    #[test]
    fn a_terminal_delivery_behind_earlier_frames_is_still_the_terminal_outcome() {
        let recorded_request = request(1, b"recorded");
        let recorded_answer = delivery(1, 1, b"recorded-answer");
        let run_cancel = run_cancel(2, b"cancelled");
        let ended = journal(vec![
            entry(1, JournalFrame::Request(recorded_request)),
            entry(2, JournalFrame::Delivery(recorded_answer)),
            entry(3, JournalFrame::Delivery(run_cancel.clone())),
        ]);

        assert_eq!(ended.terminal_delivery(), Some(&run_cancel));
    }

    #[test]
    fn the_first_terminal_delivery_wins_over_frames_recorded_behind_it() {
        let recorded_request = request(1, b"recorded");
        let first_cancel = run_cancel(1, b"first");
        let later_cancel = run_cancel(2, b"later");
        let ended = journal(vec![
            entry(1, JournalFrame::Request(recorded_request)),
            entry(2, JournalFrame::Delivery(first_cancel.clone())),
            entry(3, JournalFrame::Delivery(later_cancel)),
        ]);

        assert_eq!(ended.terminal_delivery(), Some(&first_cancel));
    }

    #[test]
    fn a_journal_of_resolving_deliveries_records_no_terminal_outcome() {
        let recorded_request = request(1, b"recorded");
        let recorded_answer = delivery(1, 1, b"recorded-answer");
        let running = journal(vec![
            entry(1, JournalFrame::Request(recorded_request)),
            entry(2, JournalFrame::Delivery(recorded_answer)),
        ]);

        assert_eq!(running.terminal_delivery(), None);
    }

    /// a replay mismatch is a typed failure carrying both frames.
    #[test]
    fn replay_divergence_carries_expected_and_observed_frames() {
        let expected = request(1, b"recorded");
        let observed = request(1, b"different");
        let mut replay = ReplayCursor::new(journal(vec![entry(
            1,
            JournalFrame::Request(expected.clone()),
        )]));

        let error = replay
            .submit_request(observed.clone())
            .expect_err("different canonical request bytes must diverge");

        assert_eq!(
            error.run(),
            ProgramRunId::from_uuid(Uuid::from_u128(RUN_ID))
        );
        assert_eq!(error.expected(), &expected);
        assert_eq!(error.observed(), &observed);
        assert_eq!(
            error.into_fault(),
            ProgramFault::Nondeterminism { expected, observed }
        );
    }

    #[test]
    fn persisted_nondeterminism_fault_replays_after_restart() {
        let expected = request(1, b"recorded");
        let observed = request(1, b"different");
        let fault = DeliveryFrame::new(
            DeliveryOrdinal::try_from_u64(1).expect("fixture ordinal is positive"),
            DeliveryKind::Fault(ProgramFault::Nondeterminism {
                expected: expected.clone(),
                observed: observed.clone(),
            }),
        );
        let mut replay = ReplayCursor::new(journal(vec![
            entry(1, JournalFrame::Request(expected)),
            entry(2, JournalFrame::Delivery(fault.clone())),
        ]));

        assert_eq!(replay.next_instruction(), ReplayInstruction::AwaitRequest);
        assert_eq!(
            replay.submit_request(observed),
            Ok(ReplayedRequest::Matched)
        );
        assert_eq!(replay.next_instruction(), ReplayInstruction::Deliver(fault));
        assert_eq!(replay.next_instruction(), ReplayInstruction::Live);
    }

    #[test]
    fn tail_appended_nondeterminism_fault_replays_after_recorded_suffix() {
        let expected = request(1, b"recorded");
        let observed = request(1, b"different");
        let suffix_request = request(2, b"suffix");
        let suffix_delivery = delivery(1, 2, b"suffix-answer");
        let fault = DeliveryFrame::new(
            DeliveryOrdinal::try_from_u64(2).expect("fixture ordinal is positive"),
            DeliveryKind::Fault(ProgramFault::Nondeterminism {
                expected: expected.clone(),
                observed: observed.clone(),
            }),
        );
        let mut replay = ReplayCursor::new(journal(vec![
            entry(1, JournalFrame::Request(expected)),
            entry(2, JournalFrame::Request(suffix_request)),
            entry(3, JournalFrame::Delivery(suffix_delivery)),
            entry(4, JournalFrame::Delivery(fault.clone())),
        ]));

        assert_eq!(replay.next_instruction(), ReplayInstruction::AwaitRequest);
        assert_eq!(
            replay.submit_request(observed),
            Ok(ReplayedRequest::Matched)
        );
        assert_eq!(replay.next_instruction(), ReplayInstruction::Deliver(fault));
        assert_eq!(replay.next_instruction(), ReplayInstruction::Live);
    }

    #[test]
    fn persisted_nondeterminism_fault_replays_before_later_frames() {
        let expected = request(1, b"recorded");
        let observed = request(1, b"different");
        let fault = DeliveryFrame::new(
            DeliveryOrdinal::try_from_u64(1).expect("fixture ordinal is positive"),
            DeliveryKind::Fault(ProgramFault::Nondeterminism {
                expected: expected.clone(),
                observed: observed.clone(),
            }),
        );
        let later_request = request(2, b"later");
        let later_delivery = delivery(2, 2, b"later-answer");
        let mut replay = ReplayCursor::new(journal(vec![
            entry(1, JournalFrame::Request(expected)),
            entry(2, JournalFrame::Delivery(fault.clone())),
            entry(3, JournalFrame::Request(later_request)),
            entry(4, JournalFrame::Delivery(later_delivery)),
        ]));

        assert_eq!(replay.next_instruction(), ReplayInstruction::AwaitRequest);
        assert_eq!(
            replay.submit_request(observed),
            Ok(ReplayedRequest::Matched)
        );
        assert_eq!(replay.next_instruction(), ReplayInstruction::Deliver(fault));
    }

    #[test]
    fn first_persisted_nondeterminism_fault_replays_when_observation_changes() {
        let expected = request(1, b"recorded");
        let first_observed = request(1, b"first-divergence");
        let later_observed = request(1, b"later-divergence");
        let fault = DeliveryFrame::new(
            DeliveryOrdinal::try_from_u64(1).expect("fixture ordinal is positive"),
            DeliveryKind::Fault(ProgramFault::Nondeterminism {
                expected: expected.clone(),
                observed: first_observed,
            }),
        );
        let mut replay = ReplayCursor::new(journal(vec![
            entry(1, JournalFrame::Request(expected)),
            entry(2, JournalFrame::Delivery(fault.clone())),
        ]));

        assert_eq!(replay.next_instruction(), ReplayInstruction::AwaitRequest);
        assert_eq!(
            replay.submit_request(later_observed),
            Ok(ReplayedRequest::Matched)
        );
        assert_eq!(replay.next_instruction(), ReplayInstruction::Deliver(fault));
    }
}
