//! Core domain boundary for Signalbox.
//!
//! Domain identities are distinct from storage, protocol, and framework types.
//! Lifecycle behavior is introduced only in slices authorized by accepted
//! decisions; aggregate orchestration and product behavior remain deferred.
//! [`AcceptedInputLifecycle`] is the canonical public boundary for validated
//! disposition transitions that preserve an accepted input's identity.

mod accepted_input;
mod actor;
mod applied_interrupt;
mod configuration;
mod context_frontier;
mod delivery_request;
mod fatal_mismatch;
mod model_call;
mod provider_evidence;
mod queue_order;
mod replace_session_defaults;
mod session;
mod submit_input;
mod turn_attempt;
mod turn_lifecycle;
mod user_content;

pub use accepted_input::{
    AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputLifecycleTransitionError,
    SteeringBinding, SteeringReclassificationReason,
};
pub use actor::Actor;
pub use applied_interrupt::{AppliedInterruptCommandResult, AppliedInterruptProof};
pub use configuration::{
    ConfigurationRequest, DirectModelSelection, EffectiveConfiguration, FrozenAliasDefinition,
    FrozenModelSelection, KnownProviderFailureRetry, ModelAlias, ModelFallback, ModelParameters,
    ModelSelectionOverride, ModelSelectionRequest, OriginConfiguration,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion,
    SessionDefaultsVersionMismatch, TurnConfigurationProvenance, UnknownModelAlias,
    VersionCheckedConfigurationRequest, VersionedSessionConfigurationDefaults,
};
pub use context_frontier::{
    ContextFrontier, ContextFrontierId, ResolvedContextFrontierSnapshot, SemanticTranscriptEntryId,
    SemanticTranscriptEntryRef,
};
pub use delivery_request::{DeliveryRequest, PerInputConfigurationChoices};
pub use model_call::{
    CurrentModelCall, CurrentModelCallState, EndedModelCall, ModelCallDisposition,
    PinnedProviderTarget, ProviderModelIdentity, ResolvedProviderTarget,
};
pub use provider_evidence::{
    ProviderTargetEvidence, ProviderTargetEvidenceLog, ProviderTargetMismatchInvalidation,
    ProviderTargetMismatchInvalidationLog, ProviderTargetObservation,
};
pub use queue_order::{
    AcceptedInputQueueOrder, AcceptedInputQueueOrderError, AcceptedInputQueuePriority,
    AcceptedInputQueueWork, SessionInputPosition, derive_accepted_input_total_order,
};
pub use replace_session_defaults::{
    PreparedReplaceSessionDefaults, ReconstitutedReplaceSessionDefaults, ReplaceSessionDefaults,
    ReplaceSessionDefaultsAppliedResult, ReplaceSessionDefaultsCurrentVersionMismatch,
    ReplaceSessionDefaultsPreparationError, ReplaceSessionDefaultsReconstitutionError,
    ReplaceSessionDefaultsReconstitutionFailure, ReplaceSessionDefaultsReconstitutionInput,
    ReplaceSessionDefaultsRejectedResult, ReplaceSessionDefaultsResult,
    ReplaceSessionDefaultsSessionNotFound, ReplaceSessionDefaultsVersionExhausted,
};
pub use session::{
    CreateSession, CreateSessionAppliedResult, CreateSessionPreparationError,
    CreateSessionPreparationFailure, CreateSessionReconstitutionError,
    CreateSessionReconstitutionFailure, CreateSessionReconstitutionInput, InitialSession,
    PreparedCreateSession, ReconstitutedSessionCreation, Session, SessionCreationCause,
    SessionCreationProvenance, SessionReconstitutionError, SessionReconstitutionFailure,
    SessionReconstitutionInput, TranscriptAncestry, TranscriptFrontier,
};
pub use submit_input::{
    PreparedSubmitInput, ReconstitutedSubmitInput, SubmitInput, SubmitInputAppliedResult,
    SubmitInputPreparationError, SubmitInputPreparationFailure, SubmitInputReconstitutionError,
    SubmitInputReconstitutionFailure, SubmitInputReconstitutionInput, SubmitInputRejectedResult,
    SubmitInputResult,
};
pub use turn_attempt::{
    AppliedInterruptState, AttemptEnd, CancellationStopDisposition, CurrentTurnAttempt,
    CurrentTurnAttemptState, EndedTurnAttempt, FatalMismatchStopCauses,
    FatalMismatchStopDisposition, ProviderTargetMismatchFailureKind,
    ProviderTargetMismatchFailureRef, TurnAttemptStopCauseUnionError, TurnAttemptStopCauses,
    UnstoppedAttemptDisposition,
};
pub use turn_lifecycle::{
    AcceptedInputStartingLineage, AcceptedInputTurnStart, ActiveTurnPhase,
    AppliedStopForReconciliationProof, IssuedOperationRef, NonEmptyIssuedOperationRefs,
    NonEmptyIssuedOperationRefsError, ReconciliationMarker, ReconciliationReason, TurnDisposition,
};
pub use user_content::{
    NonEmptyUnicodeText, NonEmptyUnicodeTextError, NonEmptyUnicodeTextFailure, UserContent,
};

