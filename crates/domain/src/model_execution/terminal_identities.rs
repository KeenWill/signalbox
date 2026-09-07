//! Model-call terminal identities for `docs/spec/model-call-execution.md`.

use super::{
    AmbiguousModelCallTurn, CancelledModelCallTurn, CancelledToolRoundModelCallTurn,
    CompletedModelCallTurn, FailedModelCallTurn, ReconciliationRequiredModelCallTurn,
    ReconciliationRequiredToolTurn, RefusedModelCallTurn, StopRequestedModelCallTurn,
    ToolRoundModelCallTurn,
};
use crate::{
    AcceptedInputId, ContextFrontierId, InitialToolApproval, SemanticTranscriptEntryId,
    ToolRequestId, TurnAttemptId, TurnId,
};

/// One fresh turn identity correlated to an exact pending steering input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingSteeringReclassificationIdentity {
    pub(super) accepted_input: AcceptedInputId,
    pub(super) turn: TurnId,
}

impl PendingSteeringReclassificationIdentity {
    /// Associates one pending accepted input with its proposed successor turn.
    pub const fn new(accepted_input: AcceptedInputId, turn: TurnId) -> Self {
        Self {
            accepted_input,
            turn,
        }
    }

    /// Returns the pending accepted input being reclassified.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns the fresh turn proposed for that input.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
}

/// Fresh identities for a successful text-only outcome transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedModelCallIdentities {
    pub(super) assistant_entries: Vec<SemanticTranscriptEntryId>,
    pub(super) completion_entry: SemanticTranscriptEntryId,
    pub(super) terminal_frontier: ContextFrontierId,
    pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

impl CompletedModelCallIdentities {
    /// Supplies one identity per text part, the final marker, and frontier.
    pub fn new(
        assistant_entries: Vec<SemanticTranscriptEntryId>,
        completion_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            assistant_entries,
            completion_entry,
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one fresh successor identity per pending steering input, in
    /// session acceptance order.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }
}

/// Fresh identities and initial policy for one ordered tool-response part.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolResponsePartIdentity {
    /// One semantic assistant-text entry.
    Text {
        /// Fresh semantic-entry identity.
        entry: SemanticTranscriptEntryId,
    },
    /// One semantic provider-compaction entry.
    ProviderCompaction {
        /// Fresh semantic-entry identity.
        entry: SemanticTranscriptEntryId,
    },
    /// One semantic provider-reasoning entry.
    ProviderReasoning {
        /// Fresh semantic-entry identity.
        entry: SemanticTranscriptEntryId,
    },
    /// One logical request plus its reference-only semantic entry.
    ToolCall {
        /// Fresh semantic-entry identity.
        entry: SemanticTranscriptEntryId,
        /// Fresh logical request identity.
        request: ToolRequestId,
        /// The explicit initial approval outcome selected by application policy.
        approval: InitialToolApproval,
    },
}

impl ToolResponsePartIdentity {
    /// Constructs a text-part identity.
    pub const fn text(entry: SemanticTranscriptEntryId) -> Self {
        Self::Text { entry }
    }

    /// Constructs a provider-compaction-part identity.
    pub const fn provider_compaction(entry: SemanticTranscriptEntryId) -> Self {
        Self::ProviderCompaction { entry }
    }

    /// Constructs a provider-reasoning-part identity.
    pub const fn provider_reasoning(entry: SemanticTranscriptEntryId) -> Self {
        Self::ProviderReasoning { entry }
    }

    /// Constructs a tool-part identity and explicit initial policy outcome.
    pub const fn tool_call(
        entry: SemanticTranscriptEntryId,
        request: ToolRequestId,
        approval: InitialToolApproval,
    ) -> Self {
        Self::ToolCall {
            entry,
            request,
            approval,
        }
    }
}

/// Fresh identities for one nonterminal tool-using response commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRoundModelCallIdentities {
    pub(super) response_parts: Vec<ToolResponsePartIdentity>,
    pub(super) yielded_frontier: ContextFrontierId,
    pub(super) continuation_attempt: Option<TurnAttemptId>,
}

impl ToolRoundModelCallIdentities {
    /// Supplies one identity shape per response part, a yielded frontier, and
    /// a continuation attempt exactly when every request is auto-approved.
    pub fn new(
        response_parts: Vec<ToolResponsePartIdentity>,
        yielded_frontier: ContextFrontierId,
        continuation_attempt: Option<TurnAttemptId>,
    ) -> Self {
        Self {
            response_parts,
            yielded_frontier,
            continuation_attempt,
        }
    }

    /// Returns ordered response-part identities.
    pub fn response_parts(&self) -> &[ToolResponsePartIdentity] {
        &self.response_parts
    }

    /// Returns the proposed yielded snapshot identity.
    pub const fn yielded_frontier(&self) -> ContextFrontierId {
        self.yielded_frontier
    }

    /// Returns the proposed continuation attempt, if the batch has no wait.
    pub const fn continuation_attempt(&self) -> Option<TurnAttemptId> {
        self.continuation_attempt
    }
}

