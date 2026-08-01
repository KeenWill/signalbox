//! Reviewed SQL statements that acquire explicit persistence row locks.

pub(crate) const START_ELIGIBLE_TURN: &str = "SELECT
            EXISTS (
                SELECT 1
                  FROM session
                 WHERE session_id = $1
            ),
            (
                SELECT session_id
                  FROM session_scheduler
                 WHERE session_id = $1
                 FOR UPDATE
            )";

pub(crate) const STARTUP_RECOVERY: &str = "SELECT
            EXISTS (
                SELECT 1
                  FROM session
                 WHERE session_id = $1
            ),
            (
                SELECT session_id
                  FROM session_scheduler
                 WHERE session_id = $1
                 FOR UPDATE
            ),
            (
                SELECT turn_id
                  FROM turn_lifecycle
                 WHERE session_id = $1
                   AND state_kind = 'active'
            )";

pub(crate) const CONTEXT_COMPACTION_SCHEDULER: &str = "SELECT
            EXISTS (
                SELECT 1
                  FROM session
                 WHERE session_id = $1
            ),
            (
                SELECT session_id
                  FROM session_scheduler
                 WHERE session_id = $1
                 FOR UPDATE
            )";

pub(crate) const CONTEXT_COMPACTION_DEFAULTS: &str = "SELECT current_version
           FROM session_current_defaults
          WHERE session_id = $1
          FOR UPDATE";

pub(crate) const CONTEXT_COMPACTION_LIFECYCLE_SESSION: &str =
    "SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE";

pub(crate) const SUBMIT_INPUT_SESSION: &str =
    "SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE";

pub(crate) const SUBMIT_INPUT_SCHEDULER: &str = "SELECT session_id
           FROM session_scheduler
          WHERE session_id = $1
          FOR UPDATE";

pub(crate) const SUBMIT_INPUT_DEFAULTS: &str = "SELECT current_version
           FROM session_current_defaults
          WHERE session_id = $1
          FOR UPDATE";

pub(crate) const REPLACE_SESSION_METADATA: &str =
    "SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE";

pub(crate) const PLAN_APPEND_ATTEMPT: &str = "SELECT attempt.attempt_id
  FROM tool_attempt AS attempt
  JOIN tool_request AS request
    ON request.request_id = attempt.request_id
 WHERE attempt.attempt_id = $1
   AND attempt.request_id = $2
   AND attempt.issuing_turn_attempt_id = $3
   AND attempt.dispatch_generation = $4
   AND attempt.turn_id = $5
   AND attempt.session_id = $6
   AND attempt.effect_class = 'external_effect'
   AND attempt.state_kind = 'in_flight'
   AND request.request_id = $2
   AND request.session_id = $6
   AND request.turn_id = $5
   AND request.tool_name = 'plan_write'
   AND request.arguments_kind = 'json'
   AND request.arguments_text::jsonb =
        CASE $7::text
            WHEN 'created' THEN jsonb_build_object(
                'kind', 'create',
                'text', $9::text
            )
            WHEN 'text_revised' THEN jsonb_build_object(
                'kind', 'revise',
                'entry_id', $8::numeric,
                'text', $9::text
            )
            WHEN 'status_changed' THEN jsonb_build_object(
                'kind', 'set_status',
                'entry_id', $8::numeric,
                'status', $10::text
            )
        END
 FOR SHARE OF attempt";

pub(crate) const OUTBOX_DELIVERY: &str = "SELECT delivered_through
           FROM outbox_delivery_state
          WHERE singleton
          FOR UPDATE";

pub(crate) const HUB_FENCE_GENERATION: &str = "SELECT generation
           FROM hub_fence_state
          WHERE singleton
          FOR UPDATE";

pub(crate) const RUNNER_ENROLLMENT: &str = "SELECT enrollment_id
               FROM runner_enrollment
              WHERE enrollment_id = $1
              FOR UPDATE";

pub(crate) const RUNNER_GRANT: &str = "SELECT credential_profile_name
               FROM runner_credential_grant
              WHERE session_id = $1
                AND lineage_origin_event_ordinal = $2
                AND runner_id = $3
                AND grant_revision = $4
              FOR UPDATE";

