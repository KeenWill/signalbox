use super::decode::{decode_pass_turn_evidence, decode_target_row, reconstitute_pass};
use super::{
    ReviewWorkflowStoreError, corruption, decode_review_policy, decode_run_state,
    decode_workflow_kind, finding_id, pass_id, positive_u32, require_joined_reference, run_id,
    target_id,
};
use signalbox_domain::{
    ReviewExternalLinkClaim, ReviewExternalLinkId, ReviewFindingRef, ReviewPass,
    ReviewPassEvidence, ReviewPassId, ReviewPassRef, ReviewPassTurnEvidence, ReviewPolicy,
    ReviewRun, ReviewRunEvidence, ReviewRunReconstitutionInput, ReviewRunRef, ReviewTarget,
    ReviewTargetId,
};
use sqlx::postgres::PgRow;
use sqlx::types::Uuid;
use sqlx::{PgConnection, Row};

pub(super) async fn load_target_on_connection(
    connection: &mut PgConnection,
    target: ReviewTargetId,
) -> Result<Option<ReviewTarget>, ReviewWorkflowStoreError> {
    let rows = sqlx::query(
        "WITH RECURSIVE ancestry AS (
             SELECT target.target_id, target.provider_key,
                    target.repository_key, target.subject_kind,
                    target.change_request_number, target.head_revision,
                    target.base_revision, target.stack_parent_target_id,
                    ARRAY[target.target_id] AS path, 0 AS depth
               FROM review_target AS target
              WHERE target.target_id = $1
             UNION ALL
             SELECT parent.target_id, parent.provider_key,
                    parent.repository_key, parent.subject_kind,
                    parent.change_request_number, parent.head_revision,
                    parent.base_revision, parent.stack_parent_target_id,
                    ancestry.path || parent.target_id,
                    ancestry.depth + 1
               FROM ancestry
               JOIN review_target AS parent
                 ON parent.target_id = ancestry.stack_parent_target_id
              WHERE NOT parent.target_id = ANY(ancestry.path)
         )
         SELECT target_id, provider_key, repository_key, subject_kind,
                change_request_number, head_revision, base_revision,
                stack_parent_target_id
           FROM ancestry
          ORDER BY depth DESC",
    )
    .bind(target.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    if rows[0]
        .try_get::<Option<Uuid>, _>("stack_parent_target_id")?
        .is_some()
    {
        return Err(corruption(
            "review_target",
            String::from("stack ancestry is cyclic or has a missing parent"),
        ));
    }
    let mut reconstructed = None;
    for row in rows {
        reconstructed = Some(decode_target_row(&row, reconstructed.as_ref())?);
    }
    Ok(reconstructed)
}

#[derive(Clone, Debug)]
pub(crate) struct LoadedReviewPass {
    pub(crate) pass: ReviewPass,
    pub(super) policy: ReviewPolicy,
    pub(super) turn_evidence: Option<ReviewPassTurnEvidence>,
    pub(super) run: ReviewRunEvidence,
}

impl LoadedReviewPass {
    pub(super) fn evidence(&self) -> ReviewPassEvidence {
        ReviewPassEvidence::from_pass(&self.pass, self.policy)
    }

    pub(super) const fn run_evidence(&self) -> ReviewRunEvidence {
        self.run
    }
}