/// Fresh identities for one response part when an applied interrupt closes
/// newly proposed tools instead of continuing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoppedToolResponsePartIdentity {
    /// One semantic assistant-text entry.
    Text {
        /// Fresh semantic-entry identity.
        entry: SemanticTranscriptEntryId,
    },
    /// One semantic provider-compaction entry.
    ProviderCompaction {
        /// Fresh semantic-entry identity.
        entry: SemanticTranscriptEntryId,
    },
    /// One semantic provider-reasoning entry.
    ProviderReasoning {
        /// Fresh semantic-entry identity.
        entry: SemanticTranscriptEntryId,
    },
    /// One request, tool-use entry, and turn-closed result entry.
    ToolCall {
        /// Fresh assistant tool-use entry identity.
        entry: SemanticTranscriptEntryId,
        /// Fresh logical request identity.
        request: ToolRequestId,
        /// Fresh reference-only closed-result entry identity.
        closed_result_entry: SemanticTranscriptEntryId,
        /// Frozen policy outcome for the request.
        approval: InitialToolApproval,
    },
}

impl StoppedToolResponsePartIdentity {
    /// Constructs one text identity.
    pub const fn text(entry: SemanticTranscriptEntryId) -> Self {
        Self::Text { entry }
    }

    /// Constructs one provider-compaction identity.
    pub const fn provider_compaction(entry: SemanticTranscriptEntryId) -> Self {
        Self::ProviderCompaction { entry }
    }

    /// Constructs one provider-reasoning identity.
    pub const fn provider_reasoning(entry: SemanticTranscriptEntryId) -> Self {
        Self::ProviderReasoning { entry }
    }

    /// Constructs one closed tool-proposal identity group.
    pub const fn tool_call(
        entry: SemanticTranscriptEntryId,
        request: ToolRequestId,
        closed_result_entry: SemanticTranscriptEntryId,
        approval: InitialToolApproval,
    ) -> Self {
        Self::ToolCall {
            entry,
            request,
            closed_result_entry,
            approval,
        }
    }
}

/// Fresh identities for a tool-using response closed by an applied interrupt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoppedToolRoundModelCallIdentities {
    pub(super) response_parts: Vec<StoppedToolResponsePartIdentity>,
    pub(super) cancellation_entry: SemanticTranscriptEntryId,
    pub(super) terminal_frontier: ContextFrontierId,
    pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

impl StoppedToolRoundModelCallIdentities {
    /// Supplies ordered response identities, the cancellation marker, and
    /// terminal snapshot.
    pub fn new(
        response_parts: Vec<StoppedToolResponsePartIdentity>,
        cancellation_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            response_parts,
            cancellation_entry,
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one successor identity per pending steering input.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }
}

/// Fresh identities for a failed-turn outcome transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailedModelCallTurnIdentities {
    pub(super) failure_entry: SemanticTranscriptEntryId,
    pub(super) terminal_frontier: ContextFrontierId,
    pub(super) pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

