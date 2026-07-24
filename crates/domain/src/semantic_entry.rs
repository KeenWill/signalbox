//! Closed semantic transcript-entry values.
//!
//! docs/spec/sessions-and-transcript.md is normative. Entry construction
//! remains sealed behind aggregate transitions and checked reconstitution
//! boundaries that validate the referenced facts.

use crate::{
    AcceptedInputId, ModelCallId, NonEmptyUnicodeText, NonEmptyUnicodeTextError,
    SemanticTranscriptEntryId, SemanticTranscriptEntryRef, SessionId, ToolRequestId, TurnId,
};

/// Exact assistant-owned text from one definitive provider response.
///
/// This wrapper deliberately remains distinct from [`crate::UserContent`]
/// even though both values share the exact scalar rules in
/// docs/spec/sessions-and-transcript.md.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AssistantText(NonEmptyUnicodeText);

impl AssistantText {
    /// Checks exact decoded assistant text without trimming or normalization.
    pub fn try_new(value: String) -> Result<Self, NonEmptyUnicodeTextError> {
        Ok(Self(NonEmptyUnicodeText::try_new(value)?))
    }

    /// Borrows the exact checked assistant text.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Returns the exact checked assistant text.
    pub fn into_string(self) -> String {
        self.0.into_string()
    }
}

/// The complete semantic transcript-entry payload set.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum SemanticTranscriptEntryPayload {
    /// The exact accepted input whose origin turn became eligible.
    OriginAcceptedInput {
        /// The immutable accepted-input identity.
        accepted_input: AcceptedInputId,
    },
    /// Accepted next-safe-point input consumed by its exact source turn.
    SteeringAcceptedInput {
        /// The immutable accepted-input identity.
        accepted_input: AcceptedInputId,
        /// The exact active turn the input was accepted to steer.
        source_turn: TurnId,
    },
    /// An explicit marker for an exact failed turn.
    TurnFailed {
        /// The turn that terminalized as failed.
        turn: TurnId,
    },
    /// Exact assistant text with producing-call provenance.
    AssistantText {
        /// The outcome-authoritative call that supplied this text.
        producing_call: ModelCallId,
        /// The exact assistant-owned text.
        value: AssistantText,
    },
    /// One logical tool request named by a definitive assistant response.
    AssistantToolUse {
        /// The outcome-authoritative call that supplied this request.
        producing_call: ModelCallId,
        /// The logical request derived from that response.
        request: ToolRequestId,
    },
    /// Executed success or error evidence owned by one physical attempt.
    ToolExecutionResult {
        /// The exact terminal physical attempt.
        attempt: crate::ToolAttemptId,
    },
    /// A durable denial owned by one logical request's approval decision.
    ToolDenied {
        /// The exact denied logical request.
        request: ToolRequestId,
    },
    /// An undecided request closed because its turn terminalized.
    ToolClosed {
        /// The exact closed logical request.
        request: ToolRequestId,
    },
    /// The explicit final marker for a completed turn.
    TurnCompleted {
        /// The turn that terminalized as completed.
        turn: TurnId,
    },
    /// The explicit final marker for an interrupt-cancelled turn.
    TurnCancelled {
        /// The turn that terminalized as cancelled.
        turn: TurnId,
    },
}

/// Compatibility spelling for code limited to the initial entry variants.
pub(crate) type InitialSemanticTranscriptEntryPayload = SemanticTranscriptEntryPayload;

/// One immutable identified semantic transcript entry.
///
/// Raw identifiers and a payload cannot construct an entry. Live eligibility
/// and checked scheduling reconstitution are the only producers:
///
/// ```compile_fail
/// use signalbox_domain::{
///     SemanticTranscriptEntry, SemanticTranscriptEntryPayload,
///     SemanticTranscriptEntryId, SessionId,
/// };
///
/// fn raw_parts_are_not_a_semantic_entry(
///     identity: SemanticTranscriptEntryId,
///     source_session: SessionId,
///     payload: SemanticTranscriptEntryPayload,
/// ) {
///     let _ = SemanticTranscriptEntry {
///         identity,
///         source_session,
///         payload,
///     };
/// }
/// ```
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SemanticTranscriptEntry {
    identity: SemanticTranscriptEntryId,
    source_session: SessionId,
    payload: SemanticTranscriptEntryPayload,
}

impl SemanticTranscriptEntry {
    #[allow(
        dead_code,
        reason = "checked scheduling reconstitution and eligibility consume this sealed producer"
    )]
    pub(crate) fn from_validated_parts(
        identity: SemanticTranscriptEntryId,
        source_session: SessionId,
        payload: SemanticTranscriptEntryPayload,
    ) -> Self {
        Self {
            identity,
            source_session,
            payload,
        }
    }

    /// Returns this immutable entry's distinct identity.
    pub const fn identity(&self) -> SemanticTranscriptEntryId {
        self.identity
    }

    /// Returns the session that created this semantic entry.
    pub const fn source_session(&self) -> SessionId {
        self.source_session
    }

    /// Returns the exact closed semantic payload.
    pub const fn payload(&self) -> &SemanticTranscriptEntryPayload {
        &self.payload
    }

    /// Returns this entry's source-qualified frontier reference.
    pub const fn reference(&self) -> SemanticTranscriptEntryRef {
        SemanticTranscriptEntryRef::from_source(self.source_session, self.identity)
    }
}

