//! Transcript wire representations and validation.

use crate::delegation::{
    DelegationOutcome, DelegationProvenance, DelegationReason, DelegationWaitMode,
    TranscriptToolApproval, child_result_shape_is_valid, delegation_content_is_valid,
    delegation_terminal_outcome_reason_is_admissible, parent_delegation_provenance_has_cascade,
};
use crate::scalars::{
    CanonicalU64, CanonicalUuid, FrameValidationError, InputContent, MAX_CONTENT_FRAGMENT_BYTES,
    PositiveCanonicalU64, deserialize_optional_non_null, deserialize_required_nullable,
};
use crate::user_input::UserInputContent;
use serde::{Deserialize, Deserializer, Serialize};

/// Durable nonterminal model-call state carried by a transcript snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CurrentModelCallState {
    /// Call is prepared but unsent.
    Prepared {},
    /// Call crossed the send boundary.
    InFlight {},
    /// Cancellation was durably requested for the issued call.
    CancellationRequested {},
}

/// Current model call attached to one running turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentModelCall {
    model_call_id: CanonicalUuid,
    state: CurrentModelCallState,
}

impl CurrentModelCall {
    /// Constructs one exact current-call projection.
    pub const fn new(model_call_id: CanonicalUuid, state: CurrentModelCallState) -> Self {
        Self {
            model_call_id,
            state,
        }
    }

    /// Returns the current model-call identity.
    pub const fn model_call_id(&self) -> CanonicalUuid {
        self.model_call_id
    }

    /// Returns the exact durable nonterminal state.
    pub const fn state(&self) -> CurrentModelCallState {
        self.state
    }
}

/// Terminal model-call dispositions admitted by a failed transcript turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailedModelCallDisposition {
    /// The provider interaction failed with definitive evidence.
    KnownFailed,
    /// The provider call was cancelled without terminalizing the turn as
    /// cancelled.
    Cancelled,
}

/// Closed terminal model-call failure classifications exposed to clients.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailedModelCallCause {
    /// Distinct rendered attachments exceeded the deployment verification bound.
    AttachmentTooLarge,
    /// No recorded replica contained a required rendered attachment.
    AttachmentMissing,
    /// Recorded replicas failed attachment identity verification.
    AttachmentCorrupt,
    /// The provider rejected the request credential.
    CredentialRejected,
    /// The credential lacked permission.
    PermissionDenied,
    /// The provider judged the request invalid.
    InvalidRequest,
    /// The requested model or resource was not found.
    TargetNotFound,
    /// The request exceeded a provider size limit.
    RequestTooLarge,
    /// The provider applied a transient rate limit.
    RateLimited,
    /// The account's available quota was exhausted.
    QuotaExhausted,
    /// The provider reported overload.
    Overloaded,
    /// The provider reported an internal error.
    ProviderInternal,
    /// The adapter did not recognize the definitive provider error.
    Unrecognized,
}

/// Optional terminal call evidence carried by a failed transcript turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FailedTerminalModelCall {
    model_call_id: CanonicalUuid,
    disposition: FailedModelCallDisposition,
    #[serde(skip_serializing_if = "Option::is_none")]
    cause: Option<FailedModelCallCause>,
}

impl FailedTerminalModelCall {
    /// Constructs one exact failed-turn terminal-call projection.
    pub const fn new(
        model_call_id: CanonicalUuid,
        disposition: FailedModelCallDisposition,
    ) -> Self {
        Self {
            model_call_id,
            disposition,
            cause: None,
        }
    }

    /// Constructs one known-failed call with its closed failure classification.
    pub const fn known_failed_with_cause(
        model_call_id: CanonicalUuid,
        cause: FailedModelCallCause,
    ) -> Self {
        Self {
            model_call_id,
            disposition: FailedModelCallDisposition::KnownFailed,
            cause: Some(cause),
        }
    }

    /// Returns the terminal model-call identity.
    pub const fn model_call_id(&self) -> CanonicalUuid {
        self.model_call_id
    }

