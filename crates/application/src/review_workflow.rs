//! Durable review-workflow command and read orchestration.

use std::future::Future;

use signalbox_domain::{
    DurableCommandId, ReviewExternalLink, ReviewExternalLinkAttachment, ReviewExternalLinkId,
    ReviewFinding, ReviewFindingEvent, ReviewFindingId, ReviewFindingStatus, ReviewKey, ReviewPass,
    ReviewPassEvidence, ReviewPassId, ReviewPassState, ReviewRun, ReviewRunId, ReviewTarget,
    ReviewTargetId,
};

/// One user-global durable review-workflow command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewWorkflowCommand {
    command_id: DurableCommandId,
    semantic_digest: [u8; 32],
    operation: ReviewWorkflowOperation,
}

impl ReviewWorkflowCommand {
    /// Binds one checked operation to its durable command identity.
    pub const fn new(
        command_id: DurableCommandId,
        semantic_digest: [u8; 32],
        operation: ReviewWorkflowOperation,
    ) -> Self {
        Self {
            command_id,
            semantic_digest,
            operation,
        }
    }

    /// Returns the user-global durable command identity.
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }

    /// Returns the digest of the validated closed semantic request shape.
    pub const fn semantic_digest(&self) -> [u8; 32] {
        self.semantic_digest
    }

    /// Borrows the complete typed operation.
    pub const fn operation(&self) -> &ReviewWorkflowOperation {
        &self.operation
    }
}

/// Closed mutation units admitted through the process protocol.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewWorkflowOperation {
    /// Register one immutable target snapshot.
    CreateTarget(ReviewTarget),
    /// Admit one queued run and its sole queued pass through crash-recoverable steps.
    StartRun { run: ReviewRun, pass: ReviewPass },
    /// Atomically project one queued run and pass into their active turn.
    ActivatePass { run: ReviewRun, pass: ReviewPass },
    /// Atomically project one queued-or-active run and pass into a result-free terminal state.
    CompletePass { run: ReviewRun, pass: ReviewPass },
    /// Atomically bind a read-only result and its complete finding inventory.
    RecordFindings {
        pass: ReviewPassEvidence,
        findings: Vec<ReviewFinding>,
    },
    /// Atomically bind a finding-event result and append its exact event.
    RecordFindingEvent {
        pass: ReviewPassEvidence,
        event: ReviewFindingEvent,
    },
    /// Reserve one external-link identity before a provider write.
    ReserveExternalLink(ReviewExternalLink),
    /// Bind one provider identity through the exact attaching pass result.
    AttachExternalLink {
        link: ReviewExternalLinkId,
        attachment: ReviewExternalLinkAttachment,
    },
}

/// Stable discriminator used to inspect a durable command before aggregate reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewWorkflowOperationKind {
    /// Register one target snapshot.
    CreateTarget,
    /// Admit one run and pass.
    StartRun,
    /// Activate one pass.
    ActivatePass,
    /// Complete one pass.
    CompletePass,
    /// Record one complete finding inventory.
    RecordFindings,
    /// Record one finding event.
    RecordFindingEvent,
    /// Reserve one external-link identity.
    ReserveExternalLink,
    /// Attach one external-link identity.
    AttachExternalLink,
}

impl ReviewWorkflowOperation {
    /// Returns the stable discriminator for this operation.
    pub const fn kind(&self) -> ReviewWorkflowOperationKind {
        match self {
            Self::CreateTarget(_) => ReviewWorkflowOperationKind::CreateTarget,
            Self::StartRun { .. } => ReviewWorkflowOperationKind::StartRun,
            Self::ActivatePass { .. } => ReviewWorkflowOperationKind::ActivatePass,
            Self::CompletePass { .. } => ReviewWorkflowOperationKind::CompletePass,
            Self::RecordFindings { .. } => ReviewWorkflowOperationKind::RecordFindings,
            Self::RecordFindingEvent { .. } => ReviewWorkflowOperationKind::RecordFindingEvent,
            Self::ReserveExternalLink(_) => ReviewWorkflowOperationKind::ReserveExternalLink,
            Self::AttachExternalLink { .. } => ReviewWorkflowOperationKind::AttachExternalLink,
        }
    }
}

/// Closed result-free terminal status recorded for one completed pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewPassCompletionStatus {
    /// The pass completed successfully.
    Succeeded,
    /// The pass failed definitively.
    Failed,
    /// The pass requires reconciliation.
    Blocked,
    /// The pass was cancelled.
    Cancelled,
}

