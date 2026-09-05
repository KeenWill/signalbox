-- Retire repository-watch and convergence-sweep decision state.

-- A no-model-activity target could own a commissioned-dispatch module park.
-- Removing that target removes the module's authority, so lift only the exact
-- parks it owned before dropping the ownership record. The migration is one
-- transaction: a failed restoration leaves both the park and its owner intact.
DO $$
DECLARE
    parked_session uuid;
    held session_lifecycle%ROWTYPE;
    admission_state text;
BEGIN
    FOR parked_session IN
        SELECT DISTINCT target.parked_session_id
          FROM convergence_sweep_target AS target
         WHERE target.parked_session_id IS NOT NULL
         ORDER BY target.parked_session_id
    LOOP
        SELECT * INTO held
          FROM session_lifecycle
         WHERE session_id = parked_session
         FOR UPDATE;

        IF NOT FOUND
           OR held.state_kind <> 'parked'
           OR held.parked_cause <> 'module_park'
           OR held.parked_responder <> 'commissioned_dispatch'
           OR held.parked_standing_cause_kind IS NOT NULL
        THEN
            CONTINUE;
        END IF;

        IF held.pending_terminal_outcome_kind IS NOT NULL THEN
            RAISE EXCEPTION
                'commissioned-dispatch park % has a pending terminal outcome',
                parked_session
                USING ERRCODE = '23514';
        END IF;

        SELECT CASE
            WHEN held.start_gate_held THEN 'created'
            WHEN NOT EXISTS (
                SELECT 1 FROM turn_lifecycle WHERE session_id = parked_session
            ) THEN 'created'
            WHEN NOT EXISTS (
                SELECT 1 FROM turn_lifecycle
                 WHERE session_id = parked_session
                   AND start_lineage_kind IS NOT NULL
            ) THEN 'dispatched'
            ELSE NULL
        END INTO admission_state;

        IF admission_state IS NULL THEN
            PERFORM project_session_lifecycle(
                parked_session, true, 'module', 'commissioned_dispatch'
            );
        ELSE
            UPDATE session_lifecycle
               SET state_kind = admission_state,
                   state_entered_at = statement_timestamp(),
                   actor_kind = 'module',
                   actor_module = 'commissioned_dispatch',
                   actor_turn_id = NULL,
                   actor_tool_request_id = NULL,
                   waiting_kind = NULL,
                   waiting_waker = NULL,
                   waiting_subject_session_id = NULL,
                   recovering_op = NULL,
                   blocked_reason = NULL,
                   blocked_cycle = NULL,
                   parked_cause = NULL,
                   parked_responder = NULL,
                   parked_since = NULL,
                   parked_standing_cause_kind = NULL
             WHERE session_id = parked_session;
        END IF;
    END LOOP;
END;
$$;

DROP VIEW repo_watch_pending_stale_review_clearance;
DROP VIEW repo_watch_current_pull_request_convergence;
DROP VIEW convergence_sweep_parked_target;

DROP TABLE repo_watch_stale_review_clearance_result;
DROP TABLE repo_watch_stale_review_clearance_claim;
DROP TABLE repo_watch_stale_review_clearance_recovery_cursor;
DROP TABLE repo_watch_stale_review_clearance;
DROP TABLE repo_watch_convergence_cutoff_goal;
DROP TABLE repo_watch_convergence_cutoff;
DROP TABLE repo_watch_pull_request_convergence;
DROP TABLE repo_watch_pull_request_convergence_identity;
DROP TABLE repo_watch_pull_request_convergence_assessment;
DROP TABLE convergence_sweep_event;
DROP TABLE convergence_sweep_target;

DROP FUNCTION repo_watch_convergence_threads_are_valid(text[]);
DROP FUNCTION repo_watch_convergence_check_names_are_valid(text[]);
DROP FUNCTION convergence_sweep_retry_budget();
