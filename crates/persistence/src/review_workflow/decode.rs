use super::load::LoadedReviewPass;
use super::pass_codec::{
    decode_pass_state, pass_state_result, pass_state_turn, stored_pass_result,
};
use super::{
    ReviewTurnLifecycleState, ReviewWorkflowStoreError, accepted_input_id, context_frontier_id,
    corruption, decimal_u64, decode_diff_side, decode_external_object_kind,
    decode_external_object_state, decode_finding_status, decode_pass_kind,
    decode_review_confidence, decode_review_policy, decode_run_state, decode_severity,
    decode_workflow_kind, encode_run_state, external_link_id, finding_id, pass_id, positive_u32,
    review_key, review_text, run_id, session_id, target_id, turn_id,
};
use rust_decimal::Decimal;
use signalbox_domain::{
    ReviewChangeRequestNumber, ReviewEventOrdinal, ReviewExternalLink,
    ReviewExternalLinkAssociation, ReviewExternalLinkAttachment, ReviewExternalLinkId,
    ReviewExternalLinkObservation, ReviewExternalObjectKind, ReviewFindingConfidenceAxes,
    ReviewFindingContent, ReviewFindingEvent, ReviewFindingEventKind, ReviewFindingEventResultKind,
    ReviewFindingExternalLinkRef, ReviewFindingLocation, ReviewFindingPendingExternalLinkRef,
    ReviewFindingProposal, ReviewFindingRef, ReviewKey, ReviewLineRange, ReviewPass,
    ReviewPassAcceptedInputEvidence, ReviewPassEvidence, ReviewPassReconstitutionInput,
    ReviewPassRef, ReviewPassResult, ReviewPassState, ReviewPassTurnEvidence,
    ReviewPassTurnOutcome, ReviewPolicy, ReviewReferencedFindingEvidence, ReviewRun,
    ReviewRunEvidence, ReviewRunReconstitutionInput, ReviewRunRef, ReviewRunState, ReviewTarget,
    ReviewTargetSubject, ReviewWorkflowKind,
};
use sqlx::postgres::PgRow;
use sqlx::types::Uuid;
use sqlx::{Postgres, Row};

pub(super) fn decode_target_row(
    row: &PgRow,
    stack_parent: Option<&ReviewTarget>,
) -> Result<ReviewTarget, ReviewWorkflowStoreError> {
    let id = target_id(row.try_get("target_id")?);
    let provider = review_key(row.try_get("provider_key")?, "review_target")?;
    let repository = review_key(row.try_get("repository_key")?, "review_target")?;
    let subject_kind: String = row.try_get("subject_kind")?;
    let change_request_number: Option<Decimal> = row.try_get("change_request_number")?;
    let subject = decode_target_subject(&subject_kind, change_request_number)?;
    let stored_parent = row
        .try_get::<Option<Uuid>, _>("stack_parent_target_id")?
        .map(target_id);
    if stored_parent != stack_parent.map(ReviewTarget::id) {
        return Err(corruption(
            "review_target",
            String::from("stack ancestry is incomplete or out of order"),
        ));
    }
    ReviewTarget::try_new(
        id,
        provider,
        repository,
        subject,
        review_key(row.try_get("head_revision")?, "review_target")?,
        row.try_get::<Option<String>, _>("base_revision")?
            .map(|value| review_key(value, "review_target"))
            .transpose()?,
        stack_parent,
    )
    .map_err(|error| corruption("review_target", format!("{error:?}")))
}

fn decode_target_subject(
    subject_kind: &str,
    change_request_number: Option<Decimal>,
) -> Result<ReviewTargetSubject, ReviewWorkflowStoreError> {
    Ok(match (subject_kind, change_request_number) {
        ("change_request", Some(number)) => ReviewTargetSubject::ChangeRequest(
            ReviewChangeRequestNumber::try_new(decimal_u64(number, "review_target")?)
                .map_err(|_| corruption("review_target", String::from("zero change request")))?,
        ),
        ("commit", None) => ReviewTargetSubject::Commit,
        _ => {
            return Err(corruption(
                "review_target",
                format!("invalid subject shape {subject_kind}"),
            ));
        }
    })
}