impl ReviewPassCompletionStatus {
    /// Derives a result-free terminal status from a pass projection.
    pub const fn from_state(state: &ReviewPassState) -> Option<Self> {
        match state {
            ReviewPassState::Succeeded { result: None, .. } => Some(Self::Succeeded),
            ReviewPassState::Failed { .. } => Some(Self::Failed),
            ReviewPassState::Blocked { result: None, .. } => Some(Self::Blocked),
            ReviewPassState::Cancelled { .. } => Some(Self::Cancelled),
            ReviewPassState::Queued
            | ReviewPassState::Running { .. }
            | ReviewPassState::Succeeded {
                result: Some(_), ..
            }
            | ReviewPassState::Blocked {
                result: Some(_), ..
            } => None,
        }
    }
}

/// Stable recorded result returned equally for every exact retry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewWorkflowCommandResult {
    /// Target registration succeeded.
    TargetCreated { target: ReviewTargetId },
    /// Run and pass admission succeeded.
    RunStarted {
        run: ReviewRunId,
        pass: ReviewPassId,
    },
    /// Run and pass activation succeeded.
    PassActivated {
        run: ReviewRunId,
        pass: ReviewPassId,
    },
    /// One run and pass reached the requested result-free terminal state.
    PassCompleted {
        run: ReviewRunId,
        pass: ReviewPassId,
        status: ReviewPassCompletionStatus,
    },
    /// A complete finding inventory was committed.
    FindingsRecorded {
        run: ReviewRunId,
        pass: ReviewPassId,
        finding_count: usize,
    },
    /// One finding event was committed.
    FindingEventRecorded {
        finding: ReviewFindingId,
        status: ReviewFindingStatus,
    },
    /// One external-link reservation was committed or equally replayed.
    ExternalLinkReserved { link: ReviewExternalLinkId },
    /// One external-link attachment was committed.
    ExternalLinkAttached {
        link: ReviewExternalLinkId,
        external_object: ReviewKey,
    },
}

/// Closed outcome of the user-global durable command claim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewWorkflowCommandOutcome {
    /// First handling or an equal retry returned the recorded result.
    Recorded(ReviewWorkflowCommandResult),
    /// The command identity already names a different typed payload.
    ConflictingReuse { command_id: DurableCommandId },
}

/// Crash-recoverable durable-command handling boundary.
pub trait ReviewWorkflowTransaction {
    /// Adapter-specific infrastructure or integrity failure.
    type Error;

    /// Claims, applies the crash-recoverable aggregate mutations, and records their receipt.
    fn handle(
        &mut self,
        command: ReviewWorkflowCommand,
    ) -> impl Future<Output = Result<ReviewWorkflowCommandOutcome, Self::Error>> + Send;
}

/// Coordinates durable review-workflow mutation handling.
#[derive(Debug)]
pub struct ReviewWorkflowCommandService<Transaction> {
    transaction: Transaction,
}

impl<Transaction> ReviewWorkflowCommandService<Transaction> {
    /// Composes the use case with its atomic transaction port.
    pub const fn new(transaction: Transaction) -> Self {
        Self { transaction }
    }
}

impl<Transaction: ReviewWorkflowTransaction> ReviewWorkflowCommandService<Transaction> {
    /// Delegates one already-domain-checked command exactly once.
    pub async fn execute(
        &mut self,
        command: ReviewWorkflowCommand,
    ) -> Result<ReviewWorkflowCommandOutcome, Transaction::Error> {
        self.transaction.handle(command).await
    }
}

/// Application-owned read boundary for review aggregates and lists.
pub trait ReviewWorkflowReader {
    /// Adapter-specific infrastructure or integrity failure.
    type Error;

    /// Loads one immutable target snapshot.
    fn load_target(
        &self,
        target: ReviewTargetId,
    ) -> impl Future<Output = Result<Option<ReviewTarget>, Self::Error>> + Send;
    /// Loads one run projection.
    fn load_run(
        &self,
        run: ReviewRunId,
    ) -> impl Future<Output = Result<Option<ReviewRun>, Self::Error>> + Send;
    /// Loads one run and its optional recorded pass from one coherent snapshot.
    fn load_run_with_pass(
        &self,
        run: ReviewRunId,
    ) -> impl Future<Output = Result<Option<(ReviewRun, Option<ReviewPass>)>, Self::Error>> + Send;
    /// Loads one pass projection.
    fn load_pass(
        &self,
        pass: ReviewPassId,
    ) -> impl Future<Output = Result<Option<ReviewPass>, Self::Error>> + Send;
    /// Loads one complete finding aggregate.
    fn load_finding(
        &self,
        finding: ReviewFindingId,
    ) -> impl Future<Output = Result<Option<ReviewFinding>, Self::Error>> + Send;
    /// Lists complete findings for one exact run in identity order.
    fn list_findings(
        &self,
        run: ReviewRunId,
    ) -> impl Future<Output = Result<Vec<ReviewFinding>, Self::Error>> + Send;
}
