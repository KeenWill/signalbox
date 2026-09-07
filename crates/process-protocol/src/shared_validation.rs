//! Shared validation wire representations and validation.

use crate::delegation::{ToolApprovalEventDecider, ToolApprovalEventDecision};
use crate::review::{
    ReviewFindingEvent, ReviewJudgmentDisposition, ReviewOrchestrationConcernStatus,
    ReviewOrchestrationSnapshot, ReviewOrchestrationState,
};
use crate::scalars::{FrameValidationError, MAX_REVIEW_ORCHESTRATION_MEMBERS};
use signalbox_domain::{ToolDecisionRationale, ToolDenialReason};
use std::collections::HashSet;

pub(crate) fn validate_tool_approval_event_shape(
    decision: &ToolApprovalEventDecision,
    decider: &ToolApprovalEventDecider,
    rationale: &Option<String>,
) -> Result<(), FrameValidationError> {
    let shape_matches = match decider {
        ToolApprovalEventDecider::User { .. } => match decision {
            ToolApprovalEventDecision::Approve {}
            | ToolApprovalEventDecision::Deny { reason: None } => rationale.is_none(),
            ToolApprovalEventDecision::Deny {
                reason: Some(reason),
            } => rationale.is_none() && ToolDenialReason::try_new(reason.clone()).is_ok(),
        },
        ToolApprovalEventDecider::Delegate { .. } => match decision {
            ToolApprovalEventDecision::Approve {} => rationale
                .as_ref()
                .is_some_and(|rationale| ToolDecisionRationale::try_new(rationale.clone()).is_ok()),
            // A delegate denial's reason is exactly the derivation from its
            // rationale: absent only when the rationale derives nothing.
            ToolApprovalEventDecision::Deny { reason } => {
                rationale.as_ref().is_some_and(|rationale| {
                    ToolDecisionRationale::try_new(rationale.clone()).is_ok_and(|rationale| {
                        ToolDenialReason::from_rationale(&rationale)
                            .as_ref()
                            .map(ToolDenialReason::as_str)
                            == reason.as_deref()
                    })
                })
            }
        },
        ToolApprovalEventDecider::UserOverride { .. } => {
            matches!(decision, ToolApprovalEventDecision::Approve {}) && rationale.is_none()
        }
    };
    if !shape_matches {
        return Err(FrameValidationError::ToolApprovalShape);
    }
    Ok(())
}

/// Validates the structural display-title shape: nonempty single-line text
/// without U+0000 and no leading or trailing ASCII space or tab.
pub(crate) fn validate_imported_display_title(title: &str) -> Result<(), FrameValidationError> {
    if title.is_empty()
        || title.contains(['\0', '\n', '\r'])
        || title.starts_with([' ', '\t'])
        || title.ends_with([' ', '\t'])
    {
        return Err(FrameValidationError::ConversationListShape);
    }
    Ok(())
}

pub(crate) fn validate_session_template_name(value: &str) -> Result<(), FrameValidationError> {
    let first_is_admitted = value
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
    if value.len() > 128
        || !first_is_admitted
        || value.bytes().any(|byte| {
            !byte.is_ascii_lowercase() && !byte.is_ascii_digit() && !b"._-".contains(&byte)
        })
    {
        return Err(FrameValidationError::TemplateShape);
    }
    Ok(())
}

pub(crate) fn validate_review_key(value: &str) -> Result<(), FrameValidationError> {
    if value.is_empty() || value.len() > 1_024 || value.contains('\0') {
        return Err(FrameValidationError::ReviewShape);
    }
    Ok(())
}

fn validate_review_text(value: &str) -> Result<(), FrameValidationError> {
    if value.is_empty() || value.len() > 65_536 || value.contains('\0') {
        return Err(FrameValidationError::ReviewShape);
    }
    Ok(())
}

pub(crate) fn validate_review_judgment_disposition(
    disposition: &ReviewJudgmentDisposition,
) -> Result<(), FrameValidationError> {
    if let ReviewJudgmentDisposition::Rejected { reason } = disposition {
        validate_review_text(reason)?;
    }
    Ok(())
}

pub(crate) fn validate_review_finding_event(
    event: &ReviewFindingEvent,
) -> Result<(), FrameValidationError> {
    match event {
        ReviewFindingEvent::Rejected { reason }
        | ReviewFindingEvent::BlockedWithReason { reason, .. } => validate_review_text(reason),
        ReviewFindingEvent::Accepted {}
        | ReviewFindingEvent::Duplicate { .. }
        | ReviewFindingEvent::Superseded { .. }
        | ReviewFindingEvent::Stale {}
        | ReviewFindingEvent::Fixed {} => Ok(()),
    }
}