    /// Returns the exact terminal call disposition.
    pub const fn disposition(&self) -> FailedModelCallDisposition {
        self.disposition
    }

    /// Returns the closed failure classification when retained.
    pub const fn cause(&self) -> Option<FailedModelCallCause> {
        self.cause
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFailedTerminalModelCall {
    model_call_id: CanonicalUuid,
    disposition: FailedModelCallDisposition,
    #[serde(
        default,
        deserialize_with = "deserialize_present_failed_model_call_cause"
    )]
    cause: Option<FailedModelCallCause>,
}

// Field default handles omission; invoking this decoder means the member was
// present, so a JSON null must fail instead of collapsing into `None`.
fn deserialize_present_failed_model_call_cause<'de, DeserializerT>(
    deserializer: DeserializerT,
) -> Result<Option<FailedModelCallCause>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
{
    FailedModelCallCause::deserialize(deserializer).map(Some)
}

impl<'de> Deserialize<'de> for FailedTerminalModelCall {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let raw = RawFailedTerminalModelCall::deserialize(deserializer)?;
        if raw.cause.is_some() && raw.disposition != FailedModelCallDisposition::KnownFailed {
            return Err(serde::de::Error::custom(
                "failure cause requires a known-failed disposition",
            ));
        }
        Ok(Self {
            model_call_id: raw.model_call_id,
            disposition: raw.disposition,
            cause: raw.cause,
        })
    }
}

