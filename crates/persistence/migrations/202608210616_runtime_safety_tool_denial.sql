-- A provider credential boundary may suppress a tool argument object as a
-- whole. Preserve that event as a non-executable logical request whose fixed
-- automatic denial lets the same turn continue; the source and safe sentinel
-- jointly prove that no caller or policy granted execution authority.

ALTER TABLE tool_approval_decision
    DROP CONSTRAINT tool_approval_decision_source_closed,
    DROP CONSTRAINT tool_approval_decision_source_shape,
    ADD CONSTRAINT tool_approval_decision_source_closed
        CHECK (
            decision_source IN (
                'user_command',
                'policy_auto',
                'session_blanket',
                'delegate',
                'runtime_safety'
            )
        ),
    ADD CONSTRAINT tool_approval_decision_source_shape
        CHECK (
            (
                decision_source = 'user_command'
                AND user_command_id IS NOT NULL
                AND delegate_model_selection_id IS NULL
                AND delegate_model_call_id IS NULL
                AND rationale IS NULL
            )
            OR (
                decision_source IN ('policy_auto', 'session_blanket')
                AND decision_kind = 'approve'
                AND user_command_id IS NULL
                AND delegate_model_selection_id IS NULL
                AND delegate_model_call_id IS NULL
                AND rationale IS NULL
            )
            OR (
                decision_source = 'delegate'
                AND user_command_id IS NULL
                AND delegate_model_selection_id IS NOT NULL
                AND delegate_model_call_id IS NOT NULL
                AND rationale IS NOT NULL
                AND octet_length(rationale) BETWEEN 1 AND 4096
            )
            OR (
                decision_source = 'runtime_safety'
                AND decision_kind = 'deny'
                AND denial_reason =
                    'Tool arguments were suppressed by the credential boundary'
                AND user_command_id IS NULL
                AND delegate_model_selection_id IS NULL
                AND delegate_model_call_id IS NULL
                AND rationale IS NULL
            )
        );

CREATE OR REPLACE FUNCTION require_tool_approval_decision_authority()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    matched bigint;
BEGIN
    PERFORM 1
       FROM tool_request
      WHERE request_id = NEW.request_id
        FOR UPDATE;
    IF EXISTS (
        SELECT 1
          FROM tool_approval_judge_model_call AS judge
         WHERE judge.request_id = NEW.request_id
           AND judge.state_kind <> 'terminal'
    ) THEN
        RAISE EXCEPTION 'approval decision races an unfinished judge call'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      'tool_approval_decision_requires_terminal_judge';
    END IF;
    IF NEW.decision_source = 'runtime_safety' THEN
        SELECT count(*) INTO matched
          FROM tool_request
         WHERE request_id = NEW.request_id
           AND approval_posture = 'auto'
           AND arguments_kind = 'json'
           AND arguments_text = '{"redacted":"[redacted]"}';
        IF matched <> 1 THEN
            RAISE EXCEPTION 'runtime safety denial lacks suppressed arguments'
                USING ERRCODE = '23514',
                      CONSTRAINT =
                          'tool_approval_runtime_safety_requires_suppressed_arguments';
        END IF;
        RETURN NULL;
    END IF;
    IF NEW.decision_source IN ('policy_auto', 'session_blanket') THEN
        SELECT count(*) INTO matched
          FROM tool_request
         WHERE request_id = NEW.request_id
           AND approval_posture = 'auto';
        IF matched <> 1 THEN
            RAISE EXCEPTION 'automatic decision exceeds frozen posture'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'tool_approval_automatic_requires_auto_posture';
        END IF;
        RETURN NULL;
    END IF;
    IF NEW.decision_source = 'user_command' THEN
        SELECT count(*) INTO matched
          FROM tool_request AS request
         WHERE request.request_id = NEW.request_id
           AND (
                request.approval_posture = 'human'
                OR (
                    request.approval_posture = 'delegated'
                    AND EXISTS (
                        SELECT 1
                          FROM tool_approval_judge_model_call AS judge
                         WHERE judge.request_id = request.request_id
                           AND judge.state_kind = 'terminal'
                           AND (
                                (
                                    judge.terminal_disposition_kind = 'completed'
                                    AND judge.recommendation_kind =
                                        'escalate_to_human'
                                )
                                OR judge.terminal_disposition_kind IN (
                                    'known_failed', 'refused', 'cancelled',
                                    'ambiguous'
                                )
                           )
                    )
                )
           );
        IF matched <> 1 THEN
            RAISE EXCEPTION 'user decision lacks human approval authority'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'tool_approval_user_requires_human_authority';
        END IF;
        RETURN NULL;
    END IF;
    IF NEW.decision_source <> 'delegate' THEN
        RETURN NULL;
    END IF;
    SELECT count(*) INTO matched
      FROM tool_request AS request
      JOIN tool_approval_judge_model_call AS judge
        ON judge.request_id = request.request_id
     WHERE request.request_id = NEW.request_id
       AND request.approval_posture = 'delegated'
       AND judge.model_call_id = NEW.delegate_model_call_id
       AND judge.direct_model_selection_id = NEW.delegate_model_selection_id
       AND judge.state_kind = 'terminal'
       AND judge.terminal_disposition_kind = 'completed'
       AND judge.recommendation_kind = NEW.decision_kind
       AND judge.rationale = NEW.rationale
       AND NOT EXISTS (
            SELECT 1 FROM tool_request AS earlier
            LEFT JOIN tool_approval_decision AS earlier_decision
              ON earlier_decision.request_id = earlier.request_id
           WHERE earlier.producing_model_call_id = request.producing_model_call_id
             AND earlier.request_ordinal < request.request_ordinal
             AND earlier_decision.request_id IS NULL
       );
    IF matched <> 1 THEN
        RAISE EXCEPTION 'delegate decision lacks matching delegated authority'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'tool_approval_delegate_requires_checked_judge';
    END IF;
    RETURN NULL;
