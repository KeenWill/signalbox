use super::decode::{
    authenticate_external_object_target, decode_external_link_attachment,
    decode_external_link_observation, decode_external_link_root, decode_pass_turn_evidence,
    reconstitute_pass, same_pass_execution, same_reservation,
};
use super::load::{
    load_external_link_claims_on_connection, load_pass_on_connection, load_target_on_connection,
};
use super::pass_codec::pass_state_result;
use super::write::{bind_pass_result, insert_finding_event};
use super::{
    ReserveExternalLinkOutcome, ReviewExternalLinkReservationConflict,
    ReviewWorkflowInsertionError, ReviewWorkflowStore, ReviewWorkflowStoreError,
    ReviewWorkflowTransitionError, begin_repeatable_read, commit_mutation, corruption,
    decode_review_policy, decode_run_state, decode_workflow_kind, encode_external_object_kind,
    encode_external_object_state, encode_link_association, pass_id, require_joined_reference,
    target_id,
};
use signalbox_domain::{
    ReviewExternalLink, ReviewExternalLinkAssociation, ReviewExternalLinkAttachment,
    ReviewExternalLinkId, ReviewExternalLinkNoChangeResult, ReviewExternalLinkObservation,
    ReviewExternalLinkTransitionFailure, ReviewFindingEvent, ReviewFindingEventKind,
    ReviewFindingEventResultKind, ReviewFindingExternalLinkRef, ReviewFindingId,
    ReviewFindingPendingExternalLinkRef, ReviewPassEvidence, ReviewPassId, ReviewPassResult,
    ReviewRun, ReviewRunEvidence, ReviewRunId, ReviewRunReconstitutionInput,
};
use sqlx::types::Uuid;
use sqlx::{PgConnection, Postgres, Row, Transaction};

impl ReviewWorkflowStore {
    /// Idempotently inserts one canonical pre-effect external-link reservation.
    pub async fn reserve_external_link(
        &self,
        requested: ReviewExternalLink,
    ) -> Result<ReserveExternalLinkOutcome, ReviewWorkflowStoreError> {
        let mut transaction = self.pool.begin().await?;
        let outcome = self
            .reserve_external_link_in_transaction(&mut transaction, requested)
            .await?;
        match &outcome {
            ReserveExternalLinkOutcome::Inserted(_) => commit_mutation(transaction).await?,
            ReserveExternalLinkOutcome::Existing(_) => transaction.commit().await?,
        }
        Ok(outcome)
    }