pub(crate) const RUNNER_LEASE_ENROLLMENT_AUTHORITY: &str = "SELECT state_kind
               FROM runner_enrollment
              WHERE enrollment_id = $1
              FOR SHARE";

pub(crate) const RUNNER_LEASE_GRANT_AUTHORITY: &str = "SELECT grant_record.credential_profile_name
               FROM runner_current_credential_grant_audit AS current_audit
               JOIN runner_credential_grant AS grant_record
                 ON grant_record.session_id = current_audit.session_id
                AND grant_record.lineage_origin_event_ordinal =
                    current_audit.lineage_origin_event_ordinal
                AND grant_record.runner_id = current_audit.runner_id
                AND grant_record.grant_revision = current_audit.grant_revision
              WHERE current_audit.session_id = $1
                AND current_audit.lineage_origin_event_ordinal = $2
                AND current_audit.runner_id = $3
                AND current_audit.grant_revision = $4
              FOR SHARE OF current_audit";

pub(crate) const RUNNER_REGISTRATION_HEAD: &str = "SELECT registration_revision
               FROM runner_current_registration
              WHERE enrollment_id = $1
              FOR UPDATE";

pub(crate) const RUNNER_PLACEMENT_HEAD: &str =
    "SELECT record.event_ordinal, record.placement_revision,
                    record.state_kind, record.pinned_runner_id,
                    record.pinned_credential_profile_name,
                    record.credential_grant_runner_id,
                    record.credential_grant_lineage_origin_ordinal,
                    record.credential_grant_revision
               FROM runner_current_session_placement AS current_placement
               JOIN runner_session_placement_record AS record
                 ON record.session_id = current_placement.session_id
                AND record.event_ordinal = current_placement.event_ordinal
              WHERE current_placement.session_id = $1
              FOR UPDATE OF current_placement";

pub(crate) const RUNNER_LEASE_HEAD: &str = "SELECT current_event.event_ordinal, event.state_kind,
                    lease_generation.attempt_id,
                    lease_generation.session_id,
                    lease_generation.runner_id,
                    lease_generation.tool_name,
                    lease_generation.effect_class,
                    lease_generation.credential_profile_name,
                    lease_generation.credential_grant_lineage_origin_ordinal,
                    lease_generation.credential_grant_revision,
                    lease_generation.credential_approval_kind,
                    attempt.session_id AS canonical_dispatch_session,
                    attempt.turn_id AS canonical_dispatch_turn,
                    attempt.issuing_turn_attempt_id
                        AS canonical_dispatch_issuing_attempt,
                    attempt.request_id AS canonical_dispatch_request,
                    attempt.dispatch_generation
                        AS canonical_dispatch_generation
               FROM runner_current_lease_event AS current_event
               JOIN runner_lease_event AS event
                 ON event.lease_id = current_event.lease_id
                AND event.generation = current_event.generation
                AND event.event_ordinal = current_event.event_ordinal
               JOIN runner_lease_generation AS lease_generation
                 ON lease_generation.lease_id = current_event.lease_id
                AND lease_generation.generation = current_event.generation
               JOIN tool_attempt AS attempt
                 ON attempt.attempt_id = lease_generation.attempt_id
              WHERE current_event.lease_id = $1
                AND current_event.generation = $2
              FOR UPDATE OF current_event";

pub(crate) const RUNNER_LEASE_PLACEMENT: &str = "SELECT record.*
           FROM runner_current_session_placement AS current_placement
           JOIN runner_session_placement_record AS record
             ON record.session_id = current_placement.session_id
            AND record.event_ordinal = current_placement.event_ordinal
          WHERE current_placement.session_id = $1
          FOR UPDATE OF current_placement";