pub(super) fn decode_run(
    row: &PgRow,
    canonical_pass: Option<&LoadedReviewPass>,
) -> Result<ReviewRun, ReviewWorkflowStoreError> {
    let (_, workflow, _, state) = decode_run_facts(row)?;
    let pass_evidence = projected_current_run_pass_evidence(state, workflow, canonical_pass)?;
    reconstitute_run(row, pass_evidence)
}

pub(super) fn decode_run_for_transition(
    row: &PgRow,
    canonical_pass: Option<&LoadedReviewPass>,
) -> Result<(ReviewRun, Option<ReviewPassEvidence>), ReviewWorkflowStoreError> {
    let (_, workflow, _, state) = decode_run_facts(row)?;
    let canonical_evidence = canonical_pass.map(LoadedReviewPass::evidence);
    let current_evidence = projected_current_run_pass_evidence(state, workflow, canonical_pass)?;
    Ok((reconstitute_run(row, current_evidence)?, canonical_evidence))
}

fn reconstitute_run(
    row: &PgRow,
    pass_evidence: Option<ReviewPassEvidence>,
) -> Result<ReviewRun, ReviewWorkflowStoreError> {
    let (reference, workflow, policy, state) = decode_run_facts(row)?;
    ReviewRun::try_reconstitute(ReviewRunReconstitutionInput::new(
        reference,
        workflow,
        policy,
        state,
        pass_evidence,
    ))
    .map_err(|error| {
        corruption(
            "review_run",
            format!("domain reconstitution failed: {:?}", error.failure()),
        )
    })
}

pub(super) fn decode_run_facts(
    row: &PgRow,
) -> Result<
    (
        ReviewRunRef,
        ReviewWorkflowKind,
        ReviewPolicy,
        ReviewRunState,
    ),
    ReviewWorkflowStoreError,
> {
    let reference = ReviewRunRef::new(
        target_id(row.try_get("target_id")?),
        run_id(row.try_get("run_id")?),
    );
    let policy = decode_review_policy(
        row,
        "policy_version",
        "minimum_judge_confidence",
        "minimum_publication_confidence",
        "review_run",
    )?;
    let workflow: String = row.try_get("workflow_kind")?;
    let state_kind: String = row.try_get("state_kind")?;
    let state_pass: Option<Uuid> = row.try_get("state_pass_id")?;
    let state = decode_run_state(reference, &state_kind, state_pass)?;
    Ok((reference, decode_workflow_kind(&workflow)?, policy, state))
}

fn projected_current_run_pass_evidence(
    state: ReviewRunState,
    workflow: ReviewWorkflowKind,
    canonical: Option<&LoadedReviewPass>,
) -> Result<Option<ReviewPassEvidence>, ReviewWorkflowStoreError> {
    let Some(expected) = encode_run_state(state).1 else {
        return Ok(canonical.map(LoadedReviewPass::evidence));
    };
    let Some(canonical) = canonical else {
        return Err(corruption(
            "review_run",
            String::from("referenced pass row is missing"),
        ));
    };
    if canonical.pass.reference().pass() != expected {
        return Err(corruption(
            "review_run",
            String::from("canonical pass identity mismatch"),
        ));
    }
    if !matches!(state, ReviewRunState::Running { .. }) {
        return Ok(Some(canonical.evidence()));
    }
    let projected_state = ReviewPassState::Running {
        turn: pass_state_turn(canonical.pass.state()).ok_or_else(|| {
            corruption(
                "review_run",
                String::from("canonical pass has no turn for running projection"),
            )
        })?,
    };
    let projected = ReviewPass::try_reconstitute(ReviewPassReconstitutionInput::new(
        canonical.pass.reference(),
        canonical.pass.kind(),
        canonical.pass.reference().run(),
        workflow,
        canonical.pass.session(),
        canonical.pass.accepted_input(),
        ReviewPassAcceptedInputEvidence::new(
            canonical.pass.accepted_input(),
            canonical.pass.session(),
            Some(canonical.pass.origin_turn()),
        ),
        projected_state,
        canonical.turn_evidence,
    ))
    .map_err(|error| {
        corruption(
            "review_run",
            format!(
                "canonical pass cannot support running projection: {:?}",
                error.failure()
            ),
        )
    })?;
    Ok(Some(ReviewPassEvidence::from_pass(
        &projected,
        canonical.policy,
    )))
}