/// Authoritative turn state carried by a transcript snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TurnState {
    /// Accepted work has not activated.
    Queued {
        /// Accepted input that created the queued turn.
        accepted_input_id: CanonicalUuid,
        /// Exact ordered accepted user parts.
        content: UserInputContent,
    },
    /// Delegated work has not activated.
    QueuedDelegated {
        /// Tool request that spawned the delegated session.
        spawning_request_id: CanonicalUuid,
        /// Parent session that issued the spawn request.
        parent_session_id: CanonicalUuid,
        /// Parent turn that issued the spawn request.
        parent_turn_id: CanonicalUuid,
        /// Exact delegated task text.
        content: InputContent,
    },
    /// Delivered delegation content is queued to wake an idle recipient.
    QueuedDelegationWake {
        /// First recipient-wide delivery sequence included by the wake.
        first_delivery_sequence: CanonicalU64,
        /// Last recipient-wide delivery sequence included by the wake.
        through_delivery_sequence: CanonicalU64,
    },
    /// A parent command logically terminalized delegated work while retained
    /// physical execution evidence remains inert.
    DelegationTerminated {
        /// Tool request that spawned the child.
        spawning_request_id: CanonicalUuid,
        /// Typed stopped or cancelled outcome.
        outcome: DelegationOutcome,
        /// Exact parent terminal reason.
        reason: DelegationReason,
        /// Exact parent-command provenance.
        provenance: DelegationProvenance,
    },
    /// The turn is running its current attempt.
    ActiveRunning {
        /// Current live attempt.
        current_attempt_id: CanonicalUuid,
        /// Current provider call, or null before one is prepared.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        current_model_call: Option<CurrentModelCall>,
    },
    /// The turn is parked on an ambiguous model call.
    ActiveAwaitingModelCallRecovery {
        /// Ended attempt that issued the call.
        ended_attempt_id: CanonicalUuid,
        /// Ambiguous call awaiting recovery.
        recovery_model_call_id: CanonicalUuid,
        /// Durable automatic reconciliation attempts already claimed.
        automatic_reconciliation_attempts: CanonicalU64,
        /// True only when the automatic attempt budget is exhausted.
        operator_action_required: bool,
    },
    /// The turn is parked on a user decision for a tool request.
    ActiveAwaitingToolApproval {
        /// Earliest undecided tool request.
        tool_request_id: CanonicalUuid,
    },
    /// The turn is parked on a foreground delegated-child result.
    ActiveAwaitingChild {
        /// Tool request that issued the await.
        await_request_id: CanonicalUuid,
        /// Spawn request naming the relationship.
        spawning_request_id: CanonicalUuid,
        /// Exact child whose result releases the turn.
        child_session_id: CanonicalUuid,
    },
    /// The turn is parked on an ambiguous tool attempt.
    ActiveAwaitingToolRecovery {
        /// Ended turn attempt that issued the tool effect.
        ended_attempt_id: CanonicalUuid,
        /// Ambiguous tool attempt awaiting recovery.
        recovery_tool_attempt_id: CanonicalUuid,
        /// Durable automatic reconciliation attempts already claimed.
        automatic_reconciliation_attempts: CanonicalU64,
        /// True only when the automatic attempt budget is exhausted.
        operator_action_required: bool,
    },
    /// The turn is parked on replacement of one exact lost runner placement.
    ActiveAwaitingRunnerRecovery {
        /// Runner whose durable loss owns this wait.
        runner_id: CanonicalUuid,
        /// Positive placement revision against which loss was projected.
        placement_revision: PositiveCanonicalU64,
        /// Physical tool attempt interrupted by loss, or null when none exists.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        tool_attempt_id: Option<CanonicalUuid>,
    },
    /// The turn terminalized as failed.
    Failed {
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
        /// Terminal physical attempt, or null for an evidence-free recovery
        /// failure.
        terminal_attempt_id: Option<CanonicalUuid>,
        /// Terminal call evidence, or null when no call existed.
        terminal_model_call: Option<FailedTerminalModelCall>,
    },
    /// The turn terminalized as completed.
    Completed {
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
        /// Authoritative terminal attempt.
        terminal_attempt_id: CanonicalUuid,
        /// Outcome-authoritative call.
        terminal_model_call_id: CanonicalUuid,
    },
    /// The turn terminalized as refused.
    Refused {
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
        /// Authoritative terminal attempt.
        terminal_attempt_id: CanonicalUuid,
        /// Outcome-authoritative call.
        terminal_model_call_id: CanonicalUuid,
    },
    /// The turn terminalized after confirmed cancellation.
    Cancelled {
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
        /// Authoritative terminal attempt.
        terminal_attempt_id: CanonicalUuid,
        /// Terminal call, or null when cancellation preceded preparation.
        terminal_model_call_id: Option<CanonicalUuid>,
    },
    /// The turn terminalized on an ambiguous model call.
    ReconciliationRequired {
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
        /// Authoritative terminal attempt.
        terminal_attempt_id: CanonicalUuid,
        /// Exact ambiguous terminal model call.
        terminal_model_call_id: CanonicalUuid,
    },
    /// The turn terminalized on an ambiguous tool attempt.
    ToolReconciliationRequired {
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
        /// Authoritative terminal turn attempt.
        terminal_attempt_id: CanonicalUuid,
        /// Exact terminal tool attempt.
        terminal_tool_attempt_id: CanonicalUuid,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum RawTurnState {
    Queued {
        accepted_input_id: CanonicalUuid,
        content: UserInputContent,
    },
    QueuedDelegated {
        spawning_request_id: CanonicalUuid,
        parent_session_id: CanonicalUuid,
        parent_turn_id: CanonicalUuid,
        content: InputContent,
    },
    QueuedDelegationWake {
        first_delivery_sequence: CanonicalU64,
        through_delivery_sequence: CanonicalU64,
    },
    DelegationTerminated {
        spawning_request_id: CanonicalUuid,
        outcome: DelegationOutcome,
        reason: DelegationReason,
        provenance: DelegationProvenance,
    },
    ActiveRunning {
        current_attempt_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        current_model_call: Option<CurrentModelCall>,
    },
    ActiveAwaitingModelCallRecovery {
        ended_attempt_id: CanonicalUuid,
        recovery_model_call_id: CanonicalUuid,
        automatic_reconciliation_attempts: CanonicalU64,
        operator_action_required: bool,
    },
    ActiveAwaitingToolApproval {
        tool_request_id: CanonicalUuid,
    },
    ActiveAwaitingChild {
        await_request_id: CanonicalUuid,
        spawning_request_id: CanonicalUuid,
        child_session_id: CanonicalUuid,
    },
    ActiveAwaitingToolRecovery {
        ended_attempt_id: CanonicalUuid,
        recovery_tool_attempt_id: CanonicalUuid,
        automatic_reconciliation_attempts: CanonicalU64,
        operator_action_required: bool,
    },
    ActiveAwaitingRunnerRecovery {
        runner_id: CanonicalUuid,
        placement_revision: CanonicalU64,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        tool_attempt_id: Option<CanonicalUuid>,
    },
    Failed {
        terminal_frontier_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        terminal_attempt_id: Option<CanonicalUuid>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        terminal_model_call: Option<FailedTerminalModelCall>,
    },
    Completed {
        terminal_frontier_id: CanonicalUuid,
        terminal_attempt_id: CanonicalUuid,
        terminal_model_call_id: CanonicalUuid,
    },
    Refused {
        terminal_frontier_id: CanonicalUuid,
        terminal_attempt_id: CanonicalUuid,
        terminal_model_call_id: CanonicalUuid,
    },
    Cancelled {
        terminal_frontier_id: CanonicalUuid,
        terminal_attempt_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        terminal_model_call_id: Option<CanonicalUuid>,
    },
    ReconciliationRequired {
        terminal_frontier_id: CanonicalUuid,
        terminal_attempt_id: CanonicalUuid,
        terminal_model_call_id: CanonicalUuid,
    },
    ToolReconciliationRequired {
        terminal_frontier_id: CanonicalUuid,
        terminal_attempt_id: CanonicalUuid,
        terminal_tool_attempt_id: CanonicalUuid,
    },
}

impl<'de> Deserialize<'de> for TurnState {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let state = match RawTurnState::deserialize(deserializer)? {
            RawTurnState::Queued {
                accepted_input_id,
                content,
            } => {
                content.validate().map_err(serde::de::Error::custom)?;
                Self::Queued {
                    accepted_input_id,
                    content,
                }
            }
            RawTurnState::QueuedDelegated {
                spawning_request_id,
                parent_session_id,
                parent_turn_id,
                content,
            } => Self::QueuedDelegated {
                spawning_request_id,
                parent_session_id,
                parent_turn_id,
                content,
            },
            RawTurnState::QueuedDelegationWake {
                first_delivery_sequence,
                through_delivery_sequence,
            } => {
                if first_delivery_sequence.value() == 0
                    || first_delivery_sequence > through_delivery_sequence
                {
                    return Err(serde::de::Error::custom(
                        "delegation wake requires a positive ordered delivery range",
                    ));
                }
                Self::QueuedDelegationWake {
                    first_delivery_sequence,
                    through_delivery_sequence,
                }
            }
            RawTurnState::DelegationTerminated {
                spawning_request_id,
                outcome,
                reason,
                provenance,
            } => {
                if !delegation_terminal_outcome_reason_is_admissible(outcome, reason)
                    || !parent_delegation_provenance_has_cascade(&provenance)
                {
                    return Err(serde::de::Error::custom(
                        "delegation terminal requires parent cascade authority",
                    ));
                }
                Self::DelegationTerminated {
                    spawning_request_id,
                    outcome,
                    reason,
                    provenance,
                }
            }
            RawTurnState::ActiveRunning {
                current_attempt_id,
                current_model_call,
            } => Self::ActiveRunning {
                current_attempt_id,
                current_model_call,
            },
            RawTurnState::ActiveAwaitingModelCallRecovery {
                ended_attempt_id,
                recovery_model_call_id,
                automatic_reconciliation_attempts,
                operator_action_required,
            } => Self::ActiveAwaitingModelCallRecovery {
                ended_attempt_id,
                recovery_model_call_id,
                automatic_reconciliation_attempts,
                operator_action_required,
            },
            RawTurnState::ActiveAwaitingToolApproval { tool_request_id } => {
                Self::ActiveAwaitingToolApproval { tool_request_id }
            }
            RawTurnState::ActiveAwaitingChild {
                await_request_id,
                spawning_request_id,
                child_session_id,
            } => Self::ActiveAwaitingChild {
                await_request_id,
                spawning_request_id,
                child_session_id,
            },
            RawTurnState::ActiveAwaitingToolRecovery {
                ended_attempt_id,
                recovery_tool_attempt_id,
                automatic_reconciliation_attempts,
                operator_action_required,
            } => Self::ActiveAwaitingToolRecovery {
                ended_attempt_id,
                recovery_tool_attempt_id,
                automatic_reconciliation_attempts,
                operator_action_required,
            },
            RawTurnState::ActiveAwaitingRunnerRecovery {
                runner_id,
                placement_revision,
                tool_attempt_id,
            } => Self::ActiveAwaitingRunnerRecovery {
                runner_id,
                placement_revision: PositiveCanonicalU64::try_new(placement_revision.value())
                    .map_err(|_| {
                        serde::de::Error::custom(
                            "runner recovery requires a positive placement revision",
                        )
                    })?,
                tool_attempt_id,
            },
            RawTurnState::Failed {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call,
            } => {
                if terminal_model_call.is_some() && terminal_attempt_id.is_none() {
                    return Err(serde::de::Error::custom(
                        "failed terminal call requires a terminal attempt",
                    ));
                }
                Self::Failed {
                    terminal_frontier_id,
                    terminal_attempt_id,
                    terminal_model_call,
                }
            }
            RawTurnState::Completed {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => Self::Completed {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            },
            RawTurnState::Refused {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => Self::Refused {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            },
            RawTurnState::Cancelled {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => Self::Cancelled {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            },
            RawTurnState::ReconciliationRequired {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => Self::ReconciliationRequired {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            },
            RawTurnState::ToolReconciliationRequired {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_tool_attempt_id,
            } => Self::ToolReconciliationRequired {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_tool_attempt_id,
            },
        };
        Ok(state)
    }
}

impl TurnState {
    pub(crate) fn validate(&self) -> Result<(), FrameValidationError> {
        if let Self::QueuedDelegationWake {
            first_delivery_sequence,
            through_delivery_sequence,
        } = self
            && (first_delivery_sequence.value() == 0
                || first_delivery_sequence > through_delivery_sequence)
        {
            return Err(FrameValidationError::TurnStateShape);
        }
        if let Self::Failed {
            terminal_attempt_id: None,
            terminal_model_call: Some(_),
            ..
        } = self
        {
            return Err(FrameValidationError::TurnStateShape);
        }
        if let Self::DelegationTerminated {
            outcome,
            reason,
            provenance,
            ..
        } = self
            && (!delegation_terminal_outcome_reason_is_admissible(*outcome, *reason)
                || !parent_delegation_provenance_has_cascade(provenance))
        {
            return Err(FrameValidationError::TurnStateShape);
        }
        Ok(())
    }
}

/// Source speaker admitted by an imported transcript entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportedSpeaker {
    /// The source identified the entry as user-authored.
    User,
    /// The source identified the entry as assistant-authored.
    Assistant,
}

/// Exact source attestation for an imported entry's speaker.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImportedSourceSpeaker {
    /// The source omitted the speaker field.
    NotAttested {},
    /// The source explicitly supplied no speaker.
    AttestedAbsent {},
    /// The source supplied one admitted speaker.
    Attested {
        /// Exact source-supplied speaker.
        speaker: ImportedSpeaker,
    },
}

/// Closed discriminator naming one imported entry's normalized content
/// variant.
///
/// The transcript snapshot reaches the `Text` arm only for absent or
/// unattested text, because attested text takes the separate text-entry
/// message there. An imported-conversation inspection row has no such split
/// and uses `Text` for every `Text` content, carrying attestation in its
/// preview member instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportedContentKind {
    /// One source-defined event.
    SourceEvent,
    /// One source-defined message block.
    SourceMessageBlock,
    /// Imported text content.
    Text,
    /// One imported tool call.
    ToolCall,
    /// One imported tool result.
    ToolResult,
    /// One imported thinking block.
    Thinking,
    /// One imported redacted-thinking block.
    RedactedThinking,
    /// One imported document block.
    Document,
    /// A typed absence for message content.
    MessageContentAbsent,
}

#[derive(signalbox_derive::Accessors)]
/// A leading excerpt of one imported entry's exact attested text.
///
/// The preview is the entry's exact leading Unicode scalar sequence cut at a
/// scalar boundary, never a summary, replacement, or re-encoding. It is a
/// recognition aid for choosing a position; the immutable imported aggregate
/// remains the authority for complete content.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawImportedTextPreview")]
pub struct ImportedTextPreview {
    /// Returns the exact emitted leading scalars.
    #[get(str)]
    /// Exact leading scalars within structural wire-text memory.
    pub(crate) preview: String,
    /// Whether exact text remains beyond the emitted scalars.
    pub(crate) truncated: bool,
}

/// The undecoded wire shape of a preview, before its bound and truncation
/// marker are checked.
///
/// Deserializing through this raw shape keeps the checked type unconstructible
/// from an invalid frame, so a direct `ImportedTextPreview` deserialization
/// cannot bypass the validation an embedded one performs.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawImportedTextPreview {
    preview: String,
    truncated: bool,
}

