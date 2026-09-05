-- Repository-watch v1 state is disposable derived data. The v2 module is not
-- dispatched yet, so cutover drops the old surface without a backfill.

DROP TRIGGER commissioned_dispatch_counts_pull_request_session
    ON commissioned_dispatch;
DROP TRIGGER repo_watch_dispatch_release_on_terminal_goal ON goal_event;
DROP TRIGGER repo_watch_dispatch_release_on_terminal_turn ON turn_lifecycle;

DROP VIEW repo_watch_current_pull_request_convergence,
          convergence_sweep_parked_target,
          repo_watch_headless_approval_escalation_audit,
          repo_watch_held_dispatch_slot,
          repo_watch_outstanding_dispatch_obligation,
          repo_watch_parked_dispatch_obligation,
          repo_watch_pending_stale_review_clearance,
          repo_watch_webhook_parity
CASCADE;

DROP TABLE repo_watch_achieved_dispatch_settlement,
           convergence_sweep_event,
           convergence_sweep_target,
           repo_watch_complete_poll,
           repo_watch_convergence_cutoff,
           repo_watch_convergence_cutoff_goal,
           repo_watch_current_held_dispatch,
           repo_watch_current_pull_request,
           repo_watch_cursor,
           repo_watch_pull_request_convergence,
           repo_watch_pull_request_convergence_assessment,
           repo_watch_pull_request_convergence_identity,
           repo_watch_current_pull_request_session_count,
           repo_watch_current_pull_request_work_count,
           repo_watch_current_repository_held_count,
           repo_watch_current_repository_obligation_count,
           repo_watch_current_singleton_cooldown,
           repo_watch_dispatch_action,
           repo_watch_dispatch_batch,
           repo_watch_dispatch_delivery,
           repo_watch_dispatch_delivery_intent,
           repo_watch_dispatch_obligation,
           repo_watch_dispatch_obligation_park,
           repo_watch_dispatch_release,
           repo_watch_dispatch_start_lease,
           repo_watch_dispatch_start_lease_expiration,
           repo_watch_dispatch_start_lease_quarantine,
           repo_watch_event,
           repo_watch_headless_approval_escalation,
           repo_watch_lifecycle_cutoff,
           repo_watch_lifecycle_cutoff_goal,
           repo_watch_stale_review_clearance,
           repo_watch_stale_review_clearance_result,
           repo_watch_repository_key,
           repo_watch_rule_activation,
           repo_watch_rule_deactivation,
           repo_watch_rule_evaluation,
           repo_watch_rule_field_fingerprint,
           repo_watch_stale_review_clearance_claim,
           repo_watch_stale_review_clearance_recovery_cursor,
           repo_watch_webhook_delivery,
           repo_watch_webhook_disposition,
           repo_watch_webhook_projection,
           repo_watch_webhook_payload,
           repo_watch_webhook_pending
CASCADE;

DROP FUNCTION convergence_sweep_retry_budget();

DO $drop_repo_watch_functions$
DECLARE
    function_signature regprocedure;
BEGIN
    FOR function_signature IN
        SELECT procedure.oid::regprocedure
          FROM pg_proc AS procedure
          JOIN pg_namespace AS namespace
            ON namespace.oid = procedure.pronamespace
         WHERE namespace.nspname = 'public'
           AND procedure.proname LIKE '%repo_watch%'
    LOOP
        EXECUTE format('DROP FUNCTION %s CASCADE', function_signature);
    END LOOP;
END
$drop_repo_watch_functions$;

CREATE OR REPLACE FUNCTION project_session_lifecycle_from_goal()
RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    authored_kind text;
    authored_module text;
BEGIN
    IF NEW.user_command_id IS NOT NULL THEN
        SELECT command.issuer_kind, command.issuer_module
          INTO STRICT authored_kind, authored_module
          FROM durable_command AS command
         WHERE command.command_id = NEW.user_command_id;
    END IF;
    PERFORM project_session_lifecycle(
        NEW.session_id,
        NEW.event_kind = 'resumed',
        authored_kind,
        authored_module,
        false,
        NEW.event_kind IN ('commissioned', 'resumed', 'superseded')
    );
    RETURN NULL;