pub(super) fn reconstitute_pass(
    row: &PgRow,
    turn_evidence: Option<ReviewPassTurnEvidence>,
    produced_findings: Vec<ReviewFindingRef>,
    referenced_producer: Option<&LoadedReviewPass>,
) -> Result<ReviewPass, ReviewWorkflowStoreError> {
    let reference = ReviewPassRef::new(
        ReviewRunRef::new(
            target_id(row.try_get("target_id")?),
            run_id(row.try_get("run_id")?),
        ),
        pass_id(row.try_get("pass_id")?),
    );
    let state_kind: String = row.try_get("state_kind")?;
    let turn: Option<Uuid> = row.try_get("turn_id")?;
    let frontier: Option<Uuid> = row.try_get("output_frontier_id")?;
    let workflow_run_id = row
        .try_get::<Option<Uuid>, _>("canonical_run_id")?
        .ok_or_else(|| corruption("review_pass", String::from("referenced run row is missing")))?;
    let workflow_run_target = row
        .try_get::<Option<Uuid>, _>("canonical_run_target_id")?
        .ok_or_else(|| corruption("review_pass", String::from("referenced run row is missing")))?;
    ReviewPass::try_reconstitute(ReviewPassReconstitutionInput::new(
        reference,
        decode_pass_kind(&row.try_get::<String, _>("pass_kind")?)?,
        ReviewRunRef::new(target_id(workflow_run_target), run_id(workflow_run_id)),
        decode_workflow_kind(&row.try_get::<String, _>("run_workflow_kind")?)?,
        session_id(row.try_get("pass_session_id")?),
        accepted_input_id(row.try_get("accepted_input_id")?),
        decode_pass_accepted_input_evidence(row)?,
        decode_pass_state(
            &state_kind,
            turn,
            frontier,
            stored_pass_result(row, "target_id", "")?,
            produced_findings,
            referenced_producer,
        )?,
        turn_evidence,
    ))
    .map_err(|error| {
        corruption(
            "review_pass",
            format!("domain reconstitution failed: {:?}", error.failure()),
        )
    })
}

pub(super) fn decode_pass_accepted_input_evidence(
    row: &PgRow,
) -> Result<ReviewPassAcceptedInputEvidence, ReviewWorkflowStoreError> {
    let stored_origin = row.try_get::<Uuid, _>("origin_turn_id")?;
    let canonical_origin = row.try_get::<Option<Uuid>, _>("accepted_input_origin_turn_id")?;
    if canonical_origin != Some(stored_origin) {
        return Err(corruption(
            "review_pass",
            String::from("accepted input origin turn differs from stored pass"),
        ));
    }
    Ok(ReviewPassAcceptedInputEvidence::new(
        accepted_input_id(row.try_get("accepted_input_id")?),
        session_id(
            row.try_get::<Option<Uuid>, _>("accepted_input_session_id")?
                .ok_or_else(|| {
                    corruption("review_pass", String::from("accepted input row is missing"))
                })?,
        ),
        canonical_origin.map(turn_id),
    ))
}

pub(super) fn decode_pass_turn_evidence(
    row: &PgRow,
) -> Result<Option<ReviewPassTurnEvidence>, ReviewWorkflowStoreError> {
    let turn: Option<Uuid> = row.try_get("evidence_turn_id")?;
    let session: Option<Uuid> = row.try_get("turn_session_id")?;
    let accepted_input: Option<Uuid> = row.try_get("turn_accepted_input_id")?;
    let state: Option<String> = row.try_get("turn_state_kind")?;
    let disposition: Option<String> = row.try_get("turn_terminal_disposition_kind")?;
    let frontier: Option<Uuid> = row.try_get("turn_terminal_frontier_id")?;
    match (turn, session, accepted_input, state) {
        (None, None, None, None) if disposition.is_none() && frontier.is_none() => Ok(None),
        (Some(turn), Some(session), Some(accepted_input), Some(state)) => {
            Ok(Some(ReviewPassTurnEvidence::new(
                turn_id(turn),
                session_id(session),
                accepted_input_id(accepted_input),
                decode_turn_outcome("review_pass", &state, disposition.as_deref())?,
                frontier.map(context_frontier_id),
            )))
        }
        _ => Err(corruption(
            "review_pass",
            String::from("torn canonical turn evidence"),
        )),
    }
}