    pub(crate) async fn reserve_external_link_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        requested: ReviewExternalLink,
    ) -> Result<ReserveExternalLinkOutcome, ReviewWorkflowStoreError> {
        if requested.attachment().is_some()
            || !requested.observations().is_empty()
            || !requested.claims().is_empty()
        {
            return Err(ReviewWorkflowStoreError::InvalidInsertion(
                ReviewWorkflowInsertionError::ExternalLinkNotPending,
            ));
        }
        let association = encode_link_association(requested.association());
        let result = sqlx::query(
            "INSERT INTO review_external_link
                (external_link_id, target_id, association_kind, run_id,
                 finding_id, finding_producing_pass_id, provider_key,
                 object_kind)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT DO NOTHING",
        )
        .bind(requested.id().into_uuid())
        .bind(requested.association().target().into_uuid())
        .bind(association.kind)
        .bind(association.run.map(ReviewRunId::into_uuid))
        .bind(association.finding.map(ReviewFindingId::into_uuid))
        .bind(association.finding_pass.map(ReviewPassId::into_uuid))
        .bind(requested.provider().as_str())
        .bind(encode_external_object_kind(requested.object_kind()))
        .execute(&mut **transaction)
        .await?;
        if result.rows_affected() == 1 {
            return Ok(ReserveExternalLinkOutcome::Inserted(requested));
        }
        sqlx::query(crate::lock_inventory::REVIEW_EXTERNAL_LINK_TRANSITION)
            .bind(requested.id().into_uuid())
            .fetch_one(&mut **transaction)
            .await?;
        let existing = Self::load_external_link_on_connection(transaction, requested.id())
            .await?
            .ok_or_else(|| {
                corruption(
                    "review_external_link",
                    String::from("conflicting reservation disappeared"),
                )
            })?;
        if same_reservation(&existing, &requested) {
            Ok(ReserveExternalLinkOutcome::Existing(existing))
        } else {
            Err(ReviewWorkflowStoreError::ReservationConflict(
                ReviewExternalLinkReservationConflict {
                    existing: Box::new(existing),
                    requested: Box::new(requested),
                },
            ))
        }
    }

    /// Appends the one immutable external-object attachment.
    pub async fn attach_external_link(
        &self,
        link: ReviewExternalLinkId,
        attachment: ReviewExternalLinkAttachment,
    ) -> Result<Option<ReviewExternalLink>, ReviewWorkflowStoreError> {
        let mut transaction = self.pool.begin().await?;
        let next = self
            .attach_external_link_in_transaction(&mut transaction, link, attachment)
            .await?;
        if next.is_some() {
            commit_mutation(transaction).await?;
        } else {
            transaction.rollback().await?;
        }
        Ok(next)
    }

    pub(crate) async fn attach_external_link_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        link: ReviewExternalLinkId,
        attachment: ReviewExternalLinkAttachment,
    ) -> Result<Option<ReviewExternalLink>, ReviewWorkflowStoreError> {
        let locked = sqlx::query(crate::lock_inventory::REVIEW_EXTERNAL_LINK_TRANSITION)
            .bind(link.into_uuid())
            .fetch_optional(&mut **transaction)
            .await?;
        if locked.is_none() {
            return Ok(None);
        }
        let current = Self::load_external_link_on_connection(transaction, link)
            .await?
            .ok_or_else(|| {
                corruption(
                    "review_external_link",
                    String::from("locked reservation disappeared"),
                )
            })?;
        current
            .clone()
            .attach(attachment.clone())
            .map_err(|error| {
                ReviewWorkflowStoreError::InvalidTransition(
                    ReviewWorkflowTransitionError::ExternalLink(error),
                )
            })?;
        let next = current.attach(attachment.clone()).map_err(|error| {
            ReviewWorkflowStoreError::InvalidTransition(
                ReviewWorkflowTransitionError::ExternalLink(error),
            )
        })?;
        let posted_event = match pass_state_result(attachment.pass_evidence().state()) {
            Some(ReviewPassResult::ExternalLinkAttachment(result)) => {
                result.finding_event().map(|event| {
                    let link = ReviewFindingExternalLinkRef::try_new(event.finding(), &next)
                        .map_err(|error| {
                            corruption(
                                "review_external_link_attachment",
                                format!("posted result link is invalid: {:?}", error.failure()),
                            )
                        })?;
                    Ok::<ReviewFindingEvent, ReviewWorkflowStoreError>(ReviewFindingEvent::new(
                        event.finding(),
                        event.ordinal(),
                        attachment.pass(),
                        attachment.pass_evidence().clone(),
                        attachment.run_evidence(),
                        ReviewFindingEventKind::Posted {
                            link: Box::new(link),
                        },
                    ))
                })
            }
            _ => None,
        }
        .transpose()?;
        let transition_finding = posted_event
            .as_ref()
            .map(|event| event.finding().finding())
            .or_else(|| match next.association() {
                ReviewExternalLinkAssociation::Finding(reference) => Some(reference.finding()),
                ReviewExternalLinkAssociation::Run(_)
                | ReviewExternalLinkAssociation::Target(_) => None,
            });
        if let Some(finding) = transition_finding {
            sqlx::query(crate::lock_inventory::REVIEW_FINDINGS_TRANSITION)
                .bind(vec![finding.into_uuid()])
                .fetch_all(&mut **transaction)
                .await?;
        }
        if posted_event.is_none()
            && let ReviewExternalLinkAssociation::Finding(reference) = next.association()
        {
            let current_finding = self
                .load_finding_projection_on_connection(transaction, reference.finding())
                .await?
                .ok_or_else(|| {
                    corruption(
                        "review_external_link_attachment",
                        String::from("associated finding is missing"),
                    )
                })?;
            if matches!(
                current_finding.events().last().map(ReviewFindingEvent::kind),
                Some(ReviewFindingEventKind::BlockedWithReason {
                    link: Some(pending),
                    ..
                }) if pending.link() == link
            ) {
                return Err(ReviewWorkflowStoreError::IncompletePublicationReconciliation);
            }
        }
        if let Some(event) = posted_event.as_ref() {
            let current_finding = self
                .load_finding_projection_on_connection(transaction, event.finding().finding())
                .await?
                .ok_or_else(|| {
                    corruption(
                        "review_external_link_attachment",
                        String::from("posted result finding is missing"),
                    )
                })?;
            current_finding.apply(event.clone()).map_err(|error| {
                ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::Finding(
                    error,
                ))
            })?;
        }
        if posted_event.is_none()
            && sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                     SELECT 1
                       FROM review_finding_event
                      WHERE external_link_id = $1
                        AND event_kind = 'blocked_with_reason'
                 )",
            )
            .bind(link.into_uuid())
            .fetch_one(&mut **transaction)
            .await?
        {
            return Err(ReviewWorkflowStoreError::IncompletePublicationReconciliation);
        }
        bind_pass_result(transaction, attachment.pass_evidence()).await?;
        sqlx::query(
            "INSERT INTO review_external_link_attachment
                (external_link_id, target_id, pass_run_id, pass_id,
                 provider_key, object_kind, external_object_key)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(attachment.link().into_uuid())
        .bind(next.association().target().into_uuid())
        .bind(attachment.pass().run().run().into_uuid())
        .bind(attachment.pass().pass().into_uuid())
        .bind(next.provider().as_str())
        .bind(encode_external_object_kind(next.object_kind()))
        .bind(attachment.external_object().as_str())
        .execute(&mut **transaction)
        .await?;
        if let Some(event) = posted_event.as_ref() {
            insert_finding_event(transaction, event).await?;
        }
        Ok(Some(next))
    }

    /// Appends one same-target external-state observation.
    pub async fn append_external_observation(
        &self,
        link: ReviewExternalLinkId,
        observation: ReviewExternalLinkObservation,
    ) -> Result<Option<ReviewExternalLink>, ReviewWorkflowStoreError> {
        let Some(_) = self.load_external_link(link).await? else {
            return Ok(None);
        };
        let mut transaction = self.pool.begin().await?;
        sqlx::query(crate::lock_inventory::REVIEW_EXTERNAL_LINK_TRANSITION)
            .bind(link.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let current = Self::load_external_link_on_connection(&mut transaction, link)
            .await?
            .ok_or_else(|| {
                corruption(
                    "review_external_link",
                    String::from("locked reservation disappeared"),
                )
            })?;
        let observed = current.clone().observe(observation.clone());
        if let Err(error) = &observed
            && error.failure() != ReviewExternalLinkTransitionFailure::UnchangedObservation
        {
            return Err(ReviewWorkflowStoreError::InvalidTransition(
                ReviewWorkflowTransitionError::ExternalLink(error.clone()),
            ));
        }
        if let Some(latest) = current
            .observations()
            .last()
            .filter(|latest| latest.state() == observation.state())
        {
            if latest.ordinal().get().checked_add(1) != Some(observation.ordinal().get()) {
                return Err(corruption(
                    "review_external_link_observation",
                    String::from("unchanged report ordinal is not contiguous"),
                ));
            }
            let canonical_pass =
                load_pass_on_connection(&mut transaction, observation.pass().pass())
                    .await?
                    .ok_or_else(|| {
                        corruption(
                            "review_external_link_observation",
                            String::from("observing pass row is missing"),
                        )
                    })?
                    .evidence();
            if !same_pass_execution(&canonical_pass, observation.pass_evidence()) {
                return Err(corruption(
                    "review_external_link_observation",
                    String::from("observing pass differs from canonical execution facts"),
                ));
            }
            let no_change_pass = canonical_pass
                .project_result(ReviewPassResult::ExternalLinkNoChange(
                    ReviewExternalLinkNoChangeResult::new(
                        link,
                        latest.ordinal(),
                        observation.state(),
                    ),
                ))
                .ok_or_else(|| {
                    corruption(
                        "review_external_link_observation",
                        String::from("unchanged report pass cannot bind the no-change result"),
                    )
                })?;
            let next = current
                .confirm_unchanged(no_change_pass.clone(), observation.run_evidence())
                .map_err(|error| {
                    ReviewWorkflowStoreError::InvalidTransition(
                        ReviewWorkflowTransitionError::ExternalLink(error),
                    )
                })?;
            bind_pass_result(&mut transaction, &no_change_pass).await?;
            commit_mutation(transaction).await?;
            return Ok(Some(next));
        }
        let next = observed.map_err(|error| {
            ReviewWorkflowStoreError::InvalidTransition(
                ReviewWorkflowTransitionError::ExternalLink(error),
            )
        })?;
        bind_pass_result(&mut transaction, observation.pass_evidence()).await?;
        sqlx::query(
            "INSERT INTO review_external_link_observation
                (external_link_id, observation_ordinal, target_id,
                 pass_run_id, pass_id, object_state)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(observation.link().into_uuid())
        .bind(i64::from(observation.ordinal().get()))
        .bind(next.association().target().into_uuid())
        .bind(observation.pass().run().run().into_uuid())
        .bind(observation.pass().pass().into_uuid())
        .bind(encode_external_object_state(observation.state()))
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        Ok(Some(next))
    }

    /// Binds one blocked publication pass to its exact pending reservation.
    pub async fn block_external_link_publication(
        &self,
        link: ReviewExternalLinkId,
        pass: ReviewPassEvidence,
        run: ReviewRunEvidence,
    ) -> Result<Option<ReviewExternalLink>, ReviewWorkflowStoreError> {
        let Some(_) = self.load_external_link(link).await? else {
            return Ok(None);
        };
        let mut transaction = self.pool.begin().await?;
        sqlx::query(crate::lock_inventory::REVIEW_EXTERNAL_LINK_TRANSITION)
            .bind(link.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let current = Self::load_external_link_on_connection(&mut transaction, link)
            .await?
            .ok_or_else(|| {
                corruption(
                    "review_external_link",
                    String::from("locked reservation disappeared"),
                )
            })?;
        let blocked = current
            .clone()
            .block_publication(pass.clone(), run)
            .map_err(|error| {
                ReviewWorkflowStoreError::InvalidTransition(
                    ReviewWorkflowTransitionError::ExternalLink(error),
                )
            })?;
        let blocked_event = match pass_state_result(pass.state()) {
            Some(ReviewPassResult::FindingEvent(result)) => {
                let ReviewFindingEventResultKind::BlockedWithReason {
                    reason,
                    link: Some(result_link),
                } = result.kind()
                else {
                    return Err(corruption(
                        "review_external_link",
                        String::from("publication finding result is not a linked block"),
                    ));
                };
                let pending =
                    ReviewFindingPendingExternalLinkRef::try_new(result.finding(), &current)
                        .map_err(|error| {
                            corruption(
                                "review_external_link",
                                format!("blocked result link is invalid: {:?}", error.failure()),
                            )
                        })?;
                if pending.link() != *result_link {
                    return Err(corruption(
                        "review_external_link",
                        String::from("blocked result names another reservation"),
                    ));
                }
                Some(ReviewFindingEvent::new(
                    result.finding(),
                    result.ordinal(),
                    pass.reference(),
                    pass.clone(),
                    run,
                    ReviewFindingEventKind::BlockedWithReason {
                        reason: reason.clone(),
                        link: Some(Box::new(pending)),
                    },
                ))
            }
            _ => None,
        };
        if let Some(event) = blocked_event.as_ref() {
            let finding = vec![event.finding().finding().into_uuid()];
            sqlx::query(crate::lock_inventory::REVIEW_FINDINGS_TRANSITION)
                .bind(&finding)
                .fetch_all(&mut *transaction)
                .await?;
            let current_finding = self
                .load_finding_projection_on_connection(&mut transaction, event.finding().finding())
                .await?
                .ok_or_else(|| {
                    corruption(
                        "review_external_link",
                        String::from("blocked result finding is missing"),
                    )
                })?;
            current_finding.apply(event.clone()).map_err(|error| {
                ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::Finding(
                    error,
                ))
            })?;
        }
        if sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                 SELECT 1
                   FROM review_external_link_attachment
                  WHERE external_link_id = $1
             )",
        )
        .bind(link.into_uuid())
        .fetch_one(&mut *transaction)
        .await?
        {
            return Err(ReviewWorkflowStoreError::IncompletePublicationReconciliation);
        }
        match blocked_event.as_ref() {
            Some(event) => insert_finding_event(&mut transaction, event).await?,
            None => bind_pass_result(&mut transaction, &pass).await?,
        }
        commit_mutation(transaction).await?;
        Ok(Some(blocked))
    }

    /// Loads and validates a reservation, optional attachment, and observations.
    pub async fn load_external_link(
        &self,
        link: ReviewExternalLinkId,
    ) -> Result<Option<ReviewExternalLink>, ReviewWorkflowStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let link = Self::load_external_link_on_connection(&mut transaction, link).await?;
        transaction.commit().await?;
        Ok(link)
    }

    pub(crate) async fn load_external_link_on_connection(
        connection: &mut PgConnection,
        link: ReviewExternalLinkId,
    ) -> Result<Option<ReviewExternalLink>, ReviewWorkflowStoreError> {
        let row = sqlx::query(
            "SELECT link.external_link_id, link.target_id,
                    link.association_kind, link.run_id, link.finding_id,
                    link.finding_producing_pass_id,
                    canonical_target.target_id AS canonical_target_id,
                    canonical_target.provider_key,
                    canonical_target.repository_key,
                    canonical_target.subject_kind,
                    canonical_target.change_request_number,
                    canonical_target.head_revision,
                    canonical_target.base_revision,
                    canonical_target.stack_parent_target_id,
                    parent.provider_key AS stack_parent_provider_key,
                    parent.repository_key
                        AS stack_parent_repository_key,
                    parent.subject_kind AS stack_parent_subject_kind,
                    parent.change_request_number
                        AS stack_parent_change_request_number,
                    parent.head_revision
                        AS stack_parent_head_revision,
                    parent.base_revision
                        AS stack_parent_base_revision,
                    canonical_run.run_id AS canonical_run_id,
                    association_finding.producing_pass_id
                        AS canonical_finding_pass_id,
                    association_finding_pass.pass_id
                        AS canonical_finding_producing_pass_id,
                    link.provider_key AS link_provider_key,
                    link.object_kind
               FROM review_external_link AS link
               LEFT JOIN review_target AS canonical_target
                 ON canonical_target.target_id = link.target_id
               LEFT JOIN review_target AS parent
                 ON parent.target_id =
                    canonical_target.stack_parent_target_id
               LEFT JOIN review_run AS canonical_run
                 ON canonical_run.run_id = link.run_id
                AND canonical_run.target_id = link.target_id
               LEFT JOIN review_finding AS association_finding
                 ON association_finding.finding_id = link.finding_id
                AND association_finding.run_id = link.run_id
                AND association_finding.target_id = link.target_id
                AND association_finding.producing_pass_id =
                    link.finding_producing_pass_id
               LEFT JOIN review_pass AS association_finding_pass
                 ON association_finding_pass.pass_id =
                    association_finding.producing_pass_id
                AND association_finding_pass.run_id =
                    association_finding.run_id
                AND association_finding_pass.target_id =
                    association_finding.target_id
              WHERE link.external_link_id = $1",
        )
        .bind(link.into_uuid())
        .fetch_optional(&mut *connection)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        require_joined_reference(
            &row,
            "canonical_target_id",
            "review_external_link",
            "referenced target row is missing",
        )?;
        if row.try_get::<Option<Uuid>, _>("run_id")?.is_some() {
            require_joined_reference(
                &row,
                "canonical_run_id",
                "review_external_link",
                "referenced run row is missing",
            )?;
        }
        if row.try_get::<String, _>("association_kind")? == "finding" {
            require_joined_reference(
                &row,
                "canonical_finding_producing_pass_id",
                "review_external_link",
                "finding producing pass row is missing",
            )?;
        }
        let target = load_target_on_connection(connection, target_id(row.try_get("target_id")?))
            .await?
            .ok_or_else(|| {
                corruption(
                    "review_external_link",
                    String::from("referenced target row is missing"),
                )
            })?;
        let (id, association, provider, kind) = decode_external_link_root(&row)?;
        let attachment = sqlx::query(
            "SELECT attachment.external_link_id, attachment.pass_run_id,
                    attachment.pass_id, attachment.target_id,
                    pass.pass_id AS canonical_attachment_pass_id,
                    pass.pass_kind, pass.state_kind AS pass_state_kind,
                    pass.turn_id AS pass_turn_id,
                    pass.output_frontier_id AS pass_output_frontier_id,
                    pass.result_kind AS pass_result_kind,
                    pass.result_finding_id
                        AS pass_result_finding_id,
                    pass.result_finding_run_id
                        AS pass_result_finding_run_id,
                    pass.result_finding_pass_id
                        AS pass_result_finding_pass_id,
                    pass.result_event_ordinal
                        AS pass_result_event_ordinal,
                    pass.result_event_kind AS pass_result_event_kind,
                    pass.result_reason AS pass_result_reason,
                    pass.result_referenced_finding_id
                        AS pass_result_referenced_finding_id,
                    pass.result_referenced_finding_run_id
                        AS pass_result_referenced_finding_run_id,
                    pass.result_referenced_finding_target_id
                        AS pass_result_referenced_finding_target_id,
                    pass.result_referenced_finding_pass_id
                        AS pass_result_referenced_finding_pass_id,
                    pass.result_referenced_finding_status
                        AS pass_result_referenced_finding_status,
                    pass.result_external_link_id
                        AS pass_result_external_link_id,
                    pass.result_external_object_key
                        AS pass_result_external_object_key,
                    pass.result_observation_state
                        AS pass_result_observation_state,
                    pass_run.run_id AS canonical_attachment_run_id,
                    pass_run.workflow_kind AS pass_workflow_kind,
                    pass_run.policy_version AS pass_policy_version,
                    pass_run.minimum_judge_confidence
                        AS pass_minimum_judge_confidence,
                    pass_run.minimum_publication_confidence
                        AS pass_minimum_publication_confidence,
                    pass_run.state_kind AS pass_run_state_kind,
                    pass_run.state_pass_id AS pass_run_state_pass_id,
                    attachment.external_object_key,
                    object_identity.logical_target_id
                        AS canonical_object_target_id,
                    object_identity.object_identity_count
               FROM review_external_link_attachment AS attachment
               LEFT JOIN review_pass AS pass
                 ON pass.pass_id = attachment.pass_id
                AND pass.run_id = attachment.pass_run_id
                AND pass.target_id = attachment.target_id
               LEFT JOIN review_run AS pass_run
                 ON pass_run.run_id = pass.run_id
                AND pass_run.target_id = pass.target_id
               LEFT JOIN LATERAL (
                    SELECT
                        count(*) AS object_identity_count,
                        (array_agg(
                            identity.logical_target_id
                            ORDER BY identity.logical_target_id
                        ))[1] AS logical_target_id
                      FROM review_external_object_identity AS identity
                     WHERE identity.identity_digest = md5(
                               attachment.provider_key
                                   || chr(31)
                                   || attachment.object_kind
                                   || chr(31)
                                   || attachment.external_object_key
                           )
                       AND identity.provider_key = attachment.provider_key
                       AND identity.object_kind = attachment.object_kind
                       AND identity.external_object_key =
                           attachment.external_object_key
               ) AS object_identity ON TRUE
              WHERE attachment.external_link_id = $1",
        )
        .bind(link.into_uuid())
        .fetch_optional(&mut *connection)
        .await?;
        let attachment = match attachment {
            Some(row) => {
                if row.try_get::<i64, _>("object_identity_count")? != 1 {
                    return Err(corruption(
                        "review_external_link_attachment",
                        String::from(
                            "external object identity must have exactly one canonical row",
                        ),
                    ));
                }
                require_joined_reference(
                    &row,
                    "canonical_attachment_pass_id",
                    "review_external_link_attachment",
                    "attaching pass row is missing",
                )?;
                require_joined_reference(
                    &row,
                    "canonical_attachment_run_id",
                    "review_external_link_attachment",
                    "attaching run row is missing",
                )?;
                let logical_target_id = target_id(
                    row.try_get::<Option<Uuid>, _>("canonical_object_target_id")?
                        .ok_or_else(|| {
                            corruption(
                                "review_external_link_attachment",
                                String::from("external object identity row is missing"),
                            )
                        })?,
                );
                let logical_target = load_target_on_connection(connection, logical_target_id)
                    .await?
                    .ok_or_else(|| {
                        corruption(
                            "review_external_link_attachment",
                            String::from("external object identity target row is missing"),
                        )
                    })?;
                authenticate_external_object_target(&target, &logical_target)?;
                let pass_id = pass_id(row.try_get("pass_id")?);
                let pass = load_pass_on_connection(connection, pass_id)
                    .await?
                    .ok_or_else(|| {
                        corruption(
                            "review_external_link_attachment",
                            String::from("attaching pass row is missing"),
                        )
                    })?
                    .evidence();
                Some(decode_external_link_attachment(&row, pass)?)
            }
            None => None,
        };
        let observation_rows = sqlx::query(
            "SELECT observation.external_link_id,
                    observation.observation_ordinal,
                    observation.pass_run_id,
                    observation.pass_run_id AS run_id,
                    observation.pass_id,
                    observation.target_id, observation.object_state,
                    pass.pass_id AS canonical_observation_pass_id,
                    pass.pass_kind, pass.state_kind,
                    pass.session_id AS pass_session_id,
                    pass.accepted_input_id, pass.origin_turn_id,
                    pass.turn_id, pass.output_frontier_id,
                    pass.result_kind, pass.result_finding_id,
                    pass.result_finding_run_id,
                    pass.result_finding_pass_id,
                    pass.result_event_ordinal, pass.result_event_kind,
                    pass.result_reason,
                    pass.result_referenced_finding_id,
                    pass.result_referenced_finding_run_id,
                    pass.result_referenced_finding_target_id,
                    pass.result_referenced_finding_pass_id,
                    pass.result_referenced_finding_status,
                    pass.result_external_link_id,
                    pass.result_external_object_key,
                    pass.result_observation_state,
                    pass_run.run_id AS canonical_run_id,
                    pass_run.target_id AS canonical_run_target_id,
                    pass_run.workflow_kind AS run_workflow_kind,
                    pass_run.policy_version AS pass_policy_version,
                    pass_run.minimum_judge_confidence
                        AS pass_minimum_judge_confidence,
                    pass_run.minimum_publication_confidence
                        AS pass_minimum_publication_confidence,
                    pass_run.state_kind AS run_state_kind,
                    pass_run.state_pass_id AS run_state_pass_id,
                    canonical_target.target_id AS canonical_target_id,
                    canonical_input.session_id
                        AS accepted_input_session_id,
                    canonical_input.origin_turn_id
                        AS accepted_input_origin_turn_id,
                    canonical_origin_turn.turn_id
                        AS canonical_origin_turn_id,
                    canonical_turn.turn_id AS evidence_turn_id,
                    canonical_turn.session_id AS turn_session_id,
                    canonical_turn.origin_accepted_input_id
                        AS turn_accepted_input_id,
                    canonical_turn.state_kind AS turn_state_kind,
                    canonical_turn.terminal_disposition_kind
                        AS turn_terminal_disposition_kind,
                    canonical_turn.terminal_frontier_id
                        AS turn_terminal_frontier_id,
                    produced_findings.produced_finding_count
               FROM review_external_link_observation AS observation
               LEFT JOIN review_pass AS pass
                 ON pass.pass_id = observation.pass_id
                AND pass.run_id = observation.pass_run_id
                AND pass.target_id = observation.target_id
               LEFT JOIN review_run AS pass_run
                 ON pass_run.run_id = pass.run_id
                AND pass_run.target_id = pass.target_id
               LEFT JOIN review_target AS canonical_target
                 ON canonical_target.target_id = pass.target_id
               LEFT JOIN accepted_input AS canonical_input
                 ON canonical_input.accepted_input_id =
                    pass.accepted_input_id
               LEFT JOIN turn_lifecycle AS canonical_origin_turn
                 ON canonical_origin_turn.turn_id = pass.origin_turn_id
                AND canonical_origin_turn.session_id = pass.session_id
                AND canonical_origin_turn.origin_accepted_input_id =
                    pass.accepted_input_id
               LEFT JOIN turn_lifecycle AS canonical_turn
                 ON canonical_turn.turn_id = pass.turn_id
               LEFT JOIN LATERAL (
                   SELECT count(*) AS produced_finding_count
                     FROM review_finding AS produced
                    WHERE produced.producing_pass_id = pass.pass_id
                      AND produced.run_id = pass.run_id
                      AND produced.target_id = pass.target_id
               ) AS produced_findings ON true
              WHERE observation.external_link_id = $1
              ORDER BY observation.observation_ordinal",
        )
        .bind(link.into_uuid())
        .fetch_all(&mut *connection)
        .await?;
        let mut observations = Vec::with_capacity(observation_rows.len());
        for row in observation_rows {
            require_joined_reference(
                &row,
                "canonical_observation_pass_id",
                "review_external_link_observation",
                "observing pass row is missing",
            )?;
            require_joined_reference(
                &row,
                "canonical_run_id",
                "review_external_link_observation",
                "observing run row is missing",
            )?;
            require_joined_reference(
                &row,
                "canonical_target_id",
                "review_external_link_observation",
                "observing target row is missing",
            )?;
            if row.try_get::<i64, _>("produced_finding_count")? != 0 {
                return Err(corruption(
                    "review_external_link_observation",
                    String::from("observing pass unexpectedly produced findings"),
                ));
            }
            let turn_evidence = decode_pass_turn_evidence(&row)?;
            let pass = reconstitute_pass(&row, turn_evidence, Vec::new(), None)?;
            require_joined_reference(
                &row,
                "canonical_origin_turn_id",
                "review_external_link_observation",
                "observing pass origin turn row is missing",
            )?;
            let policy = decode_review_policy(
                &row,
                "pass_policy_version",
                "pass_minimum_judge_confidence",
                "pass_minimum_publication_confidence",
                "review_external_link_observation",
            )?;
            let pass = ReviewPassEvidence::from_pass(&pass, policy);
            ReviewRun::try_reconstitute(ReviewRunReconstitutionInput::new(
                pass.reference().run(),
                decode_workflow_kind(&row.try_get::<String, _>("run_workflow_kind")?)?,
                policy,
                decode_run_state(
                    pass.reference().run(),
                    &row.try_get::<String, _>("run_state_kind")?,
                    row.try_get("run_state_pass_id")?,
                )?,
                Some(pass.clone()),
            ))
            .map_err(|error| {
                corruption(
                    "review_external_link_observation",
                    format!(
                        "observing run contradicts its pass projection: {:?}",
                        error.failure()
                    ),
                )
            })?;
            observations.push(decode_external_link_observation(&row, pass)?);
        }
        let claims = load_external_link_claims_on_connection(connection, link).await?;
        let link = ReviewExternalLink::try_reconstitute(
            id,
            association,
            provider,
            kind,
            attachment,
            observations,
            claims,
            &target,
        )
        .map_err(|error| corruption("review_external_link", format!("{error:?}")))?;
        Ok(Some(link))
    }
}