pub(crate) const REVIEW_RUN_TRANSITION: &str = "SELECT
            workflow_run.run_id, workflow_run.target_id,
            workflow_run.workflow_kind, workflow_run.policy_version,
            workflow_run.minimum_judge_confidence,
            workflow_run.minimum_publication_confidence,
            workflow_run.state_kind, workflow_run.state_pass_id,
            canonical_pass.pass_id AS evidence_pass_id,
            canonical_pass.run_id AS evidence_pass_run_id,
            canonical_pass.target_id AS evidence_pass_target_id,
            canonical_pass.pass_kind AS evidence_pass_kind,
            canonical_pass.state_kind AS evidence_pass_state_kind,
            canonical_pass.turn_id AS evidence_pass_turn_id,
            canonical_pass.output_frontier_id
                AS evidence_pass_output_frontier_id,
            canonical_pass.result_kind
                AS evidence_pass_result_kind,
            canonical_pass.result_finding_id
                AS evidence_pass_result_finding_id,
            canonical_pass.result_finding_run_id
                AS evidence_pass_result_finding_run_id,
            canonical_pass.result_finding_pass_id
                AS evidence_pass_result_finding_pass_id,
            canonical_pass.result_event_ordinal
                AS evidence_pass_result_event_ordinal,
            canonical_pass.result_event_kind
                AS evidence_pass_result_event_kind,
            canonical_pass.result_reason
                AS evidence_pass_result_reason,
            canonical_pass.result_referenced_finding_id
                AS evidence_pass_result_referenced_finding_id,
            canonical_pass.result_referenced_finding_run_id
                AS evidence_pass_result_referenced_finding_run_id,
            canonical_pass.result_referenced_finding_pass_id
                AS evidence_pass_result_referenced_finding_pass_id,
            canonical_pass.result_referenced_finding_status
                AS evidence_pass_result_referenced_finding_status,
            canonical_pass.result_external_link_id
                AS evidence_pass_result_external_link_id,
            canonical_pass.result_external_object_key
                AS evidence_pass_result_external_object_key,
            canonical_pass.result_observation_state
                AS evidence_pass_result_observation_state
       FROM review_run AS workflow_run
       LEFT JOIN review_pass AS canonical_pass
         ON canonical_pass.run_id = workflow_run.run_id
        AND canonical_pass.target_id = workflow_run.target_id
      WHERE workflow_run.run_id = $1
      FOR UPDATE OF workflow_run";

pub(crate) const REVIEW_PASS_TRANSITION: &str = "SELECT
            workflow_pass.pass_id, workflow_pass.run_id,
            workflow_pass.target_id, workflow_pass.pass_kind,
            canonical_run.workflow_kind AS run_workflow_kind,
            workflow_pass.session_id AS pass_session_id,
            workflow_pass.accepted_input_id, workflow_pass.origin_turn_id,
            workflow_pass.state_kind,
            workflow_pass.turn_id, workflow_pass.output_frontier_id,
            workflow_pass.result_kind,
            workflow_pass.result_finding_id,
            workflow_pass.result_finding_run_id,
            workflow_pass.result_finding_pass_id,
            workflow_pass.result_event_ordinal,
            workflow_pass.result_event_kind,
            workflow_pass.result_reason,
            workflow_pass.result_referenced_finding_id,
            workflow_pass.result_referenced_finding_run_id,
            workflow_pass.result_referenced_finding_pass_id,
            workflow_pass.result_referenced_finding_status,
            workflow_pass.result_external_link_id,
            workflow_pass.result_external_object_key,
            workflow_pass.result_observation_state,
            canonical_input.session_id AS accepted_input_session_id,
            canonical_input.origin_turn_id AS accepted_input_origin_turn_id,
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
            canonical_run.target_id AS canonical_run_target_id
       FROM review_pass AS workflow_pass
       JOIN review_run AS canonical_run
         ON canonical_run.run_id = workflow_pass.run_id
        AND canonical_run.target_id = workflow_pass.target_id
       LEFT JOIN accepted_input AS canonical_input
         ON canonical_input.accepted_input_id =
            workflow_pass.accepted_input_id
       LEFT JOIN turn_lifecycle AS canonical_turn
         ON canonical_turn.turn_id =
            COALESCE(workflow_pass.turn_id, $2)
      WHERE workflow_pass.pass_id = $1
      FOR UPDATE OF workflow_pass";

pub(crate) const REVIEW_FINDINGS_TRANSITION: &str = "SELECT finding_id
       FROM review_finding
      WHERE finding_id = ANY($1)
      ORDER BY finding_id
      FOR NO KEY UPDATE";

pub(crate) const REVIEW_EXTERNAL_LINK_TRANSITION: &str = "SELECT external_link_id
       FROM review_external_link
      WHERE external_link_id = $1
      FOR NO KEY UPDATE";
