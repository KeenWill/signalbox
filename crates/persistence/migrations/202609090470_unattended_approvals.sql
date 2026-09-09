CREATE TABLE tool_approval_human_wait (
    request_id uuid PRIMARY KEY REFERENCES tool_request(request_id),
    deadline timestamp with time zone
);

CREATE FUNCTION tool_request_waits_for_human(checked_request uuid) RETURNS boolean
    LANGUAGE sql STABLE AS $$
    SELECT EXISTS (
        SELECT 1 FROM tool_request AS request
         WHERE request.request_id = checked_request
           AND (request.approval_posture = 'human' OR EXISTS (
               SELECT 1 FROM tool_approval_judge_model_call AS judge
                WHERE judge.request_id = request.request_id AND judge.state_kind = 'terminal'
           ))
           AND NOT EXISTS (SELECT 1 FROM tool_approval_decision AS decision
                WHERE decision.request_id = request.request_id)
    );
$$;

CREATE FUNCTION tool_approval_human_wait_is_due(checked_request uuid) RETURNS boolean
    LANGUAGE sql STABLE AS $$
    SELECT tool_request_waits_for_human(checked_request) AND (
        NOT EXISTS (SELECT 1 FROM tool_approval_human_wait WHERE request_id = checked_request)
        OR EXISTS (SELECT 1 FROM tool_approval_human_wait
            WHERE request_id = checked_request AND deadline <= transaction_timestamp())
    );
$$;

