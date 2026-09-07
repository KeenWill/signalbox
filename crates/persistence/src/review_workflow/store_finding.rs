use super::decode::{decode_finding_event, decode_finding_proposal};
use super::load::{load_pass_on_connection, load_target_on_connection};
use super::pass_codec::pass_state_result;
use super::write::{bind_pass_result, insert_finding_event, insert_finding_row};
use super::{
    ReviewWorkflowInsertionError, ReviewWorkflowStore, ReviewWorkflowStoreError,
    ReviewWorkflowTransitionError, begin_repeatable_read, commit_mutation, corruption,
    encode_finding_status, external_link_id, finding_id, pass_id, require_joined_reference,
    target_id,
};
use signalbox_domain::{
    ReviewFinding, ReviewFindingEvent, ReviewFindingEventKind, ReviewFindingId,
    ReviewFindingStatus, ReviewPassEvidence, ReviewPassResult, ReviewRunId,
    validate_complete_review_finding_reference_graph,
};
use sqlx::types::Uuid;
use sqlx::{PgConnection, Row};
use std::collections::{BTreeMap, BTreeSet};

impl ReviewWorkflowStore {
    /// Inserts immutable finding content in its initial open state.
    pub async fn insert_finding(
        &self,
        finding: &ReviewFinding,
    ) -> Result<(), ReviewWorkflowStoreError> {
        if finding.status() != ReviewFindingStatus::Open {
            return Err(ReviewWorkflowStoreError::InvalidInsertion(
                ReviewWorkflowInsertionError::FindingNotOpen {
                    status: finding.status(),
                },
            ));
        }
        self.insert_findings(
            finding.proposal().producing_pass(),
            std::slice::from_ref(finding),
        )
        .await
    }

    /// Atomically binds one produced-finding result and its complete inventory.
    pub async fn insert_findings(
        &self,
        pass: &ReviewPassEvidence,
        findings: &[ReviewFinding],
    ) -> Result<(), ReviewWorkflowStoreError> {
        let Some(ReviewPassResult::ProducedFindings(inventory)) = pass_state_result(pass.state())
        else {
            return Err(ReviewWorkflowStoreError::IncompleteFindingInventory);
        };
        let mut supplied = findings
            .iter()
            .map(|finding| finding.proposal().reference())
            .collect::<Vec<_>>();
        supplied.sort_unstable();
        if supplied != inventory.findings()
            || findings.iter().any(|finding| {
                finding.status() != ReviewFindingStatus::Open
                    || finding.proposal().producing_pass() != pass
            })
        {
            return Err(ReviewWorkflowStoreError::IncompleteFindingInventory);
        }
        let mut transaction = self.pool.begin().await?;
        bind_pass_result(&mut transaction, pass).await?;
        for finding in findings {
            insert_finding_row(&mut transaction, finding).await?;
        }
        for (index, reference) in inventory.findings().iter().enumerate() {
            sqlx::query(
                "INSERT INTO review_pass_produced_finding
                    (pass_id, result_ordinal, finding_id, finding_run_id,
                     target_id, finding_pass_id)
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(reference.pass().pass().into_uuid())
            .bind(i64::try_from(index + 1).map_err(|_| {
                corruption(
                    "review_pass_produced_finding",
                    String::from("finding inventory ordinal overflow"),
                )
            })?)
            .bind(reference.finding().into_uuid())
            .bind(reference.run().run().into_uuid())
            .bind(reference.target().into_uuid())
            .bind(reference.pass().pass().into_uuid())
            .execute(&mut *transaction)
            .await?;
        }
        sqlx::query(
            "INSERT INTO review_pass_finding_inventory_seal
                (pass_id, finding_count)
             VALUES ($1, $2)",
        )
        .bind(pass.reference().pass().into_uuid())
        .bind(i32::try_from(inventory.findings().len()).map_err(|_| {
            corruption(
                "review_pass_finding_inventory_seal",
                String::from("finding inventory count overflow"),
            )
        })?)
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        Ok(())
    }

    /// Appends one event after domain validation of the complete current history.
    pub async fn append_finding_event(
        &self,
        finding: ReviewFindingId,
        event: ReviewFindingEvent,
    ) -> Result<Option<ReviewFinding>, ReviewWorkflowStoreError> {
        let publication_link = match event.kind() {
            ReviewFindingEventKind::BlockedWithReason {
                link: Some(pending),
                ..
            } => Some(pending.link()),
            _ => None,
        };
        let mut transaction = self.pool.begin().await?;
        if let Some(link) = publication_link {
            // Publication reconciliation locks the reservation before any
            // finding, matching the attachment-path lock order.
            sqlx::query(crate::lock_inventory::REVIEW_EXTERNAL_LINK_TRANSITION)
                .bind(link.into_uuid())
                .fetch_one(&mut *transaction)
                .await?;
            sqlx::query(crate::lock_inventory::REVIEW_FINDINGS_TRANSITION)
                .bind(vec![finding.into_uuid()])
                .fetch_all(&mut *transaction)
                .await?;
        } else {
            // Every ordinary event writer locks the complete target inventory
            // in identity order. Under read committed, a waiter then loads the
            // graph from snapshots taken after the winning event commits; the
            // held inventory keeps later event writes from changing that graph
            // across the loader statements.
            sqlx::query(crate::lock_inventory::REVIEW_TARGET_FINDINGS_TRANSITION)
                .bind(finding.into_uuid())
                .fetch_all(&mut *transaction)
                .await?;
        }
        let current = if publication_link.is_some() {
            // The held reservation and finding locks make the subject projection
            // stable while preserving attachment commits observed after waiting.
            self.load_finding_projection_on_connection(&mut transaction, finding)
                .await?
        } else {
            self.load_finding_on_connection(&mut transaction, finding)
                .await?
        };
        let Some(current) = current else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let next = current.apply(event.clone()).map_err(|error| {
            ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::Finding(
                error,
            ))
        })?;
        insert_finding_event(&mut transaction, &event).await?;
        commit_mutation(transaction).await?;
        Ok(Some(next))
    }

