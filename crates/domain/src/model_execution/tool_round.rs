//! Model-call tool round for `docs/spec/model-call-execution.md`.

use super::{
    CancelledToolRoundModelCallTurn, ModelCallClosureError, ModelCallTurnScope,
    ReclassifiedPendingSteeringTurn, StoppedToolResponsePartIdentity,
    StoppedToolRoundModelCallIdentities, ToolResponsePartIdentity, ToolRoundModelCallIdentities,
    ToolRoundModelCallTurn,
};
use crate::{
    ActiveTurnPhase, AppliedInterruptProof, AssistantResponsePart, CurrentTurnAttempt,
    DangerousToolAutoApproval, EndedModelCall, EndedTurnAttempt, InitialToolApproval,
    ResolvedContextFrontierSnapshot, SemanticTranscriptEntry, SemanticTranscriptEntryPayload,
    ToolRequest, ToolRequestOrdinal, ToolUsingAssistantResponse, TurnDisposition,
};
use std::collections::BTreeSet;

#[allow(clippy::too_many_arguments)]
pub(super) fn assemble_tool_round(
    scope: ModelCallTurnScope,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    frontier_entries: Vec<SemanticTranscriptEntry>,
    response: ToolUsingAssistantResponse,
    identities: ToolRoundModelCallIdentities,
    dangerous_tool_auto_approval: DangerousToolAutoApproval,
) -> Result<ToolRoundModelCallTurn, ModelCallClosureError> {
    let ModelCallTurnScope { session, turn } = scope;
    if response.parts().len() != identities.response_parts.len() {
        return Err(ModelCallClosureError::ToolResponseIdentityMismatch);
    }
    let mut used_entries = frontier_entries
        .iter()
        .map(SemanticTranscriptEntry::identity)
        .collect::<BTreeSet<_>>();
    let mut used_requests = frontier_entries
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::AssistantToolUse { request, .. } => Some(*request),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut assistant_entries = Vec::with_capacity(response.parts().len());
    let mut requests = Vec::with_capacity(response.tool_count());
    let mut automatic_approvals = Vec::with_capacity(response.tool_count());
    let mut earliest_undecided = None;
    let mut tool_ordinal = 0usize;

    for (part, identity) in response.parts().iter().zip(identities.response_parts) {
        let entry = match (part, identity) {
            (AssistantResponsePart::Text(value), ToolResponsePartIdentity::Text { entry }) => {
                if !used_entries.insert(entry) {
                    return Err(ModelCallClosureError::FrontierDerivationFailed);
                }
                SemanticTranscriptEntry::from_validated_parts(
                    entry,
                    session,
                    SemanticTranscriptEntryPayload::AssistantText {
                        producing_call: call.id(),
                        value: value.clone(),
                    },
                )
            }
            (
                AssistantResponsePart::ProviderCompaction(block),
                ToolResponsePartIdentity::ProviderCompaction { entry },
            ) => {
                if !used_entries.insert(entry) {
                    return Err(ModelCallClosureError::FrontierDerivationFailed);
                }
                SemanticTranscriptEntry::from_validated_parts(
                    entry,
                    session,
                    SemanticTranscriptEntryPayload::ProviderCompaction {
                        producing_call: call.id(),
                        block: block.clone(),
                    },
                )
            }
            (
                AssistantResponsePart::ProviderReasoning(item),
                ToolResponsePartIdentity::ProviderReasoning { entry },
            ) => {
                if !used_entries.insert(entry) {
                    return Err(ModelCallClosureError::FrontierDerivationFailed);
                }
                SemanticTranscriptEntry::from_validated_parts(
                    entry,
                    session,
                    SemanticTranscriptEntryPayload::ProviderReasoning {
                        producing_call: call.id(),
                        item: item.clone(),
                    },
                )
            }
            (
                AssistantResponsePart::ToolCall(proposal),
                ToolResponsePartIdentity::ToolCall {
                    entry,
                    request,
                    approval,
                },
            ) => {
                if !used_entries.insert(entry) || !used_requests.insert(request) {
                    return Err(ModelCallClosureError::FrontierDerivationFailed);
                }
                if proposal.is_suppressed() != (approval == InitialToolApproval::RuntimeSafetyDeny)
                    || proposal.inadmissible_reason().is_some()
                        != (approval == InitialToolApproval::Inadmissible)
                    || !initial_tool_approval_matches_posture(
                        dangerous_tool_auto_approval,
                        approval,
                    )
                {
                    return Err(ModelCallClosureError::InitialToolApprovalMismatch);
                }
                let ordinal = ToolRequestOrdinal::try_from_usize(tool_ordinal)
                    .ok_or(ModelCallClosureError::ToolRequestOrdinalOverflow)?;
                tool_ordinal += 1;
                let request_record = ToolRequest::from_model_proposal(
                    request,
                    session,
                    turn,
                    call.id(),
                    ordinal,
                    proposal.clone(),
                    approval,
                );
                match approval.resolution(request) {
                    Some(resolution) => automatic_approvals.push(resolution),
                    None if request_record.inadmissible_reason().is_none() => {
                        earliest_undecided.get_or_insert(request);
                    }
                    None => {}
                }
                requests.push(request_record);
                SemanticTranscriptEntry::from_validated_parts(
                    entry,
                    session,
                    SemanticTranscriptEntryPayload::AssistantToolUse {
                        producing_call: call.id(),
                        request,
                    },
                )
            }
            _ => return Err(ModelCallClosureError::ToolResponseIdentityMismatch),
        };
        assistant_entries.push(entry);
    }

    let next_phase = match (earliest_undecided, identities.continuation_attempt) {
        (Some(request), None) => ActiveTurnPhase::AwaitingApproval { request },
        (None, Some(continuation)) if continuation != attempt.id() => ActiveTurnPhase::Running {
            current_attempt: CurrentTurnAttempt::prepared(continuation),
        },
        _ => return Err(ModelCallClosureError::ContinuationAttemptIdentityMismatch),
    };
    let source = ResolvedContextFrontierSnapshot::try_from_candidate(
        session,
        call.frontier().snapshot(),
        frontier_entries
            .iter()
            .map(SemanticTranscriptEntry::reference)
            .collect(),
    )
    .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    let yielded_snapshot = source
        .derive_appending_candidate(
            identities.yielded_frontier,
            assistant_entries
                .iter()
                .map(SemanticTranscriptEntry::reference)
                .collect(),
        )
        .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;

    Ok(ToolRoundModelCallTurn {
        session,
        turn,
        call,
        attempt,
        assistant_entries: assistant_entries.into_boxed_slice(),
        requests: requests.into_boxed_slice(),
        automatic_approvals: automatic_approvals.into_boxed_slice(),
        yielded_snapshot,
        next_phase,
    })
}