impl TryFrom<RawImportedTextPreview> for ImportedTextPreview {
    type Error = FrameValidationError;

    fn try_from(raw: RawImportedTextPreview) -> Result<Self, Self::Error> {
        let preview = Self {
            preview: raw.preview,
            truncated: raw.truncated,
        };
        preview.validate()?;
        Ok(preview)
    }
}

impl ImportedTextPreview {
    /// Constructs a structurally bounded preview of one exact attested text.
    ///
    /// The cut lands on a Unicode scalar boundary, so the preview is always a
    /// prefix of the source text rather than a truncated encoding.
    pub fn of_exact_text(text: &str) -> Self {
        Self::of_exact_text_with_limit(text, None)
    }

    /// Constructs a preview under the deployment's optional retained-detail policy.
    pub fn of_exact_text_with_limit(text: &str, limit: Option<usize>) -> Self {
        let effective_limit = limit
            .unwrap_or(MAX_CONTENT_FRAGMENT_BYTES)
            .min(MAX_CONTENT_FRAGMENT_BYTES);
        let mut end = effective_limit.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            preview: text[..end].to_owned(),
            truncated: end < text.len(),
        }
    }

    /// Returns whether exact text remains beyond the emitted scalars.
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    pub(crate) fn validate(&self) -> Result<(), FrameValidationError> {
        if self.preview.len() > MAX_CONTENT_FRAGMENT_BYTES {
            return Err(FrameValidationError::ImportedTextPreviewShape);
        }
        // Every nonempty text yields at least one scalar inside the bound, so
        // an empty preview cannot be the cut prefix of a longer text.
        if self.truncated && self.preview.is_empty() {
            return Err(FrameValidationError::ImportedTextPreviewShape);
        }
        Ok(())
    }
}