/// Reads one column of a returned row, reporting a decode failure as corruption.
///
/// The fetch keeps `?`: a connection that dropped mid-query is retryable, and
/// `mutation_unavailable` is the honest answer. A column read on a row the
/// query already returned is not — a type or a name the projection and the
/// schema disagree about is a fault no retry repairs, and the handlers these
/// projections replaced answered it with the internal projection diagnostic.
pub(super) fn projected<'row, T>(
    row: &'row PgRow,
    aggregate: &'static str,
    column: &'static str,
) -> Result<T, ReviewWorkflowStoreError>
where
    T: sqlx::Decode<'row, Postgres> + sqlx::Type<Postgres>,
{
    row.try_get(column).map_err(|error| {
        // The driver's `Display` output is unstable prose and nothing downstream
        // can match on it, so the classification carries labels instead: the
        // aggregate, the static column name, and which of the ways a row that
        // was returned can still fail to answer for one of its columns.
        let failure = match error {
            sqlx::Error::ColumnNotFound(_) => "absent",
            sqlx::Error::ColumnDecode { .. } => "undecodable",
            _ => "unreadable",
        };
        corruption(aggregate, format!("{failure} column {column}"))
    })
}

/// Decodes one turn outcome, reporting corruption against the reading table.
///
/// The same state/disposition pair is stored on `turn_lifecycle` and copied
/// onto the `review_pass` evidence columns, so the caller names which of the
/// two it read: a corruption report that named the other one would send a
/// reader to a table whose rows are intact.
fn decode_turn_outcome(
    aggregate: &'static str,
    state: &str,
    disposition: Option<&str>,
) -> Result<ReviewPassTurnOutcome, ReviewWorkflowStoreError> {
    match (state, disposition) {
        ("active", None) => Ok(ReviewPassTurnOutcome::Active),
        ("terminal", Some("completed")) => Ok(ReviewPassTurnOutcome::Completed),
        ("terminal", Some("refused")) => Ok(ReviewPassTurnOutcome::Refused),
        ("terminal", Some("failed")) => Ok(ReviewPassTurnOutcome::Failed),
        ("terminal", Some("cancelled")) => Ok(ReviewPassTurnOutcome::Cancelled),
        ("terminal", Some("reconciliation_required")) => {
            Ok(ReviewPassTurnOutcome::ReconciliationRequired)
        }
        _ => Err(corruption(
            aggregate,
            format!("invalid canonical turn outcome {state}/{disposition:?}"),
        )),
    }
}

pub(super) fn decode_turn_lifecycle_state(
    state: &str,
    disposition: Option<&str>,
) -> Result<ReviewTurnLifecycleState, ReviewWorkflowStoreError> {
    match (state, disposition) {
        ("queued", None) => Ok(ReviewTurnLifecycleState::Queued),
        ("active", None) => Ok(ReviewTurnLifecycleState::Active),
        ("terminal", Some(_)) => Ok(ReviewTurnLifecycleState::Terminal(decode_turn_outcome(
            "turn_lifecycle",
            state,
            disposition,
        )?)),
        _ => Err(corruption(
            "turn_lifecycle",
            format!("invalid turn lifecycle state {state}/{disposition:?}"),
        )),
    }
}