macro_rules! define_identity {
    ($(#[$documentation:meta])* $name:ident) => {
        $(#[$documentation])*
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(uuid::Uuid);

        impl $name {
            /// Creates this domain identity from its UUID value.
            pub const fn from_uuid(value: uuid::Uuid) -> Self {
                Self(value)
            }

            /// Borrows the UUID value.
            pub const fn as_uuid(&self) -> &uuid::Uuid {
                &self.0
            }

            /// Returns the UUID value.
            pub const fn into_uuid(self) -> uuid::Uuid {
                self.0
            }
        }
    };
}

pub(crate) use define_identity;

define_identity!(
    /// Identifies one owner-global, durably handled command submission.
    ///
    /// This identity does not prove that the command was applied.
    DurableCommandId
);

define_identity!(
    /// Identifies one durable, independently browsable conversation.
    SessionId
);

define_identity!(
    /// Identifies one user submission durably accepted with a delivery treatment.
    AcceptedInputId
);

define_identity!(
    /// Identifies one logical request for a conversational outcome.
    TurnId
);

define_identity!(
    /// Identifies one physical orchestration tenure for a turn.
    TurnAttemptId
);

define_identity!(
    /// Identifies one hub authorization to attempt a provider interaction.
    ModelCallId
);

define_identity!(
    /// Identifies one trusted observation of a provider's reported model.
    ProviderTargetEvidenceId
);

define_identity!(
    /// Identifies one logical request for a normalized tool operation.
    ToolRequestId
);

define_identity!(
    /// Identifies one physical effort to execute a tool request.
    ToolAttemptId
);

#[cfg(test)]
pub(crate) mod test_support {
    //! Behavior-irrelevant test plumbing shared across unit-test modules.
    //!
    //! Every unit test needs deterministic identity values, and each identity
    //! is a distinct UUID-backed newtype built by [`define_identity`]. These
    //! constructors keep the `from_uuid(Uuid::from_u128(..))` pattern in one
    //! place instead of repeating it in each module's test helpers. [`table`]
    //! renders snapshot tables for the expect tests described in
    //! `docs/testing-style.md`.

    macro_rules! identity_constructors {
        ($($constructor:ident -> $identity:ty),+ $(,)?) => {
            $(
                pub(crate) fn $constructor(value: u128) -> $identity {
                    <$identity>::from_uuid(uuid::Uuid::from_u128(value))
                }
            )+
        };
    }

    identity_constructors! {
        command_id -> crate::DurableCommandId,
        turn_id -> crate::TurnId,
        turn_attempt_id -> crate::TurnAttemptId,
        session_id -> crate::SessionId,
        accepted_input_id -> crate::AcceptedInputId,
        model_call_id -> crate::ModelCallId,
        provider_target_evidence_id -> crate::ProviderTargetEvidenceId,
        provider_model_identity -> crate::ProviderModelIdentity,
        context_frontier_id -> crate::ContextFrontierId,
        semantic_transcript_entry_id -> crate::SemanticTranscriptEntryId,
        tool_request_id -> crate::ToolRequestId,
        tool_attempt_id -> crate::ToolAttemptId,
        direct -> crate::DirectModelSelection,
        alias -> crate::ModelAlias,
    }

    /// Renders one pipe-separated, left-aligned table for expect-test
    /// snapshots (`docs/testing-style.md`, rule 12).
    ///
    /// Each column pads to the widest of its header and cells, the header is
    /// underlined with dashes, and every line is right-trimmed — trailing
    /// padding would be invisible in a snapshot literal and break blessing —
    /// with one trailing newline ending the table.
    ///
    /// # Panics
    ///
    /// Panics when a row's arity differs from the header's.
    pub(crate) fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
        let mut widths: Vec<usize> = headers
            .iter()
            .map(|header| header.chars().count())
            .collect();
        for row in rows {
            assert_eq!(
                row.len(),
                headers.len(),
                "every table row must have one cell per header"
            );
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.chars().count());
            }
        }

        let underlines: Vec<String> = widths.iter().map(|width| "-".repeat(*width)).collect();
        let mut lines = Vec::with_capacity(rows.len() + 2);
        lines.push(rendered_row(headers.iter().copied(), &widths));
        lines.push(rendered_row(underlines.iter().map(String::as_str), &widths));
        for row in rows {
            lines.push(rendered_row(row.iter().map(String::as_str), &widths));
        }

        let mut rendered = lines.join("\n");
        rendered.push('\n');
        rendered
    }

    fn rendered_row<'cells>(cells: impl Iterator<Item = &'cells str>, widths: &[usize]) -> String {
        let padded: Vec<String> = cells
            .zip(widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        padded.join(" | ").trim_end().to_string()
    }

    mod tests {
        use super::table;

        #[test]
        fn table_pads_to_the_widest_of_header_and_cells_and_right_trims() {
            let rendered = table(
                &["turn", "outcome"],
                &[
                    vec!["t1".to_string(), "applied".to_string()],
                    vec!["t2-long".to_string(), String::new()],
                ],
            );

            assert_eq!(
                rendered,
                "turn    | outcome\n\
                 ------- | -------\n\
                 t1      | applied\n\
                 t2-long |\n"
            );
        }

        #[test]
        fn table_with_no_rows_is_a_header_and_its_underline() {
            assert_eq!(table(&["only"], &[]), "only\n----\n");
        }

        #[test]
        #[should_panic(expected = "one cell per header")]
        fn table_rejects_a_row_with_missing_cells() {
            table(&["first", "second"], &[vec!["lonely".to_string()]]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AcceptedInputId, ContextFrontierId, DurableCommandId, ModelCallId,
        ProviderTargetEvidenceId, SemanticTranscriptEntryId, SessionId, ToolAttemptId,
        ToolRequestId, TurnAttemptId, TurnId,
    };
    use uuid::Uuid;

    macro_rules! assert_uuid_contract {
        ($identity:ty) => {{
            let first_uuid = Uuid::from_u128(1);
            let second_uuid = Uuid::from_u128(2);
            let first_id = <$identity>::from_uuid(first_uuid);
            let equal_id = <$identity>::from_uuid(first_uuid);
            let different_id = <$identity>::from_uuid(second_uuid);

            assert_eq!(first_id, equal_id);
            assert_ne!(first_id, different_id);
            assert_eq!(first_id.as_uuid(), &first_uuid);
            assert_eq!(first_id.into_uuid(), first_uuid);
        }};
    }

    #[test]
    fn identity_uuid_representation_contract() {
        assert_uuid_contract!(DurableCommandId);
        assert_uuid_contract!(SessionId);
        assert_uuid_contract!(AcceptedInputId);
        assert_uuid_contract!(TurnId);
        assert_uuid_contract!(TurnAttemptId);
        assert_uuid_contract!(ModelCallId);
        assert_uuid_contract!(ProviderTargetEvidenceId);
        assert_uuid_contract!(ContextFrontierId);
        assert_uuid_contract!(SemanticTranscriptEntryId);
        assert_uuid_contract!(ToolRequestId);
        assert_uuid_contract!(ToolAttemptId);
    }
}
