use super::load::LoadedReviewPass;
use super::{
    ReviewWorkflowStoreError, context_frontier_id, corruption, decode_external_object_state,
    decode_finding_status, external_link_id, finding_id, pass_id, positive_u32, review_key,
    review_text, run_id, target_id, turn_id,
};
use signalbox_domain::{
    ContextFrontierId, ReviewEventOrdinal, ReviewExternalLinkAttachmentResult,
    ReviewExternalLinkId, ReviewExternalLinkNoChangeResult, ReviewExternalLinkObservationResult,
    ReviewExternalLinkPublicationBlockedResult, ReviewExternalObjectState,
    ReviewFindingEventResult, ReviewFindingEventResultKind, ReviewFindingRef, ReviewPassRef,
    ReviewPassResult, ReviewPassState, ReviewProducedFindings, ReviewReferencedFindingEvidence,
    ReviewRunRef, TurnId,
};
use sqlx::Row;
use sqlx::postgres::PgRow;
use sqlx::types::Uuid;

pub(super) struct EncodedPassState<'a> {
    pub(super) kind: &'static str,
    pub(super) turn: Option<TurnId>,
    pub(super) frontier: Option<ContextFrontierId>,
    pub(super) result: Option<EncodedPassResult<'a>>,
}

#[derive(Clone, Copy)]
pub(super) struct EncodedPassResult<'a> {
    pub(super) kind: &'static str,
    pub(super) finding: Option<ReviewFindingRef>,
    pub(super) ordinal: Option<ReviewEventOrdinal>,
    pub(super) event_kind: Option<&'static str>,
    pub(super) reason: Option<&'a str>,
    pub(super) referenced: Option<ReviewReferencedFindingEvidence>,
    pub(super) external_link: Option<ReviewExternalLinkId>,
    pub(super) external_object: Option<&'a str>,
    pub(super) observation_state: Option<ReviewExternalObjectState>,
}

pub(super) fn pass_state_turn(state: &ReviewPassState) -> Option<TurnId> {
    match state {
        ReviewPassState::Queued | ReviewPassState::Cancelled { turn: None } => None,
        ReviewPassState::Running { turn }
        | ReviewPassState::Succeeded { turn, .. }
        | ReviewPassState::Failed { turn }
        | ReviewPassState::Blocked { turn, .. }
        | ReviewPassState::Cancelled { turn: Some(turn) } => Some(*turn),
    }
}

pub(super) fn pass_state_result(state: &ReviewPassState) -> Option<&ReviewPassResult> {
    match state {
        ReviewPassState::Succeeded { result, .. } | ReviewPassState::Blocked { result, .. } => {
            result.as_ref()
        }
        ReviewPassState::Queued
        | ReviewPassState::Running { .. }
        | ReviewPassState::Failed { .. }
        | ReviewPassState::Cancelled { .. } => None,
    }
}

pub(super) fn encode_pass_state(state: &ReviewPassState) -> EncodedPassState<'_> {
    match state {
        ReviewPassState::Queued => EncodedPassState {
            kind: "queued",
            turn: None,
            frontier: None,
            result: None,
        },
        ReviewPassState::Running { turn } => EncodedPassState {
            kind: "running",
            turn: Some(*turn),
            frontier: None,
            result: None,
        },
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            result,
        } => EncodedPassState {
            kind: "succeeded",
            turn: Some(*turn),
            frontier: Some(*output_frontier),
            result: result.as_ref().map(encode_pass_result),
        },
        ReviewPassState::Failed { turn } => EncodedPassState {
            kind: "failed",
            turn: Some(*turn),
            frontier: None,
            result: None,
        },
        ReviewPassState::Blocked { turn, result } => EncodedPassState {
            kind: "blocked",
            turn: Some(*turn),
            frontier: None,
            result: result.as_ref().map(encode_pass_result),
        },
        ReviewPassState::Cancelled { turn } => EncodedPassState {
            kind: "cancelled",
            turn: *turn,
            frontier: None,
            result: None,
        },
    }
}