pub(super) fn decode_finding_proposal(
    row: &PgRow,
    producing_pass: ReviewPassEvidence,
    target: &ReviewTarget,
) -> Result<ReviewFindingProposal, ReviewWorkflowStoreError> {
    let run = ReviewRunRef::new(
        target_id(row.try_get("target_id")?),
        run_id(row.try_get("run_id")?),
    );
    let producing_pass_reference =
        ReviewPassRef::new(run, pass_id(row.try_get("producing_pass_id")?));
    if producing_pass.reference() != producing_pass_reference {
        return Err(corruption(
            "review_finding",
            String::from("producing pass ancestry mismatch"),
        ));
    }
    let producing_run = ReviewRunEvidence::new(
        run,
        decode_workflow_kind(&row.try_get::<String, _>("producing_workflow_kind")?)?,
        producing_pass.policy(),
        decode_run_state(
            run,
            &row.try_get::<String, _>("producing_run_state_kind")?,
            row.try_get("producing_run_state_pass_id")?,
        )?,
    );
    let reference = ReviewFindingRef::new(
        producing_pass_reference,
        finding_id(row.try_get("finding_id")?),
    );
    let line_start: Option<i64> = row.try_get("line_start")?;
    let line_end: Option<i64> = row.try_get("line_end")?;
    let line_range = match (line_start, line_end) {
        (None, None) => None,
        (Some(start), Some(end)) => Some(
            ReviewLineRange::try_new(
                positive_u32(start, "review_finding")?,
                positive_u32(end, "review_finding")?,
            )
            .map_err(|error| corruption("review_finding", format!("{error:?}")))?,
        ),
        _ => {
            return Err(corruption(
                "review_finding",
                String::from("torn line range"),
            ));
        }
    };
    let content = ReviewFindingContent::new(
        ReviewFindingLocation::new(
            review_key(row.try_get("file_path")?, "review_finding")?,
            line_range,
            row.try_get::<Option<String>, _>("diff_side")?
                .map(|side| decode_diff_side(&side))
                .transpose()?,
        ),
        review_text(row.try_get("title")?, "review_finding")?,
        review_text(row.try_get("body")?, "review_finding")?,
        decode_severity(&row.try_get::<String, _>("severity")?)?,
        ReviewFindingConfidenceAxes::new(
            decode_review_confidence(row.try_get("is_real_confidence")?, "review_finding")?,
            decode_review_confidence(row.try_get("severity_label_confidence")?, "review_finding")?,
        ),
        review_key(row.try_get("category")?, "review_finding")?,
        row.try_get::<Option<String>, _>("recommended_fix")?
            .map(|value| review_text(value, "review_finding"))
            .transpose()?,
    );
    ReviewFindingProposal::try_new(reference, producing_pass, producing_run, target, content)
        .map_err(|error| corruption("review_finding", format!("{error:?}")))
}