pub(crate) async fn load_pass_on_connection(
    connection: &mut PgConnection,
    pass: ReviewPassId,
) -> Result<Option<LoadedReviewPass>, ReviewWorkflowStoreError> {
    let row = sqlx::query(
        "SELECT workflow_pass.pass_id, workflow_pass.run_id,
                workflow_pass.target_id, workflow_pass.pass_kind,
                canonical_run.workflow_kind AS run_workflow_kind,
                canonical_run.policy_version AS run_policy_version,
                canonical_run.minimum_judge_confidence
                    AS run_minimum_judge_confidence,
                canonical_run.minimum_publication_confidence
                    AS run_minimum_publication_confidence,
                canonical_run.state_kind AS run_state_kind,
                canonical_run.state_pass_id AS run_state_pass_id,
                workflow_pass.session_id AS pass_session_id,
                workflow_pass.accepted_input_id, workflow_pass.state_kind,
                workflow_pass.origin_turn_id,
                workflow_pass.turn_id, workflow_pass.output_frontier_id,
                workflow_pass.result_kind,
                workflow_pass.result_finding_id,
                workflow_pass.result_finding_run_id,
                workflow_pass.result_finding_pass_id,
                workflow_pass.result_event_ordinal,
                workflow_pass.result_event_kind,
                workflow_pass.result_reason,
                workflow_pass.result_judge_confidence,
                workflow_pass.result_referenced_finding_id,
                workflow_pass.result_referenced_finding_run_id,
                workflow_pass.result_referenced_finding_target_id,
                workflow_pass.result_referenced_finding_pass_id,
                workflow_pass.result_referenced_finding_status,
                workflow_pass.result_external_link_id,
                workflow_pass.result_external_object_key,
                workflow_pass.result_observation_state,
                canonical_input.session_id AS accepted_input_session_id,
                canonical_input.origin_turn_id AS accepted_input_origin_turn_id,
                canonical_origin_turn.turn_id AS canonical_origin_turn_id,
                canonical_turn.turn_id AS evidence_turn_id,
                canonical_turn.session_id AS turn_session_id,
                canonical_turn.origin_accepted_input_id
                    AS turn_accepted_input_id,
                canonical_turn.state_kind AS turn_state_kind,
                canonical_turn.terminal_disposition_kind
                    AS turn_terminal_disposition_kind,
                canonical_turn.terminal_frontier_id
                    AS turn_terminal_frontier_id,
                canonical_run.run_id AS canonical_run_id,
                canonical_run.target_id AS canonical_run_target_id,
                canonical_target.target_id AS canonical_target_id
           FROM review_pass AS workflow_pass
           LEFT JOIN review_run AS canonical_run
             ON canonical_run.run_id = workflow_pass.run_id
            AND canonical_run.target_id = workflow_pass.target_id
           LEFT JOIN review_target AS canonical_target
             ON canonical_target.target_id = workflow_pass.target_id
           LEFT JOIN accepted_input AS canonical_input
             ON canonical_input.accepted_input_id =
                workflow_pass.accepted_input_id
           LEFT JOIN turn_lifecycle AS canonical_origin_turn
             ON canonical_origin_turn.turn_id =
                workflow_pass.origin_turn_id
            AND canonical_origin_turn.session_id =
                workflow_pass.session_id
            AND canonical_origin_turn.origin_accepted_input_id =
                workflow_pass.accepted_input_id
           LEFT JOIN turn_lifecycle AS canonical_turn
             ON canonical_turn.turn_id = workflow_pass.turn_id
          WHERE workflow_pass.pass_id = $1",
    )
    .bind(pass.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    require_joined_reference(
        &row,
        "canonical_run_id",
        "review_pass",
        "referenced run row is missing",
    )?;
    require_joined_reference(
        &row,
        "canonical_target_id",
        "review_pass",
        "referenced target row is missing",
    )?;
    let produced_findings = load_produced_findings_on_connection(connection, pass).await?;
    let referenced_producer =
        load_referenced_producer_on_connection(connection, &row, pass).await?;
    let turn_evidence = decode_pass_turn_evidence(&row)?;
    let pass = reconstitute_pass(
        &row,
        turn_evidence,
        produced_findings,
        referenced_producer.as_ref(),
    )?;
    require_joined_reference(
        &row,
        "canonical_origin_turn_id",
        "review_pass",
        "origin turn row is missing",
    )?;
    let policy = decode_review_policy(
        &row,
        "run_policy_version",
        "run_minimum_judge_confidence",
        "run_minimum_publication_confidence",
        "review_pass",
    )?;
    let run_reference = pass.reference().run();
    let workflow = decode_workflow_kind(&row.try_get::<String, _>("run_workflow_kind")?)?;
    let state = decode_run_state(
        run_reference,
        &row.try_get::<String, _>("run_state_kind")?,
        row.try_get("run_state_pass_id")?,
    );
    let run = ReviewRun::try_reconstitute(ReviewRunReconstitutionInput::new(
        run_reference,
        workflow,
        policy,
        state?,
        Some(ReviewPassEvidence::from_pass(&pass, policy)),
    ))
    .map_err(|error| {
        corruption(
            "review_run",
            format!(
                "canonical run contradicts its pass projection: {:?}",
                error.failure()
            ),
        )
    })?
    .evidence();
    Ok(Some(LoadedReviewPass {
        pass,
        policy,
        turn_evidence,
        run,
    }))
}

async fn load_referenced_producer_on_connection(
    connection: &mut PgConnection,
    row: &PgRow,
    loading_pass: ReviewPassId,
) -> Result<Option<LoadedReviewPass>, ReviewWorkflowStoreError> {
    let finding: Option<Uuid> = row.try_get("result_referenced_finding_id")?;
    let run: Option<Uuid> = row.try_get("result_referenced_finding_run_id")?;
    let target: Option<Uuid> = row.try_get("result_referenced_finding_target_id")?;
    let pass: Option<Uuid> = row.try_get("result_referenced_finding_pass_id")?;
    let (finding, run, target, pass) = match (finding, run, target, pass) {
        (None, None, None, None) => return Ok(None),
        (Some(finding), Some(run), Some(target), Some(pass)) => (finding, run, target, pass),
        _ => {
            return Err(corruption(
                "review_pass",
                String::from("referenced result ancestry is incomplete"),
            ));
        }
    };
    if pass == loading_pass.into_uuid() {
        return Err(corruption(
            "review_pass",
            String::from("referenced result names its own pass as producer"),
        ));
    }
    let canonical = sqlx::query(
        "SELECT finding.finding_id AS canonical_finding_id,
                producer.pass_id AS canonical_pass_id,
                producer.pass_kind, producer.state_kind,
                producer.result_kind
           FROM review_finding AS finding
           LEFT JOIN review_pass AS producer
             ON producer.pass_id = finding.producing_pass_id
            AND producer.run_id = finding.run_id
            AND producer.target_id = finding.target_id
          WHERE finding.finding_id = $1
            AND finding.run_id = $2
            AND finding.target_id = $3
            AND finding.producing_pass_id = $4",
    )
    .bind(finding)
    .bind(run)
    .bind(target)
    .bind(pass)
    .fetch_optional(&mut *connection)
    .await?
    .ok_or_else(|| {
        corruption(
            "review_pass",
            String::from("referenced result finding ancestry is missing"),
        )
    })?;
    require_joined_reference(
        &canonical,
        "canonical_pass_id",
        "review_pass",
        "referenced result producer is missing",
    )?;
    if canonical.try_get::<String, _>("pass_kind")? != "read_only_review"
        || canonical.try_get::<String, _>("state_kind")? != "succeeded"
        || canonical
            .try_get::<Option<String>, _>("result_kind")?
            .as_deref()
            != Some("produced_findings")
    {
        return Err(corruption(
            "review_pass",
            String::from("referenced result producer is not sealed read-only success"),
        ));
    }
    let loaded = Box::pin(load_pass_on_connection(connection, pass_id(pass)))
        .await?
        .ok_or_else(|| {
            corruption(
                "review_pass",
                String::from("referenced result producer disappeared"),
            )
        })?;
    let expected = ReviewPassRef::new(
        ReviewRunRef::new(target_id(target), run_id(run)),
        pass_id(pass),
    );
    if loaded.pass.reference() != expected {
        return Err(corruption(
            "review_pass",
            String::from("referenced result producer ancestry is cross-wired"),
        ));
    }
    Ok(Some(loaded))
}

pub(super) async fn load_external_link_claims_on_connection(
    connection: &mut PgConnection,
    link: ReviewExternalLinkId,
) -> Result<Vec<ReviewExternalLinkClaim>, ReviewWorkflowStoreError> {
    let rows = sqlx::query(
        "SELECT pass_id
           FROM review_pass
          WHERE result_external_link_id = $1
            AND (
                result_kind IN (
                    'external_link_no_change',
                    'external_link_publication_blocked'
                )
                OR (
                    result_kind = 'finding_event'
                    AND result_event_kind = 'blocked_with_reason'
                )
            )
          ORDER BY pass_id",
    )
    .bind(link.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    let mut claims = Vec::with_capacity(rows.len());
    for row in rows {
        let pass = load_pass_on_connection(connection, pass_id(row.try_get("pass_id")?))
            .await?
            .ok_or_else(|| {
                corruption(
                    "review_external_link",
                    String::from("claiming pass row is missing"),
                )
            })?;
        claims.push(ReviewExternalLinkClaim::new(
            pass.evidence(),
            pass.run_evidence(),
        ));
    }
    Ok(claims)
}

async fn load_produced_findings_on_connection(
    connection: &mut PgConnection,
    pass: ReviewPassId,
) -> Result<Vec<ReviewFindingRef>, ReviewWorkflowStoreError> {
    let pass_row = sqlx::query(
        "SELECT pass.result_kind, seal.finding_count
           FROM review_pass AS pass
           LEFT JOIN review_pass_finding_inventory_seal AS seal
             ON seal.pass_id = pass.pass_id
          WHERE pass.pass_id = $1",
    )
    .bind(pass.into_uuid())
    .fetch_optional(&mut *connection)
    .await?
    .ok_or_else(|| {
        corruption(
            "review_pass",
            String::from("finding inventory pass row is missing"),
        )
    })?;
    let result_kind: Option<String> = pass_row.try_get("result_kind")?;
    let sealed_count: Option<i32> = pass_row.try_get("finding_count")?;

    let member_rows = sqlx::query(
        "SELECT member.result_ordinal, member.target_id, member.finding_run_id,
                member.finding_pass_id, member.finding_id,
                finding.finding_id AS canonical_finding_id
           FROM review_pass_produced_finding AS member
           LEFT JOIN review_finding AS finding
             ON finding.finding_id = member.finding_id
            AND finding.run_id = member.finding_run_id
            AND finding.target_id = member.target_id
            AND finding.producing_pass_id = member.finding_pass_id
          WHERE member.pass_id = $1
          ORDER BY member.result_ordinal",
    )
    .bind(pass.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    let mut members = Vec::with_capacity(member_rows.len());
    for (index, row) in member_rows.into_iter().enumerate() {
        let ordinal = positive_u32(
            row.try_get("result_ordinal")?,
            "review_pass_produced_finding",
        )?;
        let expected = u32::try_from(index + 1).map_err(|_| {
            corruption(
                "review_pass_produced_finding",
                String::from("finding inventory ordinal overflow"),
            )
        })?;
        if ordinal != expected {
            return Err(corruption(
                "review_pass_produced_finding",
                String::from("finding inventory ordinals are not contiguous"),
            ));
        }
        require_joined_reference(
            &row,
            "canonical_finding_id",
            "review_pass_produced_finding",
            "inventory member has no canonical finding",
        )?;
        members.push(ReviewFindingRef::new(
            ReviewPassRef::new(
                ReviewRunRef::new(
                    target_id(row.try_get("target_id")?),
                    run_id(row.try_get("finding_run_id")?),
                ),
                pass_id(row.try_get("finding_pass_id")?),
            ),
            finding_id(row.try_get("finding_id")?),
        ));
    }
    let member_rows = members;
    let canonical = sqlx::query(
        "SELECT target_id, run_id, producing_pass_id, finding_id
           FROM review_finding
          WHERE producing_pass_id = $1
          ORDER BY target_id, run_id, producing_pass_id, finding_id",
    )
    .bind(pass.into_uuid())
    .fetch_all(&mut *connection)
    .await?
    .into_iter()
    .map(|row| {
        Ok(ReviewFindingRef::new(
            ReviewPassRef::new(
                ReviewRunRef::new(
                    target_id(row.try_get("target_id")?),
                    run_id(row.try_get("run_id")?),
                ),
                pass_id(row.try_get("producing_pass_id")?),
            ),
            finding_id(row.try_get("finding_id")?),
        ))
    })
    .collect::<Result<Vec<_>, ReviewWorkflowStoreError>>()?;
    let count = i32::try_from(member_rows.len()).map_err(|_| {
        corruption(
            "review_pass_produced_finding",
            String::from("finding inventory count overflow"),
        )
    })?;
    if member_rows != canonical
        || match result_kind.as_deref() {
            Some("produced_findings") => sealed_count != Some(count),
            _ => sealed_count.is_some() || count != 0,
        }
    {
        return Err(corruption(
            "review_pass_produced_finding",
            String::from("inventory differs from canonical sealed findings"),
        ));
    }
    Ok(member_rows)
}