impl FailedModelCallTurnIdentities {
    /// Supplies the failure marker and terminal-frontier identities.
    pub fn new(
        failure_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            failure_entry,
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one fresh successor identity per pending steering input, in
    /// session acceptance order.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }

    /// Returns the failure-marker identity this bundle mints.
    ///
    /// Exposed alongside [`Self::terminal_frontier`] because the two are
    /// minted into one collision domain: a retry after an identity collision
    /// has to refresh *both*, and a caller that can only compare whole
    /// bundles cannot tell a full refresh from one that reused half.
    pub const fn failure_entry(&self) -> SemanticTranscriptEntryId {
        self.failure_entry
    }

    /// Returns the terminal-frontier identity this bundle mints.
    pub const fn terminal_frontier(&self) -> ContextFrontierId {
        self.terminal_frontier
    }
}

/// Fresh identities for an interrupt-cancelled turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelledModelCallTurnIdentities {
    pub(super) cancellation_entry: SemanticTranscriptEntryId,
    pub(super) terminal_frontier: ContextFrontierId,
    pub(super) pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

/// Fresh identities for a physical-cancellation observation.
///
/// The freshly reloaded attempt decides whether the terminal entry is a
/// proof-bearing cancellation marker or an ordinary failure marker. This
/// shape lets the application mint one collision domain without guessing
/// whether a concurrent interrupt committed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhysicalCancellationModelCallTurnIdentities {
    pub(super) terminal_entry: SemanticTranscriptEntryId,
    pub(super) terminal_frontier: ContextFrontierId,
    pub(super) pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

impl PhysicalCancellationModelCallTurnIdentities {
    /// Supplies the terminal-marker and terminal-frontier identities.
    pub fn new(
        terminal_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            terminal_entry,
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one fresh successor identity per pending steering input, in
    /// session acceptance order.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }
}

impl CancelledModelCallTurnIdentities {
    /// Supplies the cancellation marker and terminal-frontier identities.
    pub fn new(
        cancellation_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            cancellation_entry,
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one fresh successor identity per pending steering input, in
    /// session acceptance order.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }

    /// Reuses the terminal frontier and pending-steering successors when an
    /// interrupt closes an existing ambiguity wait instead of emitting a
    /// cancellation marker.
    pub fn into_ambiguous(self) -> AmbiguousModelCallTurnIdentities {
        AmbiguousModelCallTurnIdentities {
            terminal_frontier: self.terminal_frontier,
            pending_steering_reclassifications: self.pending_steering_reclassifications,
        }
    }
}

/// Fresh identity for a refusal terminal frontier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefusedModelCallTurnIdentities {
    pub(super) provider_compaction_entries: Vec<SemanticTranscriptEntryId>,
    pub(super) terminal_frontier: ContextFrontierId,
    pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

impl RefusedModelCallTurnIdentities {
    /// Supplies the new equal-content terminal frontier identity.
    pub fn new(terminal_frontier: ContextFrontierId) -> Self {
        Self {
            provider_compaction_entries: Vec::new(),
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one semantic identity per retained provider compaction block.
    pub fn with_provider_compaction_entries(
        mut self,
        identities: Vec<SemanticTranscriptEntryId>,
    ) -> Self {
        self.provider_compaction_entries = identities;
        self
    }

    /// Supplies one fresh successor identity per pending steering input, in
    /// session acceptance order.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }
}

/// Candidate identities matching one possible terminal observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallTerminalIdentities {
    /// Successful assistant-content and completion identities.
    Completed(CompletedModelCallIdentities),
    /// Assistant content, logical requests, and a nonterminal yielded frontier.
    ToolRound(ToolRoundModelCallIdentities),
    /// Tool response content and closed results under an applied interrupt.
    StoppedToolRound(StoppedToolRoundModelCallIdentities),
    /// Known-failure or cause-free physical-cancellation identities.
    Failed(FailedModelCallTurnIdentities),
    /// Physical-cancellation identities whose semantic meaning is selected
    /// from the freshly reloaded stop state.
    PhysicalCancellation(PhysicalCancellationModelCallTurnIdentities),
    /// Refusal terminal-frontier identity.
    Refused(RefusedModelCallTurnIdentities),
    /// Ambiguity identities used only when a stop requires terminal
    /// reconciliation; ordinary ambiguity ignores them while retaining the
    /// slot.
    Ambiguous(AmbiguousModelCallTurnIdentities),
}

/// One terminal or durable-wait result from the observation transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallTerminalOutcome {
    /// Assistant content and turn completion committed atomically.
    Completed(CompletedModelCallTurn),
    /// Assistant content and requests committed while the same turn continues.
    ToolRound(ToolRoundModelCallTurn),
    /// A tool-using response raced an interrupt and closed without execution.
    CancelledWithToolResponse(CancelledToolRoundModelCallTurn),
    /// The call and turn failed atomically.
    Failed(FailedModelCallTurn),
    /// The applied interrupt and physical evidence cancelled the turn.
    Cancelled(CancelledModelCallTurn),
    /// The provider refusal terminalized the turn.
    Refused(RefusedModelCallTurn),
    /// An applied interrupt and exact ambiguity set require reconciliation.
    ReconciliationRequired(ReconciliationRequiredModelCallTurn),
    /// Physical ambiguity ended the attempt and retained the slot.
    AwaitingRecovery(AmbiguousModelCallTurn),
}

/// Result of atomically applying one matching interrupt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallInterruptOutcome {
    /// Unsent work ended directly and released the turn slot.
    Cancelled(CancelledModelCallTurn),
    /// Issued work retained the slot while durable cancellation was requested.
    CancellationRequested(StopRequestedModelCallTurn),
    /// An existing physical-ambiguity wait closed under the applied interrupt.
    ReconciliationRequired(ReconciliationRequiredModelCallTurn),
    /// An existing tool-attempt ambiguity closed under the applied interrupt.
    ToolReconciliationRequired(ReconciliationRequiredToolTurn),
}

impl ModelCallTerminalIdentities {
    pub(super) fn pending_steering_reclassifications(
        &self,
    ) -> &[PendingSteeringReclassificationIdentity] {
        match self {
            Self::Completed(identities) => &identities.pending_steering_reclassifications,
            Self::ToolRound(_) => &[],
            Self::StoppedToolRound(identities) => &identities.pending_steering_reclassifications,
            Self::Failed(identities) => &identities.pending_steering_reclassifications,
            Self::PhysicalCancellation(identities) => {
                &identities.pending_steering_reclassifications
            }
            Self::Refused(identities) => &identities.pending_steering_reclassifications,
            Self::Ambiguous(identities) => &identities.pending_steering_reclassifications,
        }
    }
}

/// Fresh identities needed only when ambiguity terminalizes under a stop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AmbiguousModelCallTurnIdentities {
    pub(super) terminal_frontier: ContextFrontierId,
    pub(super) pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

impl AmbiguousModelCallTurnIdentities {
    /// Supplies the candidate terminal frontier for proof-bearing
    /// reconciliation.
    pub const fn new(terminal_frontier: ContextFrontierId) -> Self {
        Self {
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one fresh successor identity per pending steering input.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }
}