pub(super) fn decode_finding_event(
    row: &PgRow,
    finding: ReviewFindingRef,
    target_snapshot: &ReviewTarget,
    pass: ReviewPassEvidence,
    canonical_link: Option<&ReviewExternalLink>,
) -> Result<ReviewFindingEvent, ReviewWorkflowStoreError> {
    let row_finding = finding_id(row.try_get("finding_id")?);
    let row_run = run_id(row.try_get("finding_run_id")?);
    let row_target = target_id(row.try_get("target_id")?);
    if row_finding != finding.finding()
        || row_run != finding.run().run()
        || row_target != finding.target()
    {
        return Err(corruption(
            "review_finding_event",
            String::from("finding ancestry mismatch"),
        ));
    }
    let pass_reference = ReviewPassRef::new(
        ReviewRunRef::new(row_target, run_id(row.try_get("event_pass_run_id")?)),
        pass_id(row.try_get("event_pass_id")?),
    );
    if pass.reference() != pass_reference {
        return Err(corruption(
            "review_finding_event",
            String::from("event pass ancestry mismatch"),
        ));
    }
    let run = ReviewRunEvidence::new(
        pass_reference.run(),
        decode_workflow_kind(&row.try_get::<String, _>("event_workflow_kind")?)?,
        pass.policy(),
        decode_run_state(
            pass_reference.run(),
            &row.try_get::<String, _>("event_run_state_kind")?,
            row.try_get("event_run_state_pass_id")?,
        )?,
    );
    let kind: String = row.try_get("event_kind")?;
    let reason: Option<String> = row.try_get("reason")?;
    let referenced: Option<Uuid> = row.try_get("referenced_finding_id")?;
    let referenced_run: Option<Uuid> = row.try_get("referenced_finding_run_id")?;
    let referenced_target: Option<Uuid> = row.try_get("referenced_finding_target_id")?;
    let referenced_pass: Option<Uuid> = row.try_get("referenced_finding_pass_id")?;
    let referenced_status: Option<String> = row.try_get("referenced_finding_status")?;
    let external_link: Option<Uuid> = row.try_get("external_link_id")?;
    let kind = match (
        kind.as_str(),
        reason,
        referenced,
        referenced_run,
        referenced_target,
        referenced_pass,
        referenced_status,
        external_link,
    ) {
        ("accepted", None, None, None, None, None, None, None) => ReviewFindingEventKind::Accepted,
        ("rejected", Some(reason), None, None, None, None, None, None) => {
            ReviewFindingEventKind::Rejected {
                reason: review_text(reason, "review_finding_event")?,
            }
        }
        (
            "duplicate",
            None,
            Some(referenced),
            Some(referenced_run),
            Some(referenced_target),
            Some(referenced_pass),
            Some(referenced_status),
            None,
        ) => ReviewFindingEventKind::Duplicate {
            canonical: decode_event_referenced_finding(
                &pass,
                "duplicate",
                ReviewFindingRef::new(
                    ReviewPassRef::new(
                        ReviewRunRef::new(target_id(referenced_target), run_id(referenced_run)),
                        pass_id(referenced_pass),
                    ),
                    finding_id(referenced),
                ),
                &referenced_status,
            )?,
        },
        (
            "superseded",
            None,
            Some(referenced),
            Some(referenced_run),
            Some(referenced_target),
            Some(referenced_pass),
            Some(referenced_status),
            None,
        ) => ReviewFindingEventKind::Superseded {
            successor: decode_event_referenced_finding(
                &pass,
                "superseded",
                ReviewFindingRef::new(
                    ReviewPassRef::new(
                        ReviewRunRef::new(target_id(referenced_target), run_id(referenced_run)),
                        pass_id(referenced_pass),
                    ),
                    finding_id(referenced),
                ),
                &referenced_status,
            )?,
        },
        ("stale", None, None, None, None, None, None, None) => ReviewFindingEventKind::Stale,
        ("posted", None, None, None, None, None, None, Some(link)) => {
            let canonical =
                require_finding_event_external_link(canonical_link, external_link_id(link))?;
            ReviewFindingEventKind::Posted {
                link: Box::new(
                    ReviewFindingExternalLinkRef::try_new(finding, canonical).map_err(|error| {
                        corruption(
                            "review_finding_event",
                            format!("invalid canonical posted link: {:?}", error.failure()),
                        )
                    })?,
                ),
            }
        }
        ("fixed", None, None, None, None, None, None, None) => ReviewFindingEventKind::Fixed,
        ("blocked_with_reason", Some(reason), None, None, None, None, None, link) => {
            let link = link
                .map(|link| {
                    let canonical = require_finding_event_external_link(
                        canonical_link,
                        external_link_id(link),
                    )?;
                    let reservation = ReviewExternalLink::try_reserve(
                        canonical.id(),
                        canonical.association(),
                        canonical.provider().clone(),
                        canonical.object_kind(),
                        target_snapshot,
                    )
                    .map_err(|error| {
                        corruption(
                            "review_finding_event",
                            format!("invalid historical publication reservation: {error:?}"),
                        )
                    })?;
                    ReviewFindingPendingExternalLinkRef::try_new(finding, &reservation)
                        .map(Box::new)
                        .map_err(|error| {
                            corruption(
                                "review_finding_event",
                                format!("invalid pending publication link: {:?}", error.failure()),
                            )
                        })
                })
                .transpose()?;
            ReviewFindingEventKind::BlockedWithReason {
                reason: review_text(reason, "review_finding_event")?,
                link,
            }
        }
        _ => {
            return Err(corruption(
                "review_finding_event",
                format!("invalid event shape {kind}"),
            ));
        }
    };
    Ok(ReviewFindingEvent::new(
        finding,
        ReviewEventOrdinal::try_new(positive_u32(
            row.try_get("event_ordinal")?,
            "review_finding_event",
        )?)
        .map_err(|_| corruption("review_finding_event", String::from("zero ordinal")))?,
        pass_reference,
        pass,
        run,
        kind,
    ))
}

