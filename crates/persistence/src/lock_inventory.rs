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
            pass_id, run_id, target_id, pass_kind, session_id,
            accepted_input_id, state_kind, turn_id, output_frontier_id
       FROM review_pass
      WHERE pass_id = $1
      FOR UPDATE";
