//! Initial closed semantic transcript-entry values.
//!
//! ADR-0036 is normative. The only constructible payloads in this slice are
//! an accepted input becoming semantic history at eligibility and an explicit
//! marker for a turn that terminalized as failed. Entry construction remains
//! sealed behind the scheduling reconstitution and eligibility boundaries
//! that validate the referenced aggregate facts.

use crate::{
    AcceptedInputId, SemanticTranscriptEntryId, SemanticTranscriptEntryRef, SessionId, TurnId,
};

/// The complete initial semantic transcript-entry payload set.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InitialSemanticTranscriptEntryPayload {
    /// The exact accepted input whose origin turn became eligible.
    OriginAcceptedInput {
        /// The immutable accepted-input identity.
        accepted_input: AcceptedInputId,
    },
    /// An explicit marker for an exact failed turn.
    TurnFailed {
        /// The turn that terminalized as failed.
        turn: TurnId,
    },
}

/// One immutable identified semantic transcript entry.
///
/// Raw identifiers and a payload cannot construct an entry. Live eligibility
/// and checked scheduling reconstitution are the only producers:
///
/// ```compile_fail
/// use signalbox_domain::{
///     InitialSemanticTranscriptEntryPayload, SemanticTranscriptEntry,
///     SemanticTranscriptEntryId, SessionId,
/// };
///
/// fn raw_parts_are_not_a_semantic_entry(
///     identity: SemanticTranscriptEntryId,
///     source_session: SessionId,
///     payload: InitialSemanticTranscriptEntryPayload,
/// ) {
///     let _ = SemanticTranscriptEntry {
///         identity,
///         source_session,
///         payload,
///     };
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SemanticTranscriptEntry {
    identity: SemanticTranscriptEntryId,
    source_session: SessionId,
    payload: InitialSemanticTranscriptEntryPayload,
}

impl SemanticTranscriptEntry {
    #[allow(
        dead_code,
        reason = "checked scheduling reconstitution and eligibility consume this sealed producer"
    )]
    pub(crate) const fn from_validated_parts(
        identity: SemanticTranscriptEntryId,
        source_session: SessionId,
        payload: InitialSemanticTranscriptEntryPayload,
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
    pub const fn payload(&self) -> InitialSemanticTranscriptEntryPayload {
        self.payload
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticTranscriptEntryReconstitutionInput {
    identity: SemanticTranscriptEntryId,
    source_session: SessionId,
    payload: InitialSemanticTranscriptEntryPayload,
}

impl SemanticTranscriptEntryReconstitutionInput {
    /// Supplies the complete typed stored facts for one initial entry.
    pub const fn new(
        identity: SemanticTranscriptEntryId,
        source_session: SessionId,
        payload: InitialSemanticTranscriptEntryPayload,
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
    pub const fn payload(&self) -> InitialSemanticTranscriptEntryPayload {
        self.payload
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        accepted_input_id, semantic_transcript_entry_id, session_id, turn_id,
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

    /// INV-001 / INV-005: the initial semantic projection remains a closed
    /// typed reference to its distinct accepted-input or turn subject.
    #[test]
    fn initial_payload_variants_preserve_exact_typed_subjects() {
        let accepted_input = accepted_input_id(2);
        let turn = turn_id(3);
        let origin = semantic_entry(InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
            accepted_input,
        });
        let failed = semantic_entry(InitialSemanticTranscriptEntryPayload::TurnFailed { turn });

        assert_ne!(origin.payload(), failed.payload());
        assert!(matches!(
            origin.payload(),
            InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: actual,
            } if actual == accepted_input
        ));
        assert!(matches!(
            failed.payload(),
            InitialSemanticTranscriptEntryPayload::TurnFailed { turn: actual } if actual == turn
        ));
    }
}