/// Non-text semantic transcript entry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TranscriptEntry {
    /// A reference-only successor placement boundary.
    RunnerPlacementChanged {
        /// Exact positive successor placement revision.
        placement_revision: PositiveCanonicalU64,
    },
    /// Exact delegated task that opened one child session.
    DelegatedTask {
        /// Tool request that spawned the child.
        spawning_request_id: CanonicalUuid,
        /// Parent session that issued the spawn request.
        parent_session_id: CanonicalUuid,
        /// Parent turn that issued the spawn request.
        parent_turn_id: CanonicalUuid,
        /// Exact delegated task text.
        content: String,
    },
    /// Exact bidirectional delegation message delivered to this frontier.
    DelegationMessage {
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Immutable message identity.
        message_id: CanonicalUuid,
        /// Sending session.
        sender_session_id: CanonicalUuid,
        /// Receiving session.
        recipient_session_id: CanonicalUuid,
        /// Relationship-local message ordinal.
        ordinal: CanonicalU64,
        /// Recipient-wide delivery sequence.
        delivery_sequence: CanonicalU64,
        /// Exact delivered content.
        content: String,
    },
    /// Exact child result delivered through one registered wait.
    DelegationResult {
        /// Await request receiving this result.
        await_request_id: CanonicalUuid,
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Terminal child session.
        child_session_id: CanonicalUuid,
        /// Foreground or background delivery mode.
        mode: DelegationWaitMode,
        /// Recipient-wide position for background delivery only.
        delivery_sequence: Option<CanonicalU64>,
        /// Typed terminal result outcome.
        outcome: DelegationOutcome,
        /// Delivered content for a successful result only.
        content: Option<String>,
        /// Typed lifecycle reason.
        reason: DelegationReason,
        /// Exact child-turn or parent-command proof.
        provenance: DelegationProvenance,
    },
    /// Injected boundary declaring the model identity newly in force.
    ModelIdentityChanged {
        /// Turn whose start first observes the new model identity.
        turn_id: CanonicalUuid,
        /// Immutable defaults epoch bound by the turn.
        defaults_version: CanonicalU64,
        /// Exact direct model identity frozen for the turn.
        selected_model_id: CanonicalUuid,
    },
    /// Provider-side compaction occurred; opaque replay bytes stay internal.
    ProviderCompaction {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Producing model call.
        model_call_id: CanonicalUuid,
    },
    /// Provider reasoning was retained; opaque replay bytes stay internal.
    ProviderReasoning {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Producing model call.
        model_call_id: CanonicalUuid,
    },
    /// Assistant proposed one durable tool request.
    AssistantToolUse {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Producing model call.
        model_call_id: CanonicalUuid,
        /// Exact logical tool request.
        tool_request_id: CanonicalUuid,
        /// Exact checked tool name.
        tool_name: String,
        /// Exact normalized or scrubbed-undecodable arguments.
        arguments: String,
        /// Explicit decision provenance, absent while pending and for automatic policy.
        #[serde(
            default,
            deserialize_with = "deserialize_optional_non_null",
            skip_serializing_if = "Option::is_none"
        )]
        approval: Option<TranscriptToolApproval>,
    },
    /// One physical tool attempt produced the logical result.
    ToolExecutionResult {
        /// Exact logical tool request.
        tool_request_id: CanonicalUuid,
        /// Exact physical tool attempt.
        tool_attempt_id: CanonicalUuid,
        /// Exact provider-visible result content.
        content: String,
    },
    /// One logical tool request was denied.
    ToolDenied {
        /// Exact denied tool request.
        tool_request_id: CanonicalUuid,
        /// Exact provider-visible denial content.
        content: String,
    },
    /// One logical tool request resolved before dispatch.
    ToolInadmissible {
        /// Exact inadmissible tool request.
        tool_request_id: CanonicalUuid,
        /// Exact provider-visible inadmissibility content.
        content: String,
    },
    /// The request closed when its turn ended.
    ToolClosed {
        /// Exact closed tool request.
        tool_request_id: CanonicalUuid,
        /// Exact provider-visible terminal-closure content.
        content: String,
    },
    /// Explicit completed-turn marker.
    TurnCompleted {
        /// Completed turn.
        turn_id: CanonicalUuid,
    },
    /// Explicit failed-turn marker.
    TurnFailed {
        /// Failed turn.
        turn_id: CanonicalUuid,
    },
    /// Explicit cancelled-turn marker.
    TurnCancelled {
        /// Cancelled turn.
        turn_id: CanonicalUuid,
    },
    /// Conservative imported entry without rendered text.
    Imported {
        /// Owning imported conversation.
        imported_conversation_id: CanonicalUuid,
        /// Exact imported entry identity.
        imported_entry_id: CanonicalUuid,
        /// Exact source-speaker attestation.
        source_speaker: ImportedSourceSpeaker,
        /// Conservative normalized content kind.
        content_kind: ImportedContentKind,
    },
}

