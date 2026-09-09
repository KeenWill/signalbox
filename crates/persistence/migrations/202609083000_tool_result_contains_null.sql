ALTER TABLE tool_attempt
    DROP CONSTRAINT tool_attempt_error_kind_closed,
    ADD CONSTRAINT tool_attempt_error_kind_closed CHECK (
        error_kind IS NULL OR error_kind IN (
            'unknown_tool', 'invalid_arguments', 'preauthorization_rejected',
            'execution_failed', 'result_too_large', 'result_contains_null', 'crash_lost'
        )
    );

ALTER TABLE tool_batch_transition_detail_member
    DROP CONSTRAINT tool_batch_transition_detail_member_attempt_error_closed,
    ADD CONSTRAINT tool_batch_transition_detail_member_attempt_error_closed CHECK (
        attempt_error_kind IS NULL OR attempt_error_kind IN (
            'unknown_tool', 'invalid_arguments', 'preauthorization_rejected',
            'execution_failed', 'result_too_large', 'result_contains_null', 'crash_lost'
        )
    );

CREATE OR REPLACE FUNCTION session_plan_event_has_authority(candidate session_plan_event) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT EXISTS (
        SELECT 1
          FROM tool_attempt AS attempt
          JOIN tool_request AS request
            ON request.request_id = attempt.request_id
         WHERE attempt.attempt_id = candidate.provenance_attempt_id
           AND attempt.request_id = candidate.provenance_request_id
           AND attempt.issuing_turn_attempt_id =
                candidate.provenance_issuing_turn_attempt_id
           AND attempt.dispatch_generation =
                candidate.provenance_dispatch_generation
           AND attempt.turn_id = candidate.provenance_turn_id
           AND attempt.session_id = candidate.session_id
           AND attempt.effect_class = 'external_effect'
           AND (
                attempt.state_kind = 'in_flight'
                OR (
                    attempt.state_kind = 'terminal'
                    AND (
                        attempt.terminal_disposition_kind IN (
                            'completed', 'ambiguous'
                        )
                        OR (
                            attempt.terminal_disposition_kind = 'known_failed'
                            AND attempt.error_kind IN (
                                'execution_failed', 'result_too_large',
                                'result_contains_null'
                            )
                        )
                    )
                )
           )
           AND request.request_id = candidate.provenance_request_id
           AND request.session_id = candidate.session_id
           AND request.turn_id = candidate.provenance_turn_id
           AND request.tool_name = 'plan_write'
           AND request.arguments_kind = 'json'
           AND session_plan_request_arguments_json(
                   request.arguments_kind, request.arguments_text
               ) =
                CASE candidate.event_kind
                    WHEN 'created' THEN jsonb_build_object(
                        'kind', 'create',
                        'text', candidate.entry_text
                    )
                    WHEN 'text_revised' THEN jsonb_build_object(
                        'kind', 'revise',
                        'entry_id', candidate.entry_ordinal,
                        'text', candidate.entry_text
                    )
                    WHEN 'status_changed' THEN jsonb_build_object(
                        'kind', 'set_status',
                        'entry_id', candidate.entry_ordinal,
                        'status', candidate.entry_status
                    )
                    WHEN 'depends_on' THEN jsonb_build_object(
                        'kind', 'depends_on',
                        'entry_id', candidate.entry_ordinal,
                        'dependency_id', candidate.dependency_ordinal
                    )
                END
    );
$$;