END;
$$;

CREATE OR REPLACE FUNCTION assert_failed_terminal_execution_without_cancellation(
    checked_turn_id uuid
) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    lifecycle turn_lifecycle%ROWTYPE;
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM tool_round
         WHERE turn_id = checked_turn_id
    ) THEN
        PERFORM assert_failed_terminal_execution_without_tool_loop(
            checked_turn_id
        );
        RETURN;
    END IF;

    SELECT *
      INTO lifecycle
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id
       AND state_kind = 'terminal'
       AND terminal_disposition_kind = 'failed';
    IF NOT FOUND THEN
        RETURN;
    END IF;

    IF lifecycle.terminal_attempt_id IS NULL
       OR NOT EXISTS (
            SELECT 1
              FROM turn_attempt
             WHERE turn_attempt_id = lifecycle.terminal_attempt_id
               AND turn_id = lifecycle.turn_id
               AND session_id = lifecycle.session_id
               AND state_kind = 'ended'
               AND end_variant = 'without_stop'
               AND end_disposition IN ('known_failure', 'lost')
       )
       OR EXISTS (
            SELECT 1
              FROM turn_attempt
             WHERE turn_id = lifecycle.turn_id
               AND session_id = lifecycle.session_id
               AND turn_attempt_id <> lifecycle.terminal_attempt_id
               AND (
                    state_kind <> 'ended'
                    OR end_variant <> 'without_stop'
                    OR end_disposition <> 'yielded_to_durable_wait'
               )
       )
    THEN
        RAISE EXCEPTION
            'failed tool-loop turn % lacks its exact linear ended attempt',
            checked_turn_id
            USING ERRCODE = '23514';
    END IF;

    IF lifecycle.terminal_model_call_id IS NOT NULL THEN
        IF NOT EXISTS (
            SELECT 1
              FROM model_call
             WHERE model_call_id = lifecycle.terminal_model_call_id
               AND turn_attempt_id = lifecycle.terminal_attempt_id
               AND turn_id = lifecycle.turn_id
               AND session_id = lifecycle.session_id
               AND state_kind = 'terminal'
               AND terminal_disposition_kind IN ('known_failed', 'cancelled')
        ) THEN
            RAISE EXCEPTION
                'failed tool-loop turn % lacks its exact terminal call',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;
        PERFORM assert_model_call_final_state(
            lifecycle.terminal_model_call_id
        );
    ELSIF NOT EXISTS (
        SELECT 1
          FROM tool_attempt
         WHERE issuing_turn_attempt_id = lifecycle.terminal_attempt_id
           AND turn_id = lifecycle.turn_id
           AND session_id = lifecycle.session_id
           AND state_kind = 'terminal'
           AND terminal_disposition_kind = 'known_failed'
           AND error_kind = 'crash_lost'
    ) AND NOT EXISTS (
        SELECT 1
          FROM commissioned_dispatch_headless_approval_escalation AS escalation
          JOIN tool_approval_judge_model_call AS judge
            ON judge.model_call_id = escalation.model_call_id
           AND judge.session_id = escalation.session_id
           AND judge.turn_id = escalation.turn_id
           AND judge.request_id = escalation.request_id
         WHERE escalation.turn_id = lifecycle.turn_id
           AND escalation.session_id = lifecycle.session_id
           AND escalation.terminal_attempt_id = lifecycle.terminal_attempt_id
           AND judge.state_kind = 'terminal'
           AND judge.terminal_disposition_kind = 'completed'
           AND judge.recommendation_kind = 'escalate_to_human'
    ) THEN
        RAISE EXCEPTION
            'failed tool-loop turn % lacks its exact terminal execution cause',
            checked_turn_id
            USING ERRCODE = '23514';
    END IF;
END;
$$;