/// Metadata for a text-bearing semantic transcript entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TranscriptTextEntry {
    /// Committed assistant text.
    Assistant {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Producing model call.
        model_call_id: CanonicalUuid,
    },
    /// Model-produced summary of one exact earlier semantic range.
    ContextSummary {
        /// Dedicated model call that produced the summary.
        model_call_id: CanonicalUuid,
        /// Source session of the inclusive range's first entry.
        first_source_session_id: CanonicalUuid,
        /// Identity of the inclusive range's first entry.
        first_entry_id: CanonicalUuid,
        /// Source session of the inclusive range's final entry.
        through_source_session_id: CanonicalUuid,
        /// Identity of the inclusive range's final entry.
        through_entry_id: CanonicalUuid,
    },
    /// Imported text whose exact value was source-attested.
    Imported {
        /// Owning imported conversation.
        imported_conversation_id: CanonicalUuid,
        /// Exact imported entry identity.
        imported_entry_id: CanonicalUuid,
        /// Exact source-speaker attestation.
        source_speaker: ImportedSourceSpeaker,
    },
}

/// Durable model-call terminal disposition.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCallDisposition {
    /// Provider call completed.
    Completed,
    /// Call failed with definitive evidence.
    KnownFailed,
    /// Provider refused.
    Refused,
    /// Call was cancelled.
    Cancelled,
    /// External outcome is ambiguous.
    Ambiguous,
}