pub(super) fn initial_tool_approval_matches_posture(
    posture: DangerousToolAutoApproval,
    approval: InitialToolApproval,
) -> bool {
    match (posture, approval) {
        (DangerousToolAutoApproval::ApproveAll, InitialToolApproval::Confirm)
        | (DangerousToolAutoApproval::Disabled, InitialToolApproval::SessionBlanket) => false,
        (
            DangerousToolAutoApproval::ApproveAll,
            InitialToolApproval::AlwaysConfirm
            | InitialToolApproval::SessionBlanket
            | InitialToolApproval::PolicyAuto
            | InitialToolApproval::Human
            | InitialToolApproval::Delegated
            | InitialToolApproval::Inadmissible
            | InitialToolApproval::RuntimeSafetyDeny
            | InitialToolApproval::UserOverride { .. },
        )
        | (
            DangerousToolAutoApproval::Disabled,
            InitialToolApproval::Confirm
            | InitialToolApproval::AlwaysConfirm
            | InitialToolApproval::PolicyAuto
            | InitialToolApproval::Human
            | InitialToolApproval::Delegated
            | InitialToolApproval::Inadmissible
            | InitialToolApproval::RuntimeSafetyDeny
            | InitialToolApproval::UserOverride { .. },
        ) => true,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn assemble_stopped_tool_round(
    scope: ModelCallTurnScope,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    frontier_entries: Vec<SemanticTranscriptEntry>,
    response: ToolUsingAssistantResponse,
    proof: AppliedInterruptProof,
    identities: StoppedToolRoundModelCallIdentities,
    dangerous_tool_auto_approval: DangerousToolAutoApproval,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
) -> Result<CancelledToolRoundModelCallTurn, ModelCallClosureError> {
    let ModelCallTurnScope { session, turn } = scope;
    if proof.predecessor() != turn || response.parts().len() != identities.response_parts.len() {
        return Err(ModelCallClosureError::ToolResponseIdentityMismatch);
    }
    let mut used_entries = frontier_entries
        .iter()
        .map(SemanticTranscriptEntry::identity)
        .collect::<BTreeSet<_>>();
    if !used_entries.insert(identities.cancellation_entry) {
        return Err(ModelCallClosureError::FrontierDerivationFailed);
    }
    let mut used_requests = frontier_entries
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::AssistantToolUse { request, .. } => Some(*request),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut assistant_entries = Vec::with_capacity(response.parts().len());
    let mut requests = Vec::with_capacity(response.tool_count());
    let mut closed_result_entries = Vec::with_capacity(response.tool_count());
    let mut tool_ordinal = 0usize;

    for (part, identity) in response.parts().iter().zip(identities.response_parts) {
        let entry = match (part, identity) {
            (
                AssistantResponsePart::Text(value),
                StoppedToolResponsePartIdentity::Text { entry },
            ) => {
                if !used_entries.insert(entry) {
                    return Err(ModelCallClosureError::FrontierDerivationFailed);
                }
                SemanticTranscriptEntry::from_validated_parts(
                    entry,
                    session,
                    SemanticTranscriptEntryPayload::AssistantText {
                        producing_call: call.id(),
                        value: value.clone(),
                    },
                )
            }
            (
                AssistantResponsePart::ProviderCompaction(block),
                StoppedToolResponsePartIdentity::ProviderCompaction { entry },
            ) => {
                if !used_entries.insert(entry) {
                    return Err(ModelCallClosureError::FrontierDerivationFailed);
                }
                SemanticTranscriptEntry::from_validated_parts(
                    entry,
                    session,
                    SemanticTranscriptEntryPayload::ProviderCompaction {
                        producing_call: call.id(),
                        block: block.clone(),
                    },
                )
            }
            (
                AssistantResponsePart::ProviderReasoning(item),
                StoppedToolResponsePartIdentity::ProviderReasoning { entry },
            ) => {
                if !used_entries.insert(entry) {
                    return Err(ModelCallClosureError::FrontierDerivationFailed);
                }
                SemanticTranscriptEntry::from_validated_parts(
                    entry,
                    session,
                    SemanticTranscriptEntryPayload::ProviderReasoning {
                        producing_call: call.id(),
                        item: item.clone(),
                    },
                )
            }
            (
                AssistantResponsePart::ToolCall(proposal),
                StoppedToolResponsePartIdentity::ToolCall {
                    entry,
                    request,
                    closed_result_entry,
                    approval,
                },
            ) => {
                if !used_entries.insert(entry)
                    || !used_entries.insert(closed_result_entry)
                    || !used_requests.insert(request)
                {
                    return Err(ModelCallClosureError::FrontierDerivationFailed);
                }
                if proposal.is_suppressed() != (approval == InitialToolApproval::RuntimeSafetyDeny)
                    || proposal.inadmissible_reason().is_some()
                        != (approval == InitialToolApproval::Inadmissible)
                    || !initial_tool_approval_matches_posture(
                        dangerous_tool_auto_approval,
                        approval,
                    )
                {
                    return Err(ModelCallClosureError::InitialToolApprovalMismatch);
                }
                let ordinal = ToolRequestOrdinal::try_from_usize(tool_ordinal)
                    .ok_or(ModelCallClosureError::ToolRequestOrdinalOverflow)?;
                tool_ordinal += 1;
                requests.push(ToolRequest::from_model_proposal(
                    request,
                    session,
                    turn,
                    call.id(),
                    ordinal,
                    proposal.clone(),
                    approval,
                ));
                closed_result_entries.push(SemanticTranscriptEntry::from_validated_parts(
                    closed_result_entry,
                    session,
                    if proposal.inadmissible_reason().is_some() {
                        SemanticTranscriptEntryPayload::ToolInadmissible { request }
                    } else {
                        SemanticTranscriptEntryPayload::ToolClosed { request }
                    },
                ));
                SemanticTranscriptEntry::from_validated_parts(
                    entry,
                    session,
                    SemanticTranscriptEntryPayload::AssistantToolUse {
                        producing_call: call.id(),
                        request,
                    },
                )
            }
            _ => return Err(ModelCallClosureError::ToolResponseIdentityMismatch),
        };
        assistant_entries.push(entry);
    }
    let cancellation_entry = SemanticTranscriptEntry::from_validated_parts(
        identities.cancellation_entry,
        session,
        SemanticTranscriptEntryPayload::TurnCancelled { turn },
    );
    let source = ResolvedContextFrontierSnapshot::try_from_candidate(
        session,
        call.frontier().snapshot(),
        frontier_entries
            .iter()
            .map(SemanticTranscriptEntry::reference)
            .collect(),
    )
    .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    let appended = assistant_entries
        .iter()
        .map(SemanticTranscriptEntry::reference)
        .chain(
            closed_result_entries
                .iter()
                .map(SemanticTranscriptEntry::reference),
        )
        .chain([cancellation_entry.reference()])
        .collect();
    let terminal_snapshot = source
        .derive_appending_candidate(identities.terminal_frontier, appended)
        .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    Ok(CancelledToolRoundModelCallTurn {
        session,
        turn,
        call,
        attempt,
        disposition: TurnDisposition::Cancelled { cause: proof },
        assistant_entries: assistant_entries.into_boxed_slice(),
        requests: requests.into_boxed_slice(),
        closed_result_entries: closed_result_entries.into_boxed_slice(),
        cancellation_entry,
        terminal_snapshot,
        reclassified_pending_steering,
    })
}
