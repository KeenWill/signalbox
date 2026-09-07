//! Recorded submit-input acceptance and rejection results for
//! `docs/spec/turn-lifecycle-and-scheduling.md`.

use crate::AcceptedInputDisposition;
use crate::AcceptedInputId;
use crate::AcceptedInputQueueOrder;
use crate::AppliedInterruptCommandResult;
use crate::BlobDigest;
use crate::DurableCommandId;
use crate::ModelAlias;
use crate::OriginConfiguration;
use crate::SessionConfigurationDefaultsVersion;
use crate::SessionId;
use crate::SessionInputPosition;
use crate::SteeringBinding;
use crate::TurnId;
use std::hash::Hash;

/// The terminal recorded result of one canonical input command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubmitInputResult {
    /// The input was durably accepted with one treatment-specific effect.
    Applied(SubmitInputAppliedResult),
    /// Authoritative state rejected the caller's requested treatment.
    Rejected(SubmitInputRejectedResult),
}

/// The exact applied acceptance shape.
///
/// Both variants contain private-field values sealed behind authoritative
/// preparation and checked reconstitution. Pending steering cannot carry a
/// turn candidate, queue order, or configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubmitInputAppliedResult {
    /// Acceptance created ordinary accepted-input-origin work.
    TurnOrigin(SubmitInputTurnOriginAppliedResult),
    /// Acceptance created pending steering bound to the exact active turn.
    PendingSteering(SubmitInputPendingSteeringAppliedResult),
}

impl SubmitInputAppliedResult {
    /// Returns the durable accepted-input identity.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        match self {
            Self::TurnOrigin(result) => result.accepted_input,
            Self::PendingSteering(result) => result.accepted_input,
        }
    }

    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        match self {
            Self::TurnOrigin(result) => result.session,
            Self::PendingSteering(result) => result.session,
        }
    }

    /// Returns the immutable session acceptance position.
    pub const fn acceptance_position(&self) -> SessionInputPosition {
        match self {
            Self::TurnOrigin(result) => result.acceptance_position,
            Self::PendingSteering(result) => result.acceptance_position,
        }
    }

    /// Returns the exact initial durable disposition.
    pub const fn disposition(&self) -> AcceptedInputDisposition {
        match self {
            Self::TurnOrigin(result) => AcceptedInputDisposition::OriginOf(result.turn),
            Self::PendingSteering(result) => AcceptedInputDisposition::PendingSteering {
                binding: result.binding,
            },
        }
    }

    /// Borrows turn-origin fields when this acceptance created logical work.
    pub const fn turn_origin(&self) -> Option<&SubmitInputTurnOriginAppliedResult> {
        match self {
            Self::TurnOrigin(result) => Some(result),
            Self::PendingSteering(_) => None,
        }
    }

    /// Borrows pending-steering fields when acceptance created no turn.
    pub const fn pending_steering(&self) -> Option<&SubmitInputPendingSteeringAppliedResult> {
        match self {
            Self::PendingSteering(result) => Some(result),
            Self::TurnOrigin(_) => None,
        }
    }
}

/// The complete applied receipt for accepted-input-origin work.
///
/// Raw facts cannot construct this private-field value.
///
/// ```compile_fail
/// # use signalbox_domain::SubmitInputTurnOriginAppliedResult;
/// fn bypass_checked_construction(result: &SubmitInputTurnOriginAppliedResult) {
///     let _ = result.turn;
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmitInputTurnOriginAppliedResult {
    pub(super) accepted_input: AcceptedInputId,
    pub(super) session: SessionId,
    pub(super) acceptance_position: SessionInputPosition,
    pub(super) turn: TurnId,
    pub(super) queue_order: AcceptedInputQueueOrder,
    pub(super) origin_configuration: Box<OriginConfiguration>,
    pub(super) applied_interrupt: Option<Box<AppliedInterruptCommandResult>>,
}

impl SubmitInputTurnOriginAppliedResult {
    /// Returns the durable accepted-input identity.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the future queued logical-work identity.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the initial durable disposition.
    pub const fn disposition(&self) -> AcceptedInputDisposition {
        AcceptedInputDisposition::OriginOf(self.turn)
    }

    /// Returns the complete ordinary queue-order fact.
    pub const fn queue_order(&self) -> AcceptedInputQueueOrder {
        self.queue_order
    }

    /// Returns the immutable session acceptance position.
    pub const fn acceptance_position(&self) -> SessionInputPosition {
        self.acceptance_position
    }

    /// Borrows the complete frozen origin configuration.
    pub const fn origin_configuration(&self) -> &OriginConfiguration {
        &self.origin_configuration
    }

    /// Borrows the exact applied-interrupt authority when this origin
    /// immediately succeeds the interrupted active turn.
    pub const fn applied_interrupt(&self) -> Option<&AppliedInterruptCommandResult> {
        match &self.applied_interrupt {
            Some(result) => Some(result),
            None => None,
        }
    }

