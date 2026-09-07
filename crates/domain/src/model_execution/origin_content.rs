//! Model-call origin content for `docs/spec/model-call-execution.md`.

use crate::{
    AcceptedInputId, ReconstitutedSubmitInput, SubmitInputResult,
    SubmitInputTurnOriginReconstitutionInput, UserContent,
};

/// Exact user content for one accepted-input origin referenced by a call
/// frontier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCallOriginContent {
    pub(super) accepted_input: AcceptedInputId,
    pub(super) content: UserContent,
}

impl ModelCallOriginContent {
    /// Binds exact content carried by a checked commissioned-goal turn.
    pub const fn from_goal_turn(accepted_input: AcceptedInputId, content: UserContent) -> Self {
        Self {
            accepted_input,
            content,
        }
    }

    /// Binds content for one checked pending-steering tail member.
    pub fn from_pending_steering(
        pending: &crate::PendingSteeringInput,
        content: UserContent,
    ) -> Self {
        Self {
            accepted_input: pending.accepted_input(),
            content,
        }
    }

    /// Binds content for one checked consumed-steering frontier member.
    pub fn from_consumed_steering(
        consumed: &crate::ConsumedSteeringInput,
        content: UserContent,
    ) -> Self {
        Self {
            accepted_input: consumed.accepted_input(),
            content,
        }
    }

    #[cfg(test)]
    pub(crate) const fn from_validated_parts(
        accepted_input: AcceptedInputId,
        content: UserContent,
    ) -> Self {
        Self {
            accepted_input,
            content,
        }
    }

    /// Derives exact user content from one checked applied input receipt.
    pub fn from_recorded_submit(recorded: &ReconstitutedSubmitInput) -> Option<Self> {
        let SubmitInputResult::Applied(applied) = recorded.result() else {
            return None;
        };
        (applied.session() == recorded.command().session()).then(|| Self {
            accepted_input: applied.accepted_input(),
            content: recorded.command().content().clone(),
        })
    }

    /// Derives exact origin content from a fully validated direct or
    /// reclassified accepted-input turn-origin chain.
    pub fn from_reconstituted_turn_origin(
        origin: &SubmitInputTurnOriginReconstitutionInput,
    ) -> Option<Self> {
        let (accepted_input, content) = origin.validated_origin_content()?;
        Some(Self {
            accepted_input,
            content,
        })
    }

    /// Returns the accepted input whose origin carries this content.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Borrows the exact user-authored scalar value.
    pub const fn content(&self) -> &UserContent {
        &self.content
    }
}