fn decode_event_referenced_finding(
    pass: &ReviewPassEvidence,
    expected_kind: &str,
    reference: ReviewFindingRef,
    stored_status: &str,
) -> Result<ReviewReferencedFindingEvidence, ReviewWorkflowStoreError> {
    let canonical = match (expected_kind, pass_state_result(pass.state())) {
        ("duplicate", Some(ReviewPassResult::FindingEvent(result))) => match result.kind() {
            ReviewFindingEventResultKind::Duplicate { canonical } => Some(*canonical),
            _ => None,
        },
        ("superseded", Some(ReviewPassResult::FindingEvent(result))) => match result.kind() {
            ReviewFindingEventResultKind::Superseded { successor } => Some(*successor),
            _ => None,
        },
        _ => None,
    }
    .ok_or_else(|| {
        corruption(
            "review_finding_event",
            String::from("event pass omitted its authenticated reference result"),
        )
    })?;
    let status = decode_finding_status(stored_status)?;
    if canonical.reference() != reference || canonical.status() != status {
        return Err(corruption(
            "review_finding_event",
            String::from("event reference differs from its pass result"),
        ));
    }
    Ok(canonical)
}

fn require_finding_event_external_link(
    canonical_link: Option<&ReviewExternalLink>,
    event_link: ReviewExternalLinkId,
) -> Result<&ReviewExternalLink, ReviewWorkflowStoreError> {
    let canonical = canonical_link.ok_or_else(|| {
        corruption(
            "review_finding_event",
            String::from("event canonical link is missing"),
        )
    })?;
    if canonical.id() != event_link {
        return Err(corruption(
            "review_finding_event",
            String::from("event link identity mismatch"),
        ));
    }
    Ok(canonical)
}

pub(super) fn decode_external_link_root(
    row: &PgRow,
) -> Result<
    (
        ReviewExternalLinkId,
        ReviewExternalLinkAssociation,
        ReviewKey,
        ReviewExternalObjectKind,
    ),
    ReviewWorkflowStoreError,
> {
    let id = external_link_id(row.try_get("external_link_id")?);
    let target = target_id(row.try_get("target_id")?);
    let association_kind: String = row.try_get("association_kind")?;
    let run: Option<Uuid> = row.try_get("run_id")?;
    let finding: Option<Uuid> = row.try_get("finding_id")?;
    let finding_pass: Option<Uuid> = row.try_get("finding_producing_pass_id")?;
    let association = match (association_kind.as_str(), run, finding, finding_pass) {
        ("target", None, None, None) => ReviewExternalLinkAssociation::Target(target),
        ("run", Some(run), None, None) => {
            ReviewExternalLinkAssociation::Run(ReviewRunRef::new(target, run_id(run)))
        }
        ("finding", Some(run), Some(finding), Some(finding_pass)) => {
            ReviewExternalLinkAssociation::Finding(ReviewFindingRef::new(
                ReviewPassRef::new(
                    ReviewRunRef::new(target, run_id(run)),
                    pass_id(finding_pass),
                ),
                finding_id(finding),
            ))
        }
        _ => {
            return Err(corruption(
                "review_external_link",
                format!("invalid association shape {association_kind}"),
            ));
        }
    };
    Ok((
        id,
        association,
        review_key(row.try_get("link_provider_key")?, "review_external_link")?,
        decode_external_object_kind(&row.try_get::<String, _>("object_kind")?)?,
    ))
}

pub(super) fn decode_external_link_attachment(
    row: &PgRow,
    pass: ReviewPassEvidence,
) -> Result<ReviewExternalLinkAttachment, ReviewWorkflowStoreError> {
    let reference = ReviewPassRef::new(
        ReviewRunRef::new(
            target_id(row.try_get("target_id")?),
            run_id(row.try_get("pass_run_id")?),
        ),
        pass_id(row.try_get("pass_id")?),
    );
    if pass.reference() != reference {
        return Err(corruption(
            "review_external_link_attachment",
            String::from("attaching pass ancestry mismatch"),
        ));
    }
    Ok(ReviewExternalLinkAttachment::new(
        external_link_id(row.try_get("external_link_id")?),
        reference,
        pass.clone(),
        ReviewRunEvidence::new(
            reference.run(),
            decode_workflow_kind(&row.try_get::<String, _>("pass_workflow_kind")?)?,
            pass.policy(),
            decode_run_state(
                reference.run(),
                &row.try_get::<String, _>("pass_run_state_kind")?,
                row.try_get("pass_run_state_pass_id")?,
            )?,
        ),
        review_key(
            row.try_get("external_object_key")?,
            "review_external_link_attachment",
        )?,
    ))
}