    /// Constructs the durable settings event that belongs to this accepted
    /// origin and its frozen configuration.
    pub fn model_settings_event(&self) -> Option<crate::TurnModelSettingsResolved> {
        crate::TurnModelSettingsResolved::try_new(
            self.accepted_input,
            self.turn,
            self.origin_configuration.session_defaults_version(),
            *self.origin_configuration.effective().model(),
            self.origin_configuration
                .requested()
                .per_call_model_settings(),
            self.origin_configuration.effective().model_settings(),
            self.origin_configuration.model_settings_adjusted_from(),
            self.origin_configuration
                .model_settings_adjustments()
                .to_vec(),
        )
    }
}

/// The complete applied receipt for pending steering.
///
/// This shape has no turn-origin, queue-order, or configuration field.
///
/// ```compile_fail
/// # use signalbox_domain::SubmitInputPendingSteeringAppliedResult;
/// fn bypass_checked_construction(result: &SubmitInputPendingSteeringAppliedResult) {
///     let _ = result.binding;
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubmitInputPendingSteeringAppliedResult {
    pub(super) accepted_input: AcceptedInputId,
    pub(super) session: SessionId,
    pub(super) acceptance_position: SessionInputPosition,
    pub(super) binding: SteeringBinding,
}

impl SubmitInputPendingSteeringAppliedResult {
    /// Returns the durable accepted-input identity.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the immutable session acceptance position.
    pub const fn acceptance_position(&self) -> SessionInputPosition {
        self.acceptance_position
    }

    /// Returns the exact active-turn steering binding.
    pub const fn binding(&self) -> SteeringBinding {
        self.binding
    }
}

/// Typed authoritative input-acceptance rejections.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SubmitInputRejectedResult {
    /// An attachment digest had no catalogued verified replica.
    AttachmentBlobNotFound {
        /// The unavailable immutable byte identity.
        digest: BlobDigest,
    },
    /// Distinct attachment bytes exceeded the deployment ceiling.
    AttachmentByteBudgetExceeded {
        /// The configured maximum aggregate byte count.
        maximum_bytes: u64,
    },
    /// The target session did not exist.
    SessionNotFound {
        /// The absent target.
        session: SessionId,
    },
    /// An active-work request named a turn while the session had none.
    NoActiveTurn {
        /// The target session.
        session: SessionId,
        /// The turn the caller expected to be active.
        expected_active_turn: TurnId,
    },
    /// A no-active-turn start was submitted while a turn owned the slot.
    ActiveTurnPresent {
        /// The target session.
        session: SessionId,
        /// The authoritative active turn.
        active_turn: TurnId,
    },
    /// An active-work request named a stale turn.
    ActiveTurnMismatch {
        /// The target session.
        session: SessionId,
        /// The turn named by the command.
        expected_active_turn: TurnId,
        /// The authoritative active turn.
        actual_active_turn: TurnId,
    },
    /// The caller's expected defaults version was no longer current.
    SessionDefaultsVersionMismatch {
        /// The target session.
        session: SessionId,
        /// The caller's expected version.
        expected: SessionConfigurationDefaultsVersion,
        /// The authoritative current version.
        current: SessionConfigurationDefaultsVersion,
    },
    /// The requested alias had no selectable current definition.
    UnknownModelAlias {
        /// The target session.
        session: SessionId,
        /// The unresolved alias.
        alias: ModelAlias,
    },
    /// The session's positive input-position ordinal had no successor.
    AcceptancePositionExhausted {
        /// The target session.
        session: SessionId,
        /// The maximum recorded position.
        last: SessionInputPosition,
    },
    /// A safe-point request arrived after interruption had already stopped the
    /// active attempt from authorizing more semantic work.
    ///
    /// Recorded by earlier daemons only; a stopping turn now accepts steering,
    /// and this variant survives for replay of those records.
    SafePointUnavailableWhileStopping {
        /// The target session.
        session: SessionId,
        /// The exact active turn retaining the slot.
        active_turn: TurnId,
        /// The command whose applied result is already stopping the turn.
        existing_command: DurableCommandId,
    },
    /// A distinct later interrupt cannot replace the exact proof already
    /// applied to the active turn.
    InterruptAlreadyApplied {
        /// The target session.
        session: SessionId,
        /// The exact active turn retaining the slot.
        active_turn: TurnId,
        /// The command whose applied result remains cancellation authority.
        existing_command: DurableCommandId,
    },
    /// An interrupt arrived while a parked approval wait held the active
    /// slot; the wait remains parked until its canonical decision command
    /// resolves the approval obligation.
    InterruptUnavailableWhileAwaitingApproval {
        /// The target session.
        session: SessionId,
        /// The exact active turn retaining the slot on its approval wait.
        active_turn: TurnId,
    },
}