fn encode_pass_result(result: &ReviewPassResult) -> EncodedPassResult<'_> {
    let empty = |kind| EncodedPassResult {
        kind,
        finding: None,
        ordinal: None,
        event_kind: None,
        reason: None,
        referenced: None,
        external_link: None,
        external_object: None,
        observation_state: None,
    };
    match result {
        ReviewPassResult::ProducedFindings(_) => empty("produced_findings"),
        ReviewPassResult::FindingEvent(event) => {
            encode_pass_finding_event("finding_event", event, None, None)
        }
        ReviewPassResult::ExternalLinkAttachment(attachment) => match attachment.finding_event() {
            Some(event) => encode_pass_finding_event(
                "external_link_attachment",
                event,
                Some(attachment.link()),
                Some(attachment.external_object().as_str()),
            ),
            None => EncodedPassResult {
                external_link: Some(attachment.link()),
                external_object: Some(attachment.external_object().as_str()),
                ..empty("external_link_attachment")
            },
        },
        ReviewPassResult::ExternalLinkObservation(observation) => EncodedPassResult {
            ordinal: Some(observation.ordinal()),
            external_link: Some(observation.link()),
            observation_state: Some(observation.state()),
            ..empty("external_link_observation")
        },
        ReviewPassResult::ExternalLinkNoChange(no_change) => EncodedPassResult {
            ordinal: Some(no_change.observed_through()),
            external_link: Some(no_change.link()),
            observation_state: Some(no_change.state()),
            ..empty("external_link_no_change")
        },
        ReviewPassResult::ExternalLinkPublicationBlocked(blocked) => EncodedPassResult {
            reason: Some(blocked.reason().as_str()),
            external_link: Some(blocked.link()),
            ..empty("external_link_publication_blocked")
        },
    }
}

fn encode_pass_finding_event<'a>(
    result_kind: &'static str,
    event: &'a ReviewFindingEventResult,
    attachment_link: Option<ReviewExternalLinkId>,
    external_object: Option<&'a str>,
) -> EncodedPassResult<'a> {
    let (event_kind, reason, referenced, event_link) = match event.kind() {
        ReviewFindingEventResultKind::Accepted => ("accepted", None, None, None),
        ReviewFindingEventResultKind::Rejected { reason } => {
            ("rejected", Some(reason.as_str()), None, None)
        }
        ReviewFindingEventResultKind::Duplicate { canonical } => {
            ("duplicate", None, Some(*canonical), None)
        }
        ReviewFindingEventResultKind::Superseded { successor } => {
            ("superseded", None, Some(*successor), None)
        }
        ReviewFindingEventResultKind::Stale => ("stale", None, None, None),
        ReviewFindingEventResultKind::Posted { link } => ("posted", None, None, Some(*link)),
        ReviewFindingEventResultKind::Fixed => ("fixed", None, None, None),
        ReviewFindingEventResultKind::BlockedWithReason { reason, link } => {
            ("blocked_with_reason", Some(reason.as_str()), None, *link)
        }
    };
    EncodedPassResult {
        kind: result_kind,
        finding: Some(event.finding()),
        ordinal: Some(event.ordinal()),
        event_kind: Some(event_kind),
        reason,
        referenced,
        external_link: attachment_link.or(event_link),
        external_object,
        observation_state: None,
    }
}

pub(super) struct StoredPassResult {
    kind: Option<String>,
    target: Uuid,
    finding: Option<Uuid>,
    finding_run: Option<Uuid>,
    finding_pass: Option<Uuid>,
    ordinal: Option<i64>,
    event_kind: Option<String>,
    reason: Option<String>,
    referenced_finding: Option<Uuid>,
    referenced_run: Option<Uuid>,
    referenced_target: Option<Uuid>,
    referenced_pass: Option<Uuid>,
    referenced_status: Option<String>,
    external_link: Option<Uuid>,
    external_object: Option<String>,
    observation_state: Option<String>,
}