/// Durable model-call state carried by a session event.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelCallState {
    /// Call is prepared but unsent.
    Prepared {},
    /// Call crossed the send boundary.
    InFlight {},
    /// Cancellation was durably requested for the issued call.
    CancellationRequested {},
    /// Call reached a terminal disposition.
    Terminal {
        /// Exact terminal disposition.
        disposition: ModelCallDisposition,
    },
}

/// Exact durable state of one tool batch presentation boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolBatchState {
    /// Assistant tool proposals committed.
    Proposed {
        /// Exact frontier containing the assistant tool-use entries.
        frontier_id: CanonicalUuid,
    },
    /// Proposal-ordered logical results committed.
    ResultsProjected {
        /// Exact frontier containing the result suffix.
        frontier_id: CanonicalUuid,
    },
    /// One ambiguous physical attempt requires user recovery.
    RecoveryRequired {
        /// Exact ambiguous tool attempt.
        tool_attempt_id: CanonicalUuid,
    },
}

pub(crate) fn validate_delegation_transcript_entry(
    source_session_id: CanonicalUuid,
    entry: &TranscriptEntry,
) -> Result<(), FrameValidationError> {
    let valid = match entry {
        TranscriptEntry::DelegatedTask {
            parent_session_id,
            content,
            ..
        } => *parent_session_id != source_session_id && delegation_content_is_valid(content),
        TranscriptEntry::DelegationMessage {
            sender_session_id,
            recipient_session_id,
            ordinal,
            delivery_sequence,
            content,
            ..
        } => {
            *recipient_session_id == source_session_id
                && *sender_session_id != *recipient_session_id
                && ordinal.value() > 0
                && delivery_sequence.value() > 0
                && delegation_content_is_valid(content)
        }
        TranscriptEntry::DelegationResult {
            child_session_id,
            mode,
            delivery_sequence,
            outcome,
            content,
            reason,
            provenance,
            ..
        } => {
            *child_session_id != source_session_id
                && match mode {
                    DelegationWaitMode::Foreground => delivery_sequence.is_none(),
                    DelegationWaitMode::Background => {
                        delivery_sequence.is_some_and(|sequence| sequence.value() > 0)
                    }
                }
                && child_result_shape_is_valid(
                    source_session_id,
                    *child_session_id,
                    *outcome,
                    content,
                    *reason,
                    provenance,
                )
        }
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::DelegationShape)
    }
}
