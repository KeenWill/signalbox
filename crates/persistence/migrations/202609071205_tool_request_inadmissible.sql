ALTER TABLE tool_request
    ADD COLUMN resolution_kind text,
    ADD COLUMN inadmissible_reason text,
    ADD CONSTRAINT tool_request_inadmissible_shape CHECK (
        (resolution_kind IS NULL AND inadmissible_reason IS NULL)
        OR (resolution_kind IS NOT NULL AND inadmissible_reason IS NOT NULL AND resolution_kind = 'closed_inadmissible' AND inadmissible_reason = 'placement_lost')
    );

CREATE FUNCTION guard_tool_request_resolution() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' OR (TG_OP = 'UPDATE' AND (
        OLD.inadmissible_reason IS NOT NULL
        OR (to_jsonb(OLD) - 'resolution_kind' - 'inadmissible_reason')
            IS DISTINCT FROM (to_jsonb(NEW) - 'resolution_kind' - 'inadmissible_reason')
    )) THEN
        RAISE EXCEPTION 'tool request content and terminal resolution are immutable' USING ERRCODE = '23514';
    END IF;
    IF NEW.inadmissible_reason IS NOT NULL THEN
        IF NOT EXISTS (SELECT 1 FROM runner_current_session_placement AS head
            JOIN runner_session_placement_record AS placement USING (session_id, event_ordinal)
            WHERE head.session_id = NEW.session_id AND placement.state_kind IN ('runner_lost', 'runner_lost_before_pin'))
           OR EXISTS (SELECT 1 FROM runner_tool_request_lease_binding WHERE request_id = NEW.request_id)
           OR EXISTS (SELECT 1 FROM tool_attempt WHERE request_id = NEW.request_id
               AND (state_kind <> 'terminal' OR terminal_disposition_kind <> 'known_failed'
                    OR error_kind <> 'execution_failed' OR error_detail <> 'placement_lost'))
           OR EXISTS (SELECT 1 FROM tool_approval_judge_model_call WHERE request_id = NEW.request_id AND state_kind <> 'terminal') THEN
            RAISE EXCEPTION 'placement loss closure requires retired pre-dispatch work' USING ERRCODE = '23514';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;
DROP TRIGGER tool_request_is_append_only ON tool_request;
CREATE TRIGGER tool_request_resolution_guard BEFORE INSERT OR UPDATE OR DELETE ON tool_request
    FOR EACH ROW EXECUTE FUNCTION guard_tool_request_resolution();

DO $$
DECLARE
    definition text;
    name text;