END;
$$;

CREATE OR REPLACE FUNCTION require_explicit_tool_approval_effect()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    matching_effects bigint;
BEGIN
    IF NEW.decision_source IN (
        'policy_auto', 'session_blanket', 'runtime_safety'
    ) THEN
        RETURN NULL;
    END IF;

    SELECT count(*)
      INTO matching_effects
      FROM tool_approval_decided_outbox_event AS dispatched
      JOIN tool_request AS request
        ON request.request_id = dispatched.request_id
      JOIN turn_lifecycle AS lifecycle
        ON lifecycle.turn_id = request.turn_id
       AND lifecycle.session_id = request.session_id
     WHERE dispatched.request_id = NEW.request_id
       AND lifecycle.active_tool_round_call_id =
           request.producing_model_call_id
       AND (
            SELECT count(*)
              FROM tool_approval_decided_outbox_event AS transaction_event
              JOIN tool_approval_decision AS transaction_decision
                ON transaction_decision.request_id =
                   transaction_event.request_id
              JOIN tool_request AS transaction_request
                ON transaction_request.request_id =
                   transaction_decision.request_id
             WHERE transaction_request.producing_model_call_id =
                   request.producing_model_call_id
               AND transaction_decision.decision_source NOT IN (
                    'policy_auto', 'session_blanket', 'runtime_safety'
               )
               AND transaction_event.recording_transaction_id =
                   dispatched.recording_transaction_id
       ) = 1
       AND NOT EXISTS (
            SELECT 1
              FROM tool_request AS earlier
              LEFT JOIN tool_approval_decision AS earlier_decision
                ON earlier_decision.request_id = earlier.request_id
             WHERE earlier.producing_model_call_id =
                   request.producing_model_call_id
               AND earlier.request_ordinal < request.request_ordinal
               AND earlier_decision.request_id IS NULL
       )
       AND (
            (
                lifecycle.state_kind = 'active'
                AND lifecycle.active_phase_kind = 'awaiting_tool_approval'
                AND lifecycle.approval_tool_request_id = (
                    SELECT later.request_id
                      FROM tool_request AS later
                      LEFT JOIN tool_approval_decision AS later_decision
                        ON later_decision.request_id = later.request_id
                     WHERE later.producing_model_call_id =
                           request.producing_model_call_id
                       AND later_decision.request_id IS NULL
                     ORDER BY later.request_ordinal
                     LIMIT 1
                )
            )
            OR
            (
                lifecycle.state_kind = 'active'
                AND lifecycle.active_phase_kind = 'running'
                AND lifecycle.approval_tool_request_id IS NULL
                AND lifecycle.recovery_tool_attempt_id IS NULL
                AND NOT EXISTS (
                    SELECT 1
                      FROM tool_request AS undecided
                      LEFT JOIN tool_approval_decision AS undecided_decision
                        ON undecided_decision.request_id = undecided.request_id
                     WHERE undecided.producing_model_call_id =
                           request.producing_model_call_id
                       AND undecided_decision.request_id IS NULL
                )
                AND EXISTS (
                    SELECT 1
                      FROM turn_attempt AS successor
                      JOIN model_call AS producing_call
                        ON producing_call.model_call_id =
                           request.producing_model_call_id
                       AND producing_call.turn_id = request.turn_id
                       AND producing_call.session_id = request.session_id
                     WHERE successor.turn_attempt_id =
                           lifecycle.current_attempt_id
                       AND successor.turn_id = request.turn_id
                       AND successor.session_id = request.session_id
                       AND successor.continued_from_attempt_id =
                           producing_call.turn_attempt_id
                       AND successor.state_kind = 'prepared'
                )
            )
       );
    IF matching_effects <> 1 THEN
        RAISE EXCEPTION
            'explicit decision lacks its outbox and lifecycle effect'
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'tool_approval_explicit_requires_atomic_effect';
    END IF;
    RETURN NULL;
END;
$$;