    /// Loads and validates immutable finding content plus complete event history.
    pub async fn load_finding(
        &self,
        finding: ReviewFindingId,
    ) -> Result<Option<ReviewFinding>, ReviewWorkflowStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let finding = self
            .load_finding_on_connection(&mut transaction, finding)
            .await?;
        transaction.commit().await?;
        Ok(finding)
    }

    /// Loads and validates several findings, reading each target graph once.
    ///
    /// [`Self::load_finding`] answers for one identity by reconstructing the
    /// entire target graph that identity belongs to and then selecting a single
    /// member from it. A caller holding a list of identities therefore pays for
    /// that whole reconstruction once per member, which is quadratic in the
    /// number of findings on a target. This performs the same reconstruction
    /// once per distinct target and selects every requested member from it.
    ///
    /// Identities with no `review_finding` row are absent from the result, so a
    /// caller distinguishes them exactly as it would from [`Self::load_finding`]
    /// returning `None`.
    pub async fn load_findings(
        &self,
        findings: &[ReviewFindingId],
    ) -> Result<BTreeMap<ReviewFindingId, ReviewFinding>, ReviewWorkflowStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let loaded = self
            .load_findings_on_connection(&mut transaction, findings)
            .await?;
        transaction.commit().await?;
        Ok(loaded)
    }

    pub(crate) async fn load_findings_on_connection(
        &self,
        connection: &mut PgConnection,
        findings: &[ReviewFindingId],
    ) -> Result<BTreeMap<ReviewFindingId, ReviewFinding>, ReviewWorkflowStoreError> {
        let requested: BTreeSet<ReviewFindingId> = findings.iter().copied().collect();
        if requested.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows = sqlx::query(
            "SELECT finding_id, target_id
               FROM review_finding
              WHERE finding_id = ANY($1)",
        )
        .bind(
            requested
                .iter()
                .map(|finding| finding.into_uuid())
                .collect::<Vec<_>>(),
        )
        .fetch_all(&mut *connection)
        .await?;
        let mut resolved = BTreeSet::new();
        let mut targets = BTreeSet::new();
        for row in rows {
            resolved.insert(finding_id(row.try_get("finding_id")?));
            targets.insert(row.try_get::<Uuid, _>("target_id")?);
        }
        let mut loaded = BTreeMap::new();
        for target in targets {
            for finding in self
                .load_target_findings_on_connection(connection, target)
                .await?
            {
                let identity = finding.proposal().reference().finding();
                if requested.contains(&identity) {
                    loaded.insert(identity, finding);
                }
            }
        }
        // A resolved identity names a row whose target graph was just loaded
        // under one snapshot, so its absence is the same corruption the
        // single-identity loader reports rather than a missing finding.
        if resolved.iter().any(|finding| !loaded.contains_key(finding)) {
            return Err(corruption(
                "review_finding",
                String::from("requested target graph finding disappeared"),
            ));
        }
        Ok(loaded)
    }

    /// Lists and validates complete findings for one run in identity order.
    pub async fn list_findings(
        &self,
        run: ReviewRunId,
    ) -> Result<Vec<ReviewFinding>, ReviewWorkflowStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let target = sqlx::query_scalar::<_, Uuid>(
            "SELECT target_id
               FROM review_finding
              WHERE run_id = $1
              ORDER BY finding_id
              LIMIT 1",
        )
        .bind(run.into_uuid())
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(target) = target else {
            transaction.commit().await?;
            return Ok(Vec::new());
        };
        let findings = self
            .load_target_findings_on_connection(&mut transaction, target)
            .await?
            .into_iter()
            .filter(|finding| finding.proposal().reference().run().run() == run)
            .collect();
        transaction.commit().await?;
        Ok(findings)
    }

    async fn load_finding_on_connection(
        &self,
        connection: &mut PgConnection,
        finding: ReviewFindingId,
    ) -> Result<Option<ReviewFinding>, ReviewWorkflowStoreError> {
        let target = sqlx::query_scalar::<_, Uuid>(
            "SELECT target_id
               FROM review_finding
              WHERE finding_id = $1",
        )
        .bind(finding.into_uuid())
        .fetch_optional(&mut *connection)
        .await?;
        let Some(target) = target else {
            return Ok(None);
        };
        self.load_target_findings_on_connection(connection, target)
            .await?
            .into_iter()
            .find(|candidate| candidate.proposal().reference().finding() == finding)
            .map(Some)
            .ok_or_else(|| {
                corruption(
                    "review_finding",
                    String::from("requested target graph finding disappeared"),
                )
            })
    }

    async fn load_target_findings_on_connection(
        &self,
        connection: &mut PgConnection,
        target: Uuid,
    ) -> Result<Vec<ReviewFinding>, ReviewWorkflowStoreError> {
        let identifiers = sqlx::query_scalar::<_, Uuid>(
            "SELECT finding_id
               FROM review_finding
              WHERE target_id = $1
              ORDER BY finding_id",
        )
        .bind(target)
        .fetch_all(&mut *connection)
        .await?;
        let mut findings = Vec::with_capacity(identifiers.len());
        for identifier in identifiers {
            findings.push(
                self.load_finding_projection_on_connection(connection, finding_id(identifier))
                    .await?
                    .ok_or_else(|| {
                        corruption(
                            "review_finding",
                            String::from("target graph finding disappeared"),
                        )
                    })?,
            );
        }
        validate_complete_review_finding_reference_graph(&findings).map_err(|error| {
            corruption(
                "review_finding",
                format!("reference graph is corrupt: {error:?}"),
            )
        })?;
        Ok(findings)
    }

    pub(super) async fn load_finding_projection_on_connection(
        &self,
        connection: &mut PgConnection,
        finding: ReviewFindingId,
    ) -> Result<Option<ReviewFinding>, ReviewWorkflowStoreError> {
        let row = sqlx::query(
            "SELECT finding.finding_id, finding.run_id, finding.target_id,
                    finding.producing_pass_id, finding.file_path,
                    finding.line_start, finding.line_end, finding.diff_side,
                    finding.title, finding.body, finding.severity,
                    finding.is_real_confidence,
                    finding.severity_label_confidence, finding.category,
                    finding.recommended_fix,
                    target.provider_key, target.repository_key,
                    target.subject_kind, target.change_request_number,
                    target.head_revision, target.base_revision,
                    target.stack_parent_target_id,
                    target.target_id AS canonical_target_id,
                    parent.provider_key AS stack_parent_provider_key,
                    parent.repository_key AS stack_parent_repository_key,
                    parent.subject_kind AS stack_parent_subject_kind,
                    parent.change_request_number
                        AS stack_parent_change_request_number,
                    parent.head_revision AS stack_parent_head_revision,
                    parent.base_revision AS stack_parent_base_revision,
                    producing_pass.pass_id
                        AS canonical_producing_pass_id,
                    producing_pass.pass_kind AS producing_pass_kind,
                    producing_pass.state_kind AS producing_pass_state_kind,
                    producing_pass.turn_id AS producing_pass_turn_id,
                    producing_pass.output_frontier_id
                        AS producing_pass_output_frontier_id,
                    producing_pass.result_kind
                        AS producing_pass_result_kind,
                    producing_pass.result_finding_id
                        AS producing_pass_result_finding_id,
                    producing_pass.result_finding_run_id
                        AS producing_pass_result_finding_run_id,
                    producing_pass.result_finding_pass_id
                        AS producing_pass_result_finding_pass_id,
                    producing_pass.result_event_ordinal
                        AS producing_pass_result_event_ordinal,
                    producing_pass.result_event_kind
                        AS producing_pass_result_event_kind,
                    producing_pass.result_reason
                        AS producing_pass_result_reason,
                    producing_pass.result_referenced_finding_id
                        AS producing_pass_result_referenced_finding_id,
                    producing_pass.result_referenced_finding_run_id
                        AS producing_pass_result_referenced_finding_run_id,
                    producing_pass.result_referenced_finding_target_id
                        AS producing_pass_result_referenced_finding_target_id,
                    producing_pass.result_referenced_finding_pass_id
                        AS producing_pass_result_referenced_finding_pass_id,
                    producing_pass.result_referenced_finding_status
                        AS producing_pass_result_referenced_finding_status,
                    producing_pass.result_external_link_id
                        AS producing_pass_result_external_link_id,
                    producing_pass.result_external_object_key
                        AS producing_pass_result_external_object_key,
                    producing_pass.result_observation_state
                        AS producing_pass_result_observation_state,
                    producing_run.policy_version
                        AS producing_policy_version,
                    producing_run.run_id AS canonical_producing_run_id,
                    producing_run.workflow_kind AS producing_workflow_kind,
                    producing_run.minimum_judge_confidence
                        AS producing_minimum_judge_confidence,
                    producing_run.minimum_publication_confidence
                        AS producing_minimum_publication_confidence,
                    producing_run.state_kind AS producing_run_state_kind,
                    producing_run.state_pass_id AS producing_run_state_pass_id
               FROM review_finding AS finding
               LEFT JOIN review_target AS target
                 ON target.target_id = finding.target_id
               LEFT JOIN review_target AS parent
                 ON parent.target_id = target.stack_parent_target_id
               LEFT JOIN review_pass AS producing_pass
                 ON producing_pass.pass_id = finding.producing_pass_id
                AND producing_pass.run_id = finding.run_id
                AND producing_pass.target_id = finding.target_id
               LEFT JOIN review_run AS producing_run
                 ON producing_run.run_id = producing_pass.run_id
                AND producing_run.target_id = producing_pass.target_id
              WHERE finding.finding_id = $1",
        )
        .bind(finding.into_uuid())
        .fetch_optional(&mut *connection)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        require_joined_reference(
            &row,
            "canonical_target_id",
            "review_finding",
            "referenced target row is missing",
        )?;
        require_joined_reference(
            &row,
            "canonical_producing_pass_id",
            "review_finding",
            "producing pass row is missing",
        )?;
        require_joined_reference(
            &row,
            "canonical_producing_run_id",
            "review_finding",
            "producing run row is missing",
        )?;
        let target = load_target_on_connection(connection, target_id(row.try_get("target_id")?))
            .await?
            .ok_or_else(|| {
                corruption(
                    "review_finding",
                    String::from("referenced target row is missing"),
                )
            })?;
        let producing_pass_id = pass_id(row.try_get("producing_pass_id")?);
        let producing_pass = load_pass_on_connection(connection, producing_pass_id)
            .await?
            .ok_or_else(|| {
                corruption(
                    "review_finding",
                    String::from("producing pass row is missing"),
                )
            })?
            .evidence();
        let proposal = decode_finding_proposal(&row, producing_pass, &target)?;
        let event_rows = sqlx::query(
            "SELECT event.finding_id, event.event_ordinal,
                    event.finding_run_id, event.target_id,
                    event.event_pass_id, event.event_pass_run_id,
                    event_pass.pass_id AS canonical_event_pass_id,
                    event_pass.pass_kind AS event_pass_kind,
                    event_pass.state_kind AS event_pass_state_kind,
                    event_pass.turn_id AS event_pass_turn_id,
                    event_pass.output_frontier_id
                        AS event_pass_output_frontier_id,
                    event_pass.result_kind
                        AS event_pass_result_kind,
                    event_pass.result_finding_id
                        AS event_pass_result_finding_id,
                    event_pass.result_finding_run_id
                        AS event_pass_result_finding_run_id,
                    event_pass.result_finding_pass_id
                        AS event_pass_result_finding_pass_id,
                    event_pass.result_event_ordinal
                        AS event_pass_result_event_ordinal,
                    event_pass.result_event_kind
                        AS event_pass_result_event_kind,
                    event_pass.result_reason
                        AS event_pass_result_reason,
                    event_pass.result_referenced_finding_id
                        AS event_pass_result_referenced_finding_id,
                    event_pass.result_referenced_finding_run_id
                        AS event_pass_result_referenced_finding_run_id,
                    event_pass.result_referenced_finding_target_id
                        AS event_pass_result_referenced_finding_target_id,
                    event_pass.result_referenced_finding_pass_id
                        AS event_pass_result_referenced_finding_pass_id,
                    event_pass.result_referenced_finding_status
                        AS event_pass_result_referenced_finding_status,
                    event_pass.result_external_link_id
                        AS event_pass_result_external_link_id,
                    event_pass.result_external_object_key
                        AS event_pass_result_external_object_key,
                    event_pass.result_observation_state
                        AS event_pass_result_observation_state,
                    event_run.run_id AS canonical_event_run_id,
                    event_run.workflow_kind AS event_workflow_kind,
                    event_run.policy_version AS event_policy_version,
                    event_run.minimum_judge_confidence
                        AS event_minimum_judge_confidence,
                    event_run.minimum_publication_confidence
                        AS event_minimum_publication_confidence,
                    event_run.state_kind AS event_run_state_kind,
                    event_run.state_pass_id AS event_run_state_pass_id,
                    event.event_kind, event.reason,
                    event.referenced_finding_id,
                    event.referenced_finding_run_id,
                    event.referenced_finding_target_id,
                    event.referenced_finding_pass_id,
                    event.referenced_finding_status,
                    event.external_link_id,
                    referenced_finding.finding_id
                        AS canonical_referenced_finding_id,
                    referenced_finding_pass.pass_id
                        AS canonical_referenced_finding_pass_id
               FROM review_finding_event AS event
               LEFT JOIN review_pass AS event_pass
                 ON event_pass.pass_id = event.event_pass_id
                AND event_pass.run_id = event.event_pass_run_id
                AND event_pass.target_id = event.target_id
               LEFT JOIN review_run AS event_run
                 ON event_run.run_id = event_pass.run_id
                AND event_run.target_id = event_pass.target_id
               LEFT JOIN review_finding AS referenced_finding
                 ON referenced_finding.finding_id =
                    event.referenced_finding_id
                AND referenced_finding.run_id =
                    event.referenced_finding_run_id
                AND referenced_finding.target_id =
                    event.referenced_finding_target_id
                AND referenced_finding.producing_pass_id =
                    event.referenced_finding_pass_id
               LEFT JOIN review_pass AS referenced_finding_pass
                 ON referenced_finding_pass.pass_id =
                    referenced_finding.producing_pass_id
                AND referenced_finding_pass.run_id =
                    referenced_finding.run_id
                AND referenced_finding_pass.target_id =
                    referenced_finding.target_id
              WHERE event.finding_id = $1
              ORDER BY event.event_ordinal",
        )
        .bind(finding.into_uuid())
        .fetch_all(&mut *connection)
        .await?;
        let mut events = Vec::with_capacity(event_rows.len());
        for row in event_rows {
            require_joined_reference(
                &row,
                "canonical_event_pass_id",
                "review_finding_event",
                "event pass row is missing",
            )?;
            require_joined_reference(
                &row,
                "canonical_event_run_id",
                "review_finding_event",
                "event run row is missing",
            )?;
            if row
                .try_get::<Option<Uuid>, _>("referenced_finding_id")?
                .is_some()
            {
                require_joined_reference(
                    &row,
                    "canonical_referenced_finding_id",
                    "review_finding_event",
                    "referenced finding row is missing",
                )?;
                require_joined_reference(
                    &row,
                    "canonical_referenced_finding_pass_id",
                    "review_finding_event",
                    "referenced finding producing pass row is missing",
                )?;
            }
            let event_pass_id = pass_id(row.try_get("event_pass_id")?);
            let event_pass = load_pass_on_connection(connection, event_pass_id)
                .await?
                .ok_or_else(|| {
                    corruption(
                        "review_finding_event",
                        String::from("event pass row is missing"),
                    )
                })?
                .evidence();
            let canonical_link = match row
                .try_get::<Option<Uuid>, _>("external_link_id")?
                .map(external_link_id)
            {
                Some(link) => Some(
                    Self::load_external_link_on_connection(connection, link)
                        .await?
                        .ok_or_else(|| {
                            corruption(
                                "review_finding_event",
                                String::from("event external link is missing"),
                            )
                        })?,
                ),
                None => None,
            };
            events.push(decode_finding_event(
                &row,
                proposal.reference(),
                &target,
                event_pass,
                canonical_link.as_ref(),
            )?);
        }
        let finding = ReviewFinding::try_reconstitute(proposal, events).map_err(|error| {
            corruption(
                "review_finding",
                format!("domain reconstitution failed: {:?}", error.failure()),
            )
        })?;
        let head_is_complete = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                 SELECT 1
                   FROM review_finding_event_head AS head
                   LEFT JOIN review_finding_event AS event
                     ON event.finding_id = head.finding_id
                    AND event.event_ordinal = head.event_ordinal
                   LEFT JOIN review_pass AS event_pass
                     ON event_pass.pass_id = event.event_pass_id
                    AND event_pass.run_id = event.event_pass_run_id
                    AND event_pass.target_id = event.target_id
                  WHERE head.finding_id = $1
                    AND (
                        (
                            head.event_ordinal IS NULL
                            AND head.status = $2
                            AND head.event_pass_kind IS NULL
                            AND head.external_link_id IS NULL
                            AND NOT EXISTS (
                                SELECT 1
                                  FROM review_finding_event AS existing
                                 WHERE existing.finding_id = $1
                            )
                        )
                        OR (
                            head.event_ordinal IS NOT NULL
                            AND event.event_kind = head.status
                            AND event_pass.pass_kind = head.event_pass_kind
                            AND event.external_link_id
                                IS NOT DISTINCT FROM head.external_link_id
                            AND head.event_ordinal = (
                                SELECT max(latest.event_ordinal)
                                  FROM review_finding_event AS latest
                                 WHERE latest.finding_id = $1
                            )
                        )
                    )
             )",
        )
        .bind(finding.proposal().reference().finding().into_uuid())
        .bind(encode_finding_status(ReviewFindingStatus::Open))
        .fetch_one(&mut *connection)
        .await?;
        if !head_is_complete {
            return Err(corruption(
                "review_finding_event_head",
                String::from("event head does not name the exact latest event"),
            ));
        }
        Ok(Some(finding))
    }
}