BEGIN
    FOREACH name IN ARRAY ARRAY['semantic_transcript_entry_payload_kind_closed', 'semantic_transcript_entry_payload_shape'] LOOP
        SELECT pg_get_constraintdef(oid) INTO STRICT definition FROM pg_constraint
            WHERE conrelid = 'semantic_transcript_entry'::regclass AND conname = name;
        definition := replace(definition, '''tool_denied''::text,', '''tool_inadmissible''::text, ''tool_denied''::text,');
        EXECUTE format('ALTER TABLE semantic_transcript_entry DROP CONSTRAINT %I', name);
        EXECUTE format('ALTER TABLE semantic_transcript_entry ADD CONSTRAINT %I %s', name, definition);
    END LOOP;
    SELECT pg_get_viewdef('runner_current_tool_attempt'::regclass, true) INTO definition;
    definition := regexp_replace(definition, ';\s*$', '');
    EXECUTE 'CREATE OR REPLACE VIEW runner_current_tool_attempt AS SELECT current.* FROM (' || definition || ') AS current
        JOIN tool_request AS request USING (request_id) WHERE request.inadmissible_reason IS NULL';
END;
$$;

DO $$
DECLARE
    function record;
    definition text;
BEGIN
    FOR function IN SELECT oid FROM pg_proc WHERE pronamespace = 'public'::regnamespace
        AND prokind = 'f' AND prosrc LIKE '%''tool_closed_by_turn_end''%'
    LOOP
        definition := pg_get_functiondef(function.oid);
        definition := regexp_replace(definition, '''tool_denied'',\s*''tool_closed_by_turn_end''', '''tool_inadmissible'', ''tool_denied'', ''tool_closed_by_turn_end''', 'g');
        EXECUTE definition;
    END LOOP;
    SELECT pg_get_functiondef('assert_model_call_steering_final_state(uuid)'::regprocedure) INTO definition;
    definition := replace(definition, '''tool_denied'', ''delegation_result''', '''tool_inadmissible'', ''tool_denied'', ''delegation_result''');
    EXECUTE definition;
    SELECT pg_get_functiondef('require_semantic_entry_turn_state()'::regprocedure) INTO definition;
    definition := replace(definition, 'WHEN ''tool_closed_by_turn_end'' THEN',
        'WHEN ''tool_inadmissible'' THEN
            SELECT request.turn_id INTO checked_turn_id FROM tool_request AS request
            WHERE request.request_id = entry.tool_result_request_id
              AND request.session_id = entry.source_session_id
              AND request.resolution_kind = ''closed_inadmissible'';
        WHEN ''tool_closed_by_turn_end'' THEN');
    EXECUTE definition;
    SELECT pg_get_functiondef('reject_tool_attempt_invalid_change()'::regprocedure) INTO definition;
    definition := replace(definition, 'IF OLD.state_kind = ''prepared''
       AND NEW.state_kind = ''terminal''', 'IF OLD.state_kind = ''prepared''
       AND NEW.state_kind = ''terminal''
       AND NOT (NEW.terminal_disposition_kind = ''known_failed'' AND NEW.error_kind = ''execution_failed'' AND NEW.error_detail = ''placement_lost'')');
    EXECUTE definition;
    SELECT pg_get_functiondef('assert_tool_loop_turn_final_state_pre_delegation(uuid)'::regprocedure) INTO definition;
    definition := replace(definition, 'AND approval.request_id IS NULL', 'AND approval.request_id IS NULL AND request.inadmissible_reason IS NULL');
    -- The approval-wait query uses its own request alias.
    definition := replace(definition, 'AND waiting.producing_model_call_id =', 'AND waiting.inadmissible_reason IS NULL AND waiting.producing_model_call_id =');
    definition := replace(definition, 'AND approval.request_id IS NULL AND request.inadmissible_reason IS NULL
                           AND NOT EXISTS', 'AND approval.request_id IS NULL
                           AND NOT EXISTS');
    definition := replace(definition, 'AND earlier_approval.request_id IS NULL', 'AND earlier_approval.request_id IS NULL AND earlier.inadmissible_reason IS NULL');
    definition := replace(definition, 'WHERE request.producing_model_call_id =', 'WHERE request.inadmissible_reason IS NULL AND request.producing_model_call_id =');
    EXECUTE definition;
END;
$$;

CREATE FUNCTION require_inadmissible_tool_result() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    request uuid;
    reason text;
BEGIN
    request := NEW.tool_result_request_id;
    IF request IS NULL AND NEW.tool_result_attempt_id IS NOT NULL THEN
        SELECT request_id INTO request FROM tool_attempt WHERE attempt_id = NEW.tool_result_attempt_id;
    END IF;
    SELECT inadmissible_reason INTO reason FROM tool_request WHERE request_id = request;
    IF (NEW.payload_kind = 'tool_inadmissible') IS DISTINCT FROM (reason IS NOT NULL) THEN
        RAISE EXCEPTION 'tool result must preserve the request resolution' USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER semantic_tool_result_preserves_inadmissibility
    AFTER INSERT ON semantic_transcript_entry DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    WHEN (NEW.tool_result_request_id IS NOT NULL OR NEW.tool_result_attempt_id IS NOT NULL)
    EXECUTE FUNCTION require_inadmissible_tool_result();

CREATE FUNCTION require_prepared_placement_loss_resolution() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM tool_request WHERE request_id = NEW.request_id
        AND inadmissible_reason = 'placement_lost') THEN
        RAISE EXCEPTION 'prepared placement-loss retirement requires request closure' USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER prepared_placement_loss_requires_request_closure
    AFTER UPDATE ON tool_attempt DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    WHEN (OLD.state_kind = 'prepared' AND NEW.state_kind = 'terminal'
        AND NEW.error_kind = 'execution_failed' AND NEW.error_detail = 'placement_lost')
    EXECUTE FUNCTION require_prepared_placement_loss_resolution();