pub(super) fn authenticate_external_object_target(
    attached_target: &ReviewTarget,
    logical_target: &ReviewTarget,
) -> Result<(), ReviewWorkflowStoreError> {
    if attached_target.id() == logical_target.id() {
        return Ok(());
    }
    let same_change_request = matches!(
        (attached_target.subject(), logical_target.subject()),
        (
            ReviewTargetSubject::ChangeRequest(attached),
            ReviewTargetSubject::ChangeRequest(logical),
        ) if attached == logical
    );
    if attached_target.provider() != logical_target.provider()
        || attached_target.repository() != logical_target.repository()
        || !same_change_request
    {
        return Err(corruption(
            "review_external_link_attachment",
            String::from("external object identity names an unrelated logical target"),
        ));
    }
    Ok(())
}

pub(super) fn decode_external_link_observation(
    row: &PgRow,
    pass: ReviewPassEvidence,
) -> Result<ReviewExternalLinkObservation, ReviewWorkflowStoreError> {
    let reference = ReviewPassRef::new(
        ReviewRunRef::new(
            target_id(row.try_get("target_id")?),
            run_id(row.try_get("pass_run_id")?),
        ),
        pass_id(row.try_get("pass_id")?),
    );
    if pass.reference() != reference {
        return Err(corruption(
            "review_external_link_observation",
            String::from("observing pass ancestry mismatch"),
        ));
    }
    Ok(ReviewExternalLinkObservation::new(
        external_link_id(row.try_get("external_link_id")?),
        ReviewEventOrdinal::try_new(positive_u32(
            row.try_get("observation_ordinal")?,
            "review_external_link_observation",
        )?)
        .map_err(|_| {
            corruption(
                "review_external_link_observation",
                String::from("zero ordinal"),
            )
        })?,
        reference,
        pass.clone(),
        ReviewRunEvidence::new(
            reference.run(),
            decode_workflow_kind(&row.try_get::<String, _>("run_workflow_kind")?)?,
            pass.policy(),
            decode_run_state(
                reference.run(),
                &row.try_get::<String, _>("run_state_kind")?,
                row.try_get("run_state_pass_id")?,
            )?,
        ),
        decode_external_object_state(&row.try_get::<String, _>("object_state")?)?,
    ))
}

pub(super) fn same_reservation(left: &ReviewExternalLink, right: &ReviewExternalLink) -> bool {
    left.id() == right.id()
        && left.association() == right.association()
        && left.provider() == right.provider()
        && left.object_kind() == right.object_kind()
}

pub(super) fn same_pass_execution(left: &ReviewPassEvidence, right: &ReviewPassEvidence) -> bool {
    if left.reference() != right.reference()
        || left.kind() != right.kind()
        || left.policy() != right.policy()
    {
        return false;
    }
    match (left.state(), right.state()) {
        (ReviewPassState::Queued, ReviewPassState::Queued) => true,
        (ReviewPassState::Running { turn: left }, ReviewPassState::Running { turn: right })
        | (ReviewPassState::Failed { turn: left }, ReviewPassState::Failed { turn: right }) => {
            left == right
        }
        (
            ReviewPassState::Succeeded {
                turn: left_turn,
                output_frontier: left_frontier,
                ..
            },
            ReviewPassState::Succeeded {
                turn: right_turn,
                output_frontier: right_frontier,
                ..
            },
        ) => left_turn == right_turn && left_frontier == right_frontier,
        (
            ReviewPassState::Blocked {
                turn: left_turn, ..
            },
            ReviewPassState::Blocked {
                turn: right_turn, ..
            },
        ) => left_turn == right_turn,
        (ReviewPassState::Cancelled { turn: left }, ReviewPassState::Cancelled { turn: right }) => {
            left == right
        }
        _ => false,
    }
}