pub(crate) fn validate_review_orchestration_snapshot(
    snapshot: &ReviewOrchestrationSnapshot,
) -> Result<(), FrameValidationError> {
    validate_review_key(&snapshot.concern_set_version)?;
    if snapshot.concerns.is_empty() || snapshot.concerns.len() > MAX_REVIEW_ORCHESTRATION_MEMBERS {
        return Err(FrameValidationError::ReviewShape);
    }
    let mut keys = HashSet::new();
    for concern in &snapshot.concerns {
        validate_review_key(&concern.key)?;
        if !keys.insert(&concern.key) {
            return Err(FrameValidationError::ReviewShape);
        }
        let valid_pass = match concern.status {
            ReviewOrchestrationConcernStatus::Pending => concern.pass_id.is_none(),
            ReviewOrchestrationConcernStatus::Succeeded
            | ReviewOrchestrationConcernStatus::Failed
            | ReviewOrchestrationConcernStatus::Blocked
            | ReviewOrchestrationConcernStatus::Superseded => concern.pass_id.is_some(),
            ReviewOrchestrationConcernStatus::Cancelled => true,
        };
        if !valid_pass {
            return Err(FrameValidationError::ReviewShape);
        }
    }
    let pending_concern_count = snapshot
        .concerns
        .iter()
        .filter(|concern| concern.status == ReviewOrchestrationConcernStatus::Pending)
        .count();
    let all_concerns_succeeded = snapshot
        .concerns
        .iter()
        .all(|concern| concern.status == ReviewOrchestrationConcernStatus::Succeeded);
    let counts = snapshot.counts;
    let no_judgment_or_terminal_counts = counts.judgment_member_count.value() == 0
        && counts.judgment_effect_applied_count.value() == 0
        && counts.repair_fixed_count.value() == 0
        && counts.publication_published_count.value() == 0;
    let judgment_is_complete =
        counts.judgment_effect_applied_count.value() == counts.judgment_member_count.value();
    let judgment_is_incomplete =
        counts.judgment_member_count.value() > counts.judgment_effect_applied_count.value();
    let state_matches_facts = match snapshot.state {
        ReviewOrchestrationState::AwaitingImport | ReviewOrchestrationState::ImportIncomplete => {
            pending_concern_count == snapshot.concerns.len()
                && counts.finding_count.value() == 0
                && no_judgment_or_terminal_counts
        }
        ReviewOrchestrationState::AwaitingConcerns => {
            pending_concern_count > 0 && no_judgment_or_terminal_counts
        }
        ReviewOrchestrationState::FanoutIncomplete => {
            pending_concern_count == 0 && !all_concerns_succeeded && no_judgment_or_terminal_counts
        }
        ReviewOrchestrationState::AwaitingJudgment => {
            all_concerns_succeeded && no_judgment_or_terminal_counts
        }
        ReviewOrchestrationState::AwaitingJudgmentEffects
        | ReviewOrchestrationState::JudgmentIncomplete => {
            all_concerns_succeeded
                && judgment_is_incomplete
                && counts.repair_fixed_count.value() == 0
                && counts.publication_published_count.value() == 0
        }
        ReviewOrchestrationState::AwaitingRepair => {
            all_concerns_succeeded
                && judgment_is_complete
                && counts.repair_fixed_count.value() == 0
                && counts.publication_published_count.value() == 0
        }
        ReviewOrchestrationState::RepairIncomplete => {
            all_concerns_succeeded
                && judgment_is_complete
                && counts.publication_published_count.value() == 0
        }
        ReviewOrchestrationState::AwaitingPublication => {
            all_concerns_succeeded
                && judgment_is_complete
                && counts.publication_published_count.value() == 0
        }
        ReviewOrchestrationState::PublicationIncomplete => {
            all_concerns_succeeded
                && judgment_is_complete
                && counts.publication_published_count.value() < counts.judgment_member_count.value()
        }
        ReviewOrchestrationState::Complete => all_concerns_succeeded && judgment_is_complete,
    };
    if !state_matches_facts {
        return Err(FrameValidationError::ReviewShape);
    }
    if counts.finding_count.value() > MAX_REVIEW_ORCHESTRATION_MEMBERS as u64
        || counts.judgment_member_count.value() > counts.finding_count.value()
        || counts.judgment_effect_applied_count.value() > counts.judgment_member_count.value()
        || counts.repair_fixed_count.value() > counts.judgment_member_count.value()
        || counts.publication_published_count.value() > counts.judgment_member_count.value()
        || counts
            .repair_fixed_count
            .value()
            .checked_add(counts.publication_published_count.value())
            .is_none_or(|terminal_count| terminal_count > counts.judgment_member_count.value())
    {
        return Err(FrameValidationError::ReviewShape);
    }
    Ok(())
}