pub(super) fn stored_pass_result(
    row: &PgRow,
    target_column: &str,
    prefix: &str,
) -> Result<StoredPassResult, ReviewWorkflowStoreError> {
    let column = |suffix: &str| format!("{prefix}{suffix}");
    let referenced_finding: Option<Uuid> =
        row.try_get(column("result_referenced_finding_id").as_str())?;
    let referenced_target =
        match row.try_get(column("result_referenced_finding_target_id").as_str()) {
            Ok(target) => target,
            Err(sqlx::Error::ColumnNotFound(_)) if referenced_finding.is_none() => None,
            Err(error) => return Err(error.into()),
        };
    Ok(StoredPassResult {
        kind: row.try_get(column("result_kind").as_str())?,
        target: row.try_get(target_column)?,
        finding: row.try_get(column("result_finding_id").as_str())?,
        finding_run: row.try_get(column("result_finding_run_id").as_str())?,
        finding_pass: row.try_get(column("result_finding_pass_id").as_str())?,
        ordinal: row.try_get(column("result_event_ordinal").as_str())?,
        event_kind: row.try_get(column("result_event_kind").as_str())?,
        reason: row.try_get(column("result_reason").as_str())?,
        referenced_finding,
        referenced_run: row.try_get(column("result_referenced_finding_run_id").as_str())?,
        referenced_target,
        referenced_pass: row.try_get(column("result_referenced_finding_pass_id").as_str())?,
        referenced_status: row.try_get(column("result_referenced_finding_status").as_str())?,
        external_link: row.try_get(column("result_external_link_id").as_str())?,
        external_object: row.try_get(column("result_external_object_key").as_str())?,
        observation_state: row.try_get(column("result_observation_state").as_str())?,
    })
}

pub(super) fn decode_pass_state(
    kind: &str,
    turn: Option<Uuid>,
    frontier: Option<Uuid>,
    stored: StoredPassResult,
    produced_findings: Vec<ReviewFindingRef>,
    referenced_producer: Option<&LoadedReviewPass>,
) -> Result<ReviewPassState, ReviewWorkflowStoreError> {
    let result = decode_pass_result(stored, produced_findings, referenced_producer)?;
    match (kind, turn, frontier, result) {
        ("queued", None, None, None) => Ok(ReviewPassState::Queued),
        ("running", Some(turn), None, None) => Ok(ReviewPassState::Running {
            turn: turn_id(turn),
        }),
        ("succeeded", Some(turn), Some(frontier), result) => Ok(ReviewPassState::Succeeded {
            turn: turn_id(turn),
            output_frontier: context_frontier_id(frontier),
            result,
        }),
        ("failed", Some(turn), None, None) => Ok(ReviewPassState::Failed {
            turn: turn_id(turn),
        }),
        ("blocked", Some(turn), None, result) => Ok(ReviewPassState::Blocked {
            turn: turn_id(turn),
            result,
        }),
        ("cancelled", turn, None, None) => Ok(ReviewPassState::Cancelled {
            turn: turn.map(turn_id),
        }),
        _ => Err(corruption(
            "review_pass",
            format!("invalid state shape {kind}"),
        )),
    }
}

