CREATE TEMPORARY TABLE migrated_web_fetch_attempt ON COMMIT DROP AS
SELECT attempt.attempt_id, attempt.issuing_turn_attempt_id, attempt.turn_id,
       attempt.terminal_disposition_kind = 'ambiguous' AS recovering
  FROM tool_attempt AS attempt
  JOIN tool_request AS request ON request.request_id = attempt.request_id
  JOIN turn_lifecycle AS lifecycle ON lifecycle.turn_id = attempt.turn_id
 WHERE request.tool_name = 'web_fetch'
   AND attempt.effect_class = 'external_effect'
   AND lifecycle.state_kind = 'active'
   AND lifecycle.active_tool_round_call_id = request.producing_model_call_id
   AND (
       attempt.state_kind IN ('prepared', 'in_flight')
       OR (
           lifecycle.active_phase_kind = 'awaiting_tool_recovery'
           AND lifecycle.recovery_tool_attempt_id = attempt.attempt_id
           AND attempt.terminal_disposition_kind = 'ambiguous'
       )
   );

ALTER TABLE tool_attempt DISABLE TRIGGER tool_attempt_changes_are_guarded;
ALTER TABLE turn_attempt DISABLE TRIGGER turn_attempt_changes_are_guarded;
ALTER TABLE turn_lifecycle DISABLE TRIGGER turn_lifecycle_changes_are_guarded;

UPDATE tool_attempt AS attempt
   SET effect_class = 'effect_free',
       terminal_disposition_kind = CASE WHEN migrated.recovering THEN 'known_failed'
                                       ELSE attempt.terminal_disposition_kind END,
       error_kind = CASE WHEN migrated.recovering THEN 'execution_failed'
                         ELSE attempt.error_kind END,
       error_detail = CASE WHEN migrated.recovering THEN 'web fetch request failed'
                           ELSE attempt.error_detail END,
       context_error_detail = CASE WHEN migrated.recovering THEN 'web fetch request failed'
                                   ELSE attempt.context_error_detail END
  FROM migrated_web_fetch_attempt AS migrated
 WHERE attempt.attempt_id = migrated.attempt_id;

UPDATE turn_attempt AS attempt
   SET state_kind = 'running', end_variant = NULL, end_disposition = NULL
  FROM migrated_web_fetch_attempt AS migrated
 WHERE migrated.recovering
   AND attempt.turn_attempt_id = migrated.issuing_turn_attempt_id;

UPDATE turn_lifecycle AS lifecycle
   SET active_phase_kind = 'running', recovery_tool_attempt_id = NULL
  FROM migrated_web_fetch_attempt AS migrated
 WHERE migrated.recovering
   AND lifecycle.turn_id = migrated.turn_id;

DELETE FROM automatic_reconciliation_attempt AS attempt
 USING automatic_reconciliation AS recovery, migrated_web_fetch_attempt AS migrated
 WHERE migrated.recovering
   AND recovery.tool_attempt_id = migrated.attempt_id
   AND recovery.turn_id = attempt.turn_id;

DELETE FROM automatic_reconciliation AS recovery
 USING migrated_web_fetch_attempt AS migrated
 WHERE migrated.recovering
   AND recovery.tool_attempt_id = migrated.attempt_id;

SET CONSTRAINTS ALL IMMEDIATE;
ALTER TABLE tool_attempt ENABLE TRIGGER tool_attempt_changes_are_guarded;
ALTER TABLE turn_attempt ENABLE TRIGGER turn_attempt_changes_are_guarded;
ALTER TABLE turn_lifecycle ENABLE TRIGGER turn_lifecycle_changes_are_guarded;