ALTER TABLE tool_approval_decision DROP CONSTRAINT tool_approval_decision_source_shape;
ALTER TABLE tool_approval_decision
    ADD CONSTRAINT tool_approval_decision_source_shape CHECK (
        (decision_source = 'user_command'::text
            AND user_command_id IS NOT NULL
            AND delegate_model_selection_id IS NULL
            AND delegate_model_call_id IS NULL
            AND rationale IS NULL
            AND override_denied_request_id IS NULL)
        OR (decision_source = ANY (ARRAY[
                'policy_auto'::text, 'session_blanket'::text
            ])
            AND decision_kind = 'approve'::text
            AND user_command_id IS NULL
            AND delegate_model_selection_id IS NULL
            AND delegate_model_call_id IS NULL
            AND rationale IS NULL
            AND override_denied_request_id IS NULL)
        OR (decision_source = 'delegate'::text
            AND user_command_id IS NULL
            AND delegate_model_selection_id IS NOT NULL
            AND delegate_model_call_id IS NOT NULL
            AND rationale IS NOT NULL
            AND octet_length(rationale) BETWEEN 1 AND 4096
            AND override_denied_request_id IS NULL)
        OR (decision_source = 'user_override'::text
            AND decision_kind = 'approve'::text
            AND user_command_id IS NULL
            AND delegate_model_selection_id IS NULL
            AND delegate_model_call_id IS NULL
            AND rationale IS NULL
            AND override_denied_request_id IS NOT NULL)
        OR (decision_source = 'runtime_safety'::text
            AND decision_kind = 'deny'::text
            AND denial_reason =
                'Tool arguments were suppressed by the credential boundary'::text
            AND user_command_id IS NULL
            AND delegate_model_selection_id IS NULL
            AND delegate_model_call_id IS NULL
            AND rationale IS NULL
            AND override_denied_request_id IS NULL)
        OR (decision_source = 'runtime_safety'::text
            AND decision_kind = 'deny'::text
            AND denial_reason = 'approval_wait_timeout'::text
            AND user_command_id IS NOT NULL
            AND delegate_model_selection_id IS NULL
            AND delegate_model_call_id IS NULL
            AND rationale IS NULL
            AND override_denied_request_id IS NULL)
        OR (decision_source = 'lifecycle_closure'::text
            AND decision_kind = 'deny'::text
            AND denial_reason IS NULL
            AND user_command_id IS NOT NULL
            AND delegate_model_selection_id IS NULL
            AND delegate_model_call_id IS NULL
            AND rationale IS NULL
            AND override_denied_request_id IS NULL)
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
    IF NEW.decision_source = 'runtime_safety'
       AND NEW.denial_reason = 'approval_wait_timeout' THEN
        SELECT count(*) INTO matched
          FROM tool_approval_human_wait AS waiting
          JOIN durable_command AS command ON command.command_id = NEW.user_command_id
         WHERE waiting.request_id = NEW.request_id
           AND waiting.deadline <= transaction_timestamp()
           AND command.command_kind = 'decide_tool_request'
           AND command.issuer_kind = 'core';
        IF matched <> 1 THEN
            RAISE EXCEPTION 'approval timeout lacks an expired deadline and core command'
                USING ERRCODE = '23514', CONSTRAINT = 'tool_approval_timeout_requires_deadline';
        END IF;
        RETURN NULL;
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
    IF NEW.decision_source = 'user_override' THEN
        SELECT count(*) INTO matched
          FROM tool_request AS request
          JOIN tool_approval_user_override AS recorded
            ON recorded.denied_request_id = NEW.override_denied_request_id
          JOIN model_call_user_override AS frozen
            ON frozen.model_call_id = request.producing_model_call_id
           AND frozen.denied_request_id = recorded.denied_request_id
          JOIN tool_request AS denied_request
            ON denied_request.request_id = recorded.denied_request_id
         WHERE request.request_id = NEW.request_id
           AND request.approval_posture = 'delegated'
           AND recorded.session_id = request.session_id
           AND denied_request.tool_name = request.tool_name
           AND denied_request.arguments_kind = request.arguments_kind
           AND denied_request.arguments_text = request.arguments_text;
        IF matched <> 1 THEN
            RAISE EXCEPTION
                'user override consumption lacks a recorded override for a delegated request'
                USING ERRCODE = '23514',
                      CONSTRAINT =
                          'tool_approval_user_override_requires_recorded_override';
        END IF;
        RETURN NULL;
    END IF;
    IF NEW.decision_source = 'lifecycle_closure' THEN
        SELECT count(*) INTO matched
          FROM decide_tool_request_command AS command
          JOIN durable_command AS durable
            ON durable.command_id = command.command_id
           AND durable.command_kind = command.command_kind
           AND durable.storage_version = command.storage_version
         WHERE command.command_id = NEW.user_command_id
           AND command.request_id = NEW.request_id
           AND command.decision_kind = 'deny'
           AND command.denial_reason IS NULL
           AND command.result_kind = 'applied'
           AND durable.issuer_kind = 'core';
        IF matched <> 1 THEN
            RAISE EXCEPTION 'lifecycle closure denial lacks core authority'
                USING ERRCODE = '23514',
                      CONSTRAINT =
                          'tool_approval_lifecycle_closure_requires_core';
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

CREATE OR REPLACE FUNCTION assert_tool_decision_command_final_state(
    checked_command_id uuid
) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    command_record decide_tool_request_command%ROWTYPE;
    approval_count bigint;
    earliest_correlation_count bigint;
BEGIN
    SELECT *
      INTO command_record
      FROM decide_tool_request_command
     WHERE command_id = checked_command_id;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT count(*)
      INTO approval_count
      FROM tool_approval_decision AS approval
     WHERE approval.user_command_id = checked_command_id
       AND approval.request_id = command_record.request_id
       AND approval.decision_source IN ('user_command', 'lifecycle_closure', 'runtime_safety')
       AND approval.decision_kind = command_record.decision_kind
       AND approval.denial_reason
           IS NOT DISTINCT FROM command_record.denial_reason;

    SELECT count(*)
      INTO earliest_correlation_count
      FROM tool_request AS requested
      JOIN tool_request AS earliest
        ON earliest.request_id =
           command_record.result_earliest_undecided_request_id
       AND earliest.producing_model_call_id =
           requested.producing_model_call_id
       AND earliest.request_ordinal < requested.request_ordinal
     WHERE requested.request_id = command_record.request_id;

    IF command_record.rejection_kind = 'not_earliest_undecided'
       AND earliest_correlation_count <> 1
    THEN
        RAISE EXCEPTION
            'tool decision command names an uncorrelated earlier request'
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'decide_tool_request_command_earliest_correlation';
    END IF;

    IF (
        command_record.result_kind = 'applied'
        AND approval_count <> 1
    ) OR (
        command_record.result_kind = 'rejected'
        AND EXISTS (
            SELECT 1
              FROM tool_approval_decision
             WHERE user_command_id = checked_command_id
        )
    ) THEN
        RAISE EXCEPTION
            'tool decision command lacks its exact approval effect'
            USING ERRCODE = '23514';
    END IF;
END;
$$;
