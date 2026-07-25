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

pub(crate) const OUTBOX_DELIVERY: &str = "SELECT delivered_through
           FROM outbox_delivery_state
          WHERE singleton
          FOR UPDATE";

pub(crate) const HUB_FENCE_GENERATION: &str = "SELECT generation
           FROM hub_fence_state
          WHERE singleton
          FOR UPDATE";

pub(crate) const REVIEW_RUN_TRANSITION: &str = "SELECT
            run_id, target_id, workflow_kind, policy_version,
            minimum_judge_confidence, minimum_publication_confidence,
            state_kind, state_pass_id
       FROM review_run
      WHERE run_id = $1
      FOR UPDATE";

pub(crate) const REVIEW_PASS_TRANSITION: &str = "SELECT
            workflow_pass.pass_id, workflow_pass.run_id,
            workflow_pass.target_id, workflow_pass.pass_kind,
            workflow_pass.session_id AS pass_session_id,
            workflow_pass.accepted_input_id, workflow_pass.state_kind,
            workflow_pass.turn_id, workflow_pass.output_frontier_id,
            canonical_input.session_id AS accepted_input_session_id,
            canonical_turn.turn_id AS evidence_turn_id,
            canonical_turn.session_id AS turn_session_id,
            canonical_turn.origin_accepted_input_id
                AS turn_accepted_input_id,
            canonical_turn.terminal_frontier_id
                AS turn_terminal_frontier_id
       FROM review_pass AS workflow_pass
       LEFT JOIN accepted_input AS canonical_input
         ON canonical_input.accepted_input_id =
            workflow_pass.accepted_input_id
       LEFT JOIN turn_lifecycle AS canonical_turn
         ON canonical_turn.turn_id = workflow_pass.turn_id
      WHERE workflow_pass.pass_id = $1
      FOR UPDATE OF workflow_pass";