fn decode_pass_result(
    stored: StoredPassResult,
    produced_findings: Vec<ReviewFindingRef>,
    referenced_producer: Option<&LoadedReviewPass>,
) -> Result<Option<ReviewPassResult>, ReviewWorkflowStoreError> {
    let scalar_empty = stored.finding.is_none()
        && stored.finding_run.is_none()
        && stored.finding_pass.is_none()
        && stored.ordinal.is_none()
        && stored.event_kind.is_none()
        && stored.reason.is_none()
        && stored.referenced_finding.is_none()
        && stored.referenced_run.is_none()
        && stored.referenced_target.is_none()
        && stored.referenced_pass.is_none()
        && stored.referenced_status.is_none()
        && stored.external_link.is_none()
        && stored.external_object.is_none()
        && stored.observation_state.is_none();
    match stored.kind.as_deref() {
        None if scalar_empty && produced_findings.is_empty() => Ok(None),
        Some("produced_findings") if scalar_empty => Ok(Some(ReviewPassResult::ProducedFindings(
            ReviewProducedFindings::try_new(produced_findings).map_err(|error| {
                corruption(
                    "review_pass",
                    format!("invalid finding inventory: {error:?}"),
                )
            })?,
        ))),
        Some("finding_event")
            if stored.external_object.is_none() && stored.observation_state.is_none() =>
        {
            Ok(Some(ReviewPassResult::FindingEvent(
                decode_stored_finding_event(&stored, referenced_producer)?,
            )))
        }
        Some("external_link_attachment") if stored.observation_state.is_none() => {
            let link = stored.external_link.map(external_link_id).ok_or_else(|| {
                corruption(
                    "review_pass",
                    String::from("attachment result omitted link"),
                )
            })?;
            let external_object = review_key(
                stored.external_object.clone().ok_or_else(|| {
                    corruption(
                        "review_pass",
                        String::from("attachment result omitted object key"),
                    )
                })?,
                "review_pass",
            )?;
            let finding_event = if stored.finding.is_some() {
                Some(decode_stored_finding_event(&stored, referenced_producer)?)
            } else {
                None
            };
            Ok(Some(ReviewPassResult::ExternalLinkAttachment(
                ReviewExternalLinkAttachmentResult::new(link, external_object, finding_event),
            )))
        }
        Some("external_link_observation")
            if stored.finding.is_none()
                && stored.event_kind.is_none()
                && stored.reason.is_none()
                && stored.referenced_finding.is_none()
                && stored.external_object.is_none() =>
        {
            let link = stored.external_link.map(external_link_id).ok_or_else(|| {
                corruption(
                    "review_pass",
                    String::from("observation result omitted link"),
                )
            })?;
            let ordinal = stored.ordinal.ok_or_else(|| {
                corruption(
                    "review_pass",
                    String::from("observation result omitted ordinal"),
                )
            })?;
            let state = stored.observation_state.as_deref().ok_or_else(|| {
                corruption(
                    "review_pass",
                    String::from("observation result omitted state"),
                )
            })?;
            Ok(Some(ReviewPassResult::ExternalLinkObservation(
                ReviewExternalLinkObservationResult::new(
                    link,
                    ReviewEventOrdinal::try_new(positive_u32(ordinal, "review_pass")?).map_err(
                        |_| corruption("review_pass", String::from("zero observation ordinal")),
                    )?,
                    decode_external_object_state(state)?,
                ),
            )))
        }
        Some("external_link_no_change")
            if stored.finding.is_none()
                && stored.ordinal.is_some()
                && stored.event_kind.is_none()
                && stored.reason.is_none()
                && stored.referenced_finding.is_none()
                && stored.external_object.is_none() =>
        {
            let link = stored.external_link.map(external_link_id).ok_or_else(|| {
                corruption("review_pass", String::from("unchanged result omitted link"))
            })?;
            let state = stored.observation_state.as_deref().ok_or_else(|| {
                corruption(
                    "review_pass",
                    String::from("unchanged result omitted state"),
                )
            })?;
            let ordinal = stored.ordinal.ok_or_else(|| {
                corruption(
                    "review_pass",
                    String::from("unchanged result omitted observation frontier"),
                )
            })?;
            Ok(Some(ReviewPassResult::ExternalLinkNoChange(
                ReviewExternalLinkNoChangeResult::new(
                    link,
                    ReviewEventOrdinal::try_new(positive_u32(ordinal, "review_pass")?).map_err(
                        |_| corruption("review_pass", String::from("zero observation frontier")),
                    )?,
                    decode_external_object_state(state)?,
                ),
            )))
        }
        Some("external_link_publication_blocked")
            if stored.finding.is_none()
                && stored.ordinal.is_none()
                && stored.event_kind.is_none()
                && stored.referenced_finding.is_none()
                && stored.external_object.is_none()
                && stored.observation_state.is_none() =>
        {
            let link = stored.external_link.map(external_link_id).ok_or_else(|| {
                corruption(
                    "review_pass",
                    String::from("publication block result omitted link"),
                )
            })?;
            let reason = review_text(
                stored.reason.ok_or_else(|| {
                    corruption(
                        "review_pass",
                        String::from("publication block result omitted reason"),
                    )
                })?,
                "review_pass",
            )?;
            Ok(Some(ReviewPassResult::ExternalLinkPublicationBlocked(
                ReviewExternalLinkPublicationBlockedResult::new(link, reason),
            )))
        }
        _ => Err(corruption(
            "review_pass",
            String::from("torn or unknown pass result"),
        )),
    }
}