/// Checked domain values supplied for one stored semantic entry.
///
/// This is an input to the complete scheduling reconstitution seam, not a
/// proof factory. It cannot independently construct a
/// [`SemanticTranscriptEntry`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticTranscriptEntryReconstitutionInput {
    identity: SemanticTranscriptEntryId,
    source_session: SessionId,
    payload: SemanticTranscriptEntryPayload,
}

impl SemanticTranscriptEntryReconstitutionInput {
    /// Supplies the complete typed stored facts for one initial entry.
    pub fn new(
        identity: SemanticTranscriptEntryId,
        source_session: SessionId,
        payload: SemanticTranscriptEntryPayload,
    ) -> Self {
        Self {
            identity,
            source_session,
            payload,
        }
    }

    /// Returns the stored semantic-entry identity.
    pub const fn identity(&self) -> SemanticTranscriptEntryId {
        self.identity
    }

    /// Returns the stored source-session identity.
    pub const fn source_session(&self) -> SessionId {
        self.source_session
    }

    /// Returns the stored closed semantic payload.
    pub const fn payload(&self) -> &SemanticTranscriptEntryPayload {
        &self.payload
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        accepted_input_id, model_call_id, semantic_transcript_entry_id, session_id,
        tool_request_id, turn_id,
    };

    /// One semantic entry with canonical identity and source-session plumbing;
    /// only its payload varies at the call site.
    fn semantic_entry(payload: InitialSemanticTranscriptEntryPayload) -> SemanticTranscriptEntry {
        SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(1),
            session_id(1),
            payload,
        )
    }

    /// INV-001 / INV-005 / INV-036: the semantic projection remains a closed
    /// typed reference to its distinct accepted-input, source-turn, terminal
    /// turn, or tool subject.
    #[test]
    fn initial_payload_variants_preserve_exact_typed_subjects() {
        let accepted_input = accepted_input_id(2);
        let turn = turn_id(3);
        let producing_call = model_call_id(4);
        let request = tool_request_id(5);
        let origin = semantic_entry(InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
            accepted_input,
        });
        let failed = semantic_entry(InitialSemanticTranscriptEntryPayload::TurnFailed { turn });
        let steering = semantic_entry(
            InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input,
                source_turn: turn,
            },
        );
        let tool_use = semantic_entry(InitialSemanticTranscriptEntryPayload::AssistantToolUse {
            producing_call,
            request,
        });

        assert!(matches!(
            origin.payload(),
            InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: actual,
            } if *actual == accepted_input
        ));
        assert!(matches!(
            failed.payload(),
            InitialSemanticTranscriptEntryPayload::TurnFailed { turn: actual } if *actual == turn
        ));
        assert!(matches!(
            steering.payload(),
            InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input: actual_input,
                source_turn,
            } if *actual_input == accepted_input && *source_turn == turn
        ));
        assert!(matches!(
            tool_use.payload(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call: actual_call,
                request: actual_request,
            } if *actual_call == producing_call && *actual_request == request
        ));
    }

    /// INV-005: assistant text stays exact, remains distinct from user
    /// content, and retains producing-call provenance.
    #[test]
    fn adr0042_inv005_assistant_text_is_exact_and_call_correlated() {
        let producing_call = crate::test_support::model_call_id(7);
        let exact = String::from(" \tline one\r\ncafe\u{301}\n ");
        let entry = semantic_entry(SemanticTranscriptEntryPayload::AssistantText {
            producing_call,
            value: AssistantText::try_new(exact.clone()).expect("nonempty text is valid"),
        });

        assert!(matches!(
            entry.payload(),
            SemanticTranscriptEntryPayload::AssistantText {
                producing_call: actual_call,
                value,
            } if *actual_call == producing_call && value.as_str() == exact
        ));
        assert_ne!(
            entry.payload(),
            &SemanticTranscriptEntryPayload::AssistantText {
                producing_call,
                value: AssistantText::try_new(String::from(" \tline one\ncafé\n "))
                    .expect("normalization-distinct text is valid"),
            }
        );
    }

    /// INV-006: completion is an explicit turn marker distinct from every
    /// physical model-call outcome.
    #[test]
    fn adr0042_inv006_completion_marker_names_the_exact_turn() {
        let turn = turn_id(9);
        let entry = semantic_entry(SemanticTranscriptEntryPayload::TurnCompleted { turn });

        assert!(matches!(
            entry.payload(),
            SemanticTranscriptEntryPayload::TurnCompleted { turn: actual } if *actual == turn
        ));
    }
}