fn decode_stored_finding_event(
    stored: &StoredPassResult,
    referenced_producer: Option<&LoadedReviewPass>,
) -> Result<ReviewFindingEventResult, ReviewWorkflowStoreError> {
    let finding = ReviewFindingRef::new(
        ReviewPassRef::new(
            ReviewRunRef::new(
                target_id(stored.target),
                run_id(stored.finding_run.ok_or_else(|| {
                    corruption(
                        "review_pass",
                        String::from("event result omitted finding run"),
                    )
                })?),
            ),
            pass_id(stored.finding_pass.ok_or_else(|| {
                corruption(
                    "review_pass",
                    String::from("event result omitted finding pass"),
                )
            })?),
        ),
        finding_id(stored.finding.ok_or_else(|| {
            corruption("review_pass", String::from("event result omitted finding"))
        })?),
    );
    let ordinal = ReviewEventOrdinal::try_new(positive_u32(
        stored.ordinal.ok_or_else(|| {
            corruption("review_pass", String::from("event result omitted ordinal"))
        })?,
        "review_pass",
    )?)
    .map_err(|_| corruption("review_pass", String::from("zero event ordinal")))?;
    let event_kind = stored.event_kind.as_deref().ok_or_else(|| {
        corruption(
            "review_pass",
            String::from("event result omitted discriminator"),
        )
    })?;
    let kind = match event_kind {
        "accepted" => ReviewFindingEventResultKind::Accepted,
        "rejected" => ReviewFindingEventResultKind::Rejected {
            reason: review_text(
                stored.reason.clone().ok_or_else(|| {
                    corruption(
                        "review_pass",
                        String::from("rejected result omitted reason"),
                    )
                })?,
                "review_pass",
            )?,
        },
        "duplicate" => ReviewFindingEventResultKind::Duplicate {
            canonical: decode_stored_referenced_finding(stored, referenced_producer)?,
        },
        "superseded" => ReviewFindingEventResultKind::Superseded {
            successor: decode_stored_referenced_finding(stored, referenced_producer)?,
        },
        "stale" => ReviewFindingEventResultKind::Stale,
        "posted" => ReviewFindingEventResultKind::Posted {
            link: stored.external_link.map(external_link_id).ok_or_else(|| {
                corruption("review_pass", String::from("posted result omitted link"))
            })?,
        },
        "fixed" => ReviewFindingEventResultKind::Fixed,
        "blocked_with_reason" => ReviewFindingEventResultKind::BlockedWithReason {
            reason: review_text(
                stored.reason.clone().ok_or_else(|| {
                    corruption("review_pass", String::from("blocked result omitted reason"))
                })?,
                "review_pass",
            )?,
            link: stored.external_link.map(external_link_id),
        },
        other => {
            return Err(corruption(
                "review_pass",
                format!("unknown finding-event result {other}"),
            ));
        }
    };
    Ok(ReviewFindingEventResult::new(finding, ordinal, kind))
}

fn decode_stored_referenced_finding(
    stored: &StoredPassResult,
    referenced_producer: Option<&LoadedReviewPass>,
) -> Result<ReviewReferencedFindingEvidence, ReviewWorkflowStoreError> {
    let reference = ReviewFindingRef::new(
        ReviewPassRef::new(
            ReviewRunRef::new(
                target_id(stored.referenced_target.ok_or_else(|| {
                    corruption(
                        "review_pass",
                        String::from("referenced result omitted target"),
                    )
                })?),
                run_id(stored.referenced_run.ok_or_else(|| {
                    corruption("review_pass", String::from("referenced result omitted run"))
                })?),
            ),
            pass_id(stored.referenced_pass.ok_or_else(|| {
                corruption(
                    "review_pass",
                    String::from("referenced result omitted producing pass"),
                )
            })?),
        ),
        finding_id(stored.referenced_finding.ok_or_else(|| {
            corruption(
                "review_pass",
                String::from("referenced result omitted finding"),
            )
        })?),
    );
    let status = decode_finding_status(stored.referenced_status.as_deref().ok_or_else(|| {
        corruption(
            "review_pass",
            String::from("referenced result omitted status"),
        )
    })?)?;
    let producer = referenced_producer.ok_or_else(|| {
        corruption(
            "review_pass",
            String::from("referenced result producer is missing"),
        )
    })?;
    let producer_pass = producer.evidence();
    ReviewReferencedFindingEvidence::try_reconstitute(
        reference,
        status,
        &producer_pass,
        producer.run_evidence(),
    )
    .ok_or_else(|| {
        corruption(
            "review_pass",
            String::from("referenced result producer or status is invalid"),
        )
    })
}
