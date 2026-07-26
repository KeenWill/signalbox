-- Forward-only mid-session model selection.
--
-- Session defaults versions are already append-only epochs, and accepted
-- origins already freeze the epoch current at acceptance. This migration adds
-- the semantic boundary that records an actual model-identity transition in
-- the next started turn's frontier.

ALTER TABLE turn_lifecycle
    ADD COLUMN model_identity_boundary_required boolean NOT NULL DEFAULT false;

-- Existing started turns predate the boundary law and retain their historical
-- frontiers. Existing queued turns can still start after this migration, so
-- they and every newly inserted turn require the boundary.
UPDATE turn_lifecycle
   SET model_identity_boundary_required = true
 WHERE state_kind = 'queued';

-- Drain the backfill's deferred lifecycle checks before the next ALTER TABLE.
SET CONSTRAINTS ALL IMMEDIATE;
SET CONSTRAINTS ALL DEFERRED;

ALTER TABLE turn_lifecycle
    ALTER COLUMN model_identity_boundary_required SET DEFAULT true;

DO $$
DECLARE
    definition text;
    revised text;
BEGIN
    definition := pg_get_functiondef(
        'reject_turn_lifecycle_invalid_change()'::regprocedure
    );
    revised := replace(
        definition,
        'IF OLD.state_kind = ''terminal'' THEN',
        'IF OLD.model_identity_boundary_required IS DISTINCT FROM
              NEW.model_identity_boundary_required
        THEN
            RAISE EXCEPTION
                ''turn model-identity boundary requirement is write-once''
                USING ERRCODE = ''23514'';
        END IF;

        IF OLD.state_kind = ''terminal'' THEN'
    );
    IF revised = definition THEN
        RAISE EXCEPTION
            'unable to make the model-identity boundary requirement immutable';
    END IF;
    EXECUTE revised;
END;
$$;

ALTER TABLE semantic_transcript_entry
    ADD COLUMN model_identity_turn_id uuid,
    ADD COLUMN model_identity_defaults_version numeric(20, 0),
    ADD COLUMN model_identity_direct_selection_id uuid;

DO $$
DECLARE
    legacy_shape text;
BEGIN
    SELECT pg_get_expr(constraint_record.conbin, constraint_record.conrelid)
      INTO legacy_shape
      FROM pg_constraint AS constraint_record
     WHERE constraint_record.conrelid =
               'semantic_transcript_entry'::regclass
       AND constraint_record.conname =
               'semantic_transcript_entry_payload_shape';

    IF legacy_shape IS NULL THEN
        RAISE EXCEPTION 'semantic transcript legacy payload shape is missing';
    END IF;

    ALTER TABLE semantic_transcript_entry
        DROP CONSTRAINT semantic_transcript_entry_payload_kind_closed,
        DROP CONSTRAINT semantic_transcript_entry_payload_shape;

    ALTER TABLE semantic_transcript_entry
        ADD CONSTRAINT semantic_transcript_entry_payload_kind_closed
            CHECK (
                payload_kind IN (
                    'imported_entry',
                    'origin_accepted_input',
                    'steering_accepted_input',
                    'model_identity_changed',
                    'turn_failed',
                    'assistant_text',
                    'assistant_tool_use',
                    'tool_execution_result',
                    'tool_denied',
                    'tool_closed_by_turn_end',
                    'turn_completed',
                    'turn_cancelled'
                )
            );

    EXECUTE format(
        'ALTER TABLE semantic_transcript_entry
             ADD CONSTRAINT semantic_transcript_entry_payload_shape
             CHECK (
                 (
                     payload_kind = ''model_identity_changed''
                     AND model_identity_turn_id IS NOT NULL
                     AND model_identity_defaults_version IS NOT NULL
                     AND model_identity_direct_selection_id IS NOT NULL
                     AND origin_accepted_input_id IS NULL
                     AND steering_source_turn_id IS NULL
                     AND failed_turn_id IS NULL
                     AND cancelled_turn_id IS NULL
                     AND assistant_text_value IS NULL
                     AND producing_model_call_id IS NULL
                     AND assistant_tool_request_id IS NULL
                     AND tool_result_request_id IS NULL
                     AND tool_result_attempt_id IS NULL
                     AND completed_turn_id IS NULL
                     AND imported_conversation_id IS NULL
                     AND imported_transcript_entry_id IS NULL
                     AND assistant_response_part_ordinal IS NULL
                 )
                 OR (
                     payload_kind <> ''model_identity_changed''
                     AND model_identity_turn_id IS NULL
                     AND model_identity_defaults_version IS NULL
                     AND model_identity_direct_selection_id IS NULL
                     AND (%s)
                 )
             )',
        legacy_shape
    );
END;
$$;

ALTER TABLE semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_model_identity_version_positive_u64
        CHECK (
            model_identity_defaults_version IS NULL
            OR (
                model_identity_defaults_version >= 1
                AND model_identity_defaults_version <= 18446744073709551615
            )
        ),
    ADD CONSTRAINT semantic_transcript_entry_model_identity_turn_once
        UNIQUE (model_identity_turn_id),
    ADD CONSTRAINT semantic_transcript_entry_model_identity_turn_fk
        FOREIGN KEY (model_identity_turn_id, source_session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION turn_start_model_identity_entry_count(
    checked_turn_id uuid,
    checked_frontier_id uuid
)
RETURNS bigint
LANGUAGE sql
STABLE
AS $$
    SELECT count(*)
      FROM semantic_transcript_entry AS entry
      JOIN context_frontier_member AS member
        ON member.source_session_id = entry.source_session_id
       AND member.semantic_entry_id = entry.semantic_entry_id
     WHERE entry.model_identity_turn_id = checked_turn_id
       AND member.context_frontier_id = checked_frontier_id
       AND member.owning_session_id = entry.source_session_id
$$;

CREATE FUNCTION turn_origin_effective_model_configuration(
    checked_turn_id uuid,
    checked_session_id uuid
)
RETURNS TABLE (
    defaults_version numeric(20, 0),
    direct_selection_id uuid
)
LANGUAGE sql
STABLE
AS $$
    WITH RECURSIVE configuration_chain AS (
        SELECT
            origin.turn_id,
            origin.source_configuration_turn_id,
            origin.defaults_version,
            COALESCE(
                origin.frozen_direct_model_selection_id,
                origin.frozen_alias_selected_direct_id
            ) AS direct_selection_id,
            ARRAY[origin.turn_id]::uuid[] AS visited_turn_ids
          FROM queued_input_origin AS origin
         WHERE origin.turn_id = checked_turn_id
           AND origin.session_id = checked_session_id

        UNION ALL

        SELECT
            source.turn_id,
            source.source_configuration_turn_id,
            source.defaults_version,
            COALESCE(
                source.frozen_direct_model_selection_id,
                source.frozen_alias_selected_direct_id
            ),
            chain.visited_turn_ids || source.turn_id
          FROM configuration_chain AS chain
          JOIN queued_input_origin AS source
            ON source.turn_id = chain.source_configuration_turn_id
           AND source.session_id = checked_session_id
         WHERE NOT source.turn_id = ANY(chain.visited_turn_ids)
    )
    SELECT chain.defaults_version, chain.direct_selection_id
      FROM configuration_chain AS chain
     WHERE chain.defaults_version IS NOT NULL
       AND chain.direct_selection_id IS NOT NULL
     ORDER BY cardinality(chain.visited_turn_ids) DESC
     LIMIT 1
$$;

CREATE FUNCTION turn_start_model_identity_boundary_is_valid(
    checked_turn_id uuid,
    checked_frontier_id uuid
)
RETURNS boolean
LANGUAGE plpgsql
STABLE
AS $$
DECLARE
    checked_session uuid;
    checked_defaults_version numeric(20, 0);
    checked_selection uuid;
    boundary_required boolean;
    predecessor_turn uuid;
    predecessor_selection uuid;
    starting_member_count numeric(20, 0);
    boundary_entry_count bigint;
    boundary_member_count bigint;
    boundary_member_position numeric(20, 0);
BEGIN
    SELECT origin.session_id, lifecycle.model_identity_boundary_required
      INTO checked_session, boundary_required
      FROM queued_input_origin AS origin
      JOIN turn_lifecycle AS lifecycle
        ON lifecycle.turn_id = origin.turn_id
       AND lifecycle.session_id = origin.session_id
     WHERE origin.turn_id = checked_turn_id;

    IF NOT FOUND THEN
        RETURN false;
    END IF;

    SELECT
        effective.defaults_version,
        effective.direct_selection_id
      INTO checked_defaults_version, checked_selection
      FROM turn_origin_effective_model_configuration(
               checked_turn_id,
               checked_session
           ) AS effective;

    IF NOT FOUND THEN
        RETURN false;
    END IF;

    predecessor_turn := accepted_input_turn_queue_predecessor(
        checked_session,
        checked_turn_id
    );
    IF predecessor_turn IS NOT NULL THEN
        SELECT effective.direct_selection_id
          INTO predecessor_selection
          FROM turn_origin_effective_model_configuration(
                   predecessor_turn,
                   checked_session
               ) AS effective;
        IF NOT FOUND THEN
            RETURN false;
        END IF;
    END IF;

    SELECT count(*)
      INTO boundary_entry_count
      FROM semantic_transcript_entry AS entry
     WHERE entry.source_session_id = checked_session
       AND entry.payload_kind = 'model_identity_changed'
       AND entry.model_identity_turn_id = checked_turn_id
       AND entry.model_identity_defaults_version = checked_defaults_version
       AND entry.model_identity_direct_selection_id = checked_selection;

    IF NOT boundary_required THEN
        RETURN boundary_entry_count = 0;
    END IF;

    IF predecessor_turn IS NULL
       OR predecessor_selection IS NOT DISTINCT FROM checked_selection
    THEN
        RETURN boundary_entry_count = 0;
    END IF;

    SELECT member_count
      INTO starting_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session
       AND context_frontier_id = checked_frontier_id;

    SELECT count(*), max(member.member_position)
      INTO boundary_member_count, boundary_member_position
      FROM semantic_transcript_entry AS entry
      JOIN context_frontier_member AS member
        ON member.source_session_id = entry.source_session_id
       AND member.semantic_entry_id = entry.semantic_entry_id
     WHERE entry.source_session_id = checked_session
       AND entry.payload_kind = 'model_identity_changed'
       AND entry.model_identity_turn_id = checked_turn_id
       AND member.owning_session_id = checked_session
       AND member.context_frontier_id = checked_frontier_id;

    RETURN boundary_entry_count = 1
       AND boundary_member_count = 1
       AND boundary_member_position IS NOT DISTINCT FROM
           starting_member_count - 1;
END;
$$;

DO $$
DECLARE
    definition text;
    revised text;
BEGIN
    definition := pg_get_functiondef(
        'assert_turn_lifecycle_final_state_without_steering(uuid)'::regprocedure
    );
    revised := replace(
        definition,
        'IF origin_member_count <> 1
       OR origin_member_position IS DISTINCT FROM last_member_position',
        'IF origin_member_count <> 1
       OR origin_member_position IS DISTINCT FROM last_member_position
       OR NOT turn_start_model_identity_boundary_is_valid(
            checked_turn_id,
            checked_starting_frontier
       )'
    );
    IF revised = definition THEN
        RAISE EXCEPTION
            'unable to extend accepted-input model-identity boundary law';
    END IF;
    definition := revised;
    revised := replace(
        definition,
        'predecessor_terminal_member_count + 1',
        'predecessor_terminal_member_count + 1
               + turn_start_model_identity_entry_count(
                    checked_turn_id,
                    checked_starting_frontier
               )'
    );
    IF revised = definition THEN
        RAISE EXCEPTION
            'unable to extend accepted-input model-identity frontier count';
    END IF;
    EXECUTE revised;

    definition := pg_get_functiondef(
        'assert_terminal_started_turn_common_final_state(uuid)'::regprocedure
    );
    revised := replace(
        definition,
        'OR origin_member_position IS DISTINCT FROM starting_member_count',
        'OR origin_member_position IS DISTINCT FROM starting_member_count
       OR NOT turn_start_model_identity_boundary_is_valid(
            checked_turn_id,
            checked_starting_frontier
       )'
    );
    IF revised = definition THEN
        RAISE EXCEPTION
            'unable to extend terminal model-identity boundary law';
    END IF;
    definition := revised;
    revised := replace(
        definition,
        'predecessor_member_count + 1',
        'predecessor_member_count + 1
               + turn_start_model_identity_entry_count(
                    checked_turn_id,
                    checked_starting_frontier
               )'
    );
    IF revised = definition THEN
        RAISE EXCEPTION
            'unable to extend terminal model-identity frontier count';
    END IF;
    EXECUTE revised;
END;
$$;

CREATE OR REPLACE FUNCTION require_semantic_entry_turn_state()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    entry semantic_transcript_entry%ROWTYPE;
    checked_turn_id uuid;
    checked_producing_call_id uuid;
BEGIN
    IF TG_OP = 'DELETE' THEN
        entry := OLD;
    ELSE
        entry := NEW;
    END IF;
    checked_producing_call_id := entry.producing_model_call_id;

    IF entry.payload_kind = 'imported_entry' THEN
        RETURN NULL;
    END IF;

    CASE entry.payload_kind
        WHEN 'model_identity_changed' THEN
            SELECT origin.turn_id
              INTO checked_turn_id
              FROM queued_input_origin AS origin
              JOIN LATERAL turn_origin_effective_model_configuration(
                   origin.turn_id,
                   origin.session_id
              ) AS effective
                ON true
             WHERE origin.turn_id = entry.model_identity_turn_id
               AND origin.session_id = entry.source_session_id
               AND effective.defaults_version =
                   entry.model_identity_defaults_version
               AND effective.direct_selection_id =
                   entry.model_identity_direct_selection_id;
        WHEN 'origin_accepted_input' THEN
            SELECT origin_turn_id
              INTO checked_turn_id
              FROM accepted_input
             WHERE accepted_input_id = entry.origin_accepted_input_id
               AND session_id = entry.source_session_id
               AND disposition_kind IN (
                    'origin_of',
                    'reclassified_as_turn_origin'
               )
               AND origin_turn_id IS NOT NULL;
            IF NOT FOUND THEN
                RAISE EXCEPTION 'semantic origin input is not a turn origin'
                    USING
                        ERRCODE = '23514',
                        CONSTRAINT =
                            'semantic_transcript_entry_origin_disposition';
            END IF;
        WHEN 'steering_accepted_input' THEN
            SELECT expected_active_turn_id, consuming_model_call_id
              INTO checked_turn_id, checked_producing_call_id
              FROM accepted_input
             WHERE accepted_input_id = entry.origin_accepted_input_id
               AND session_id = entry.source_session_id
               AND disposition_kind = 'consumed_as_steering'
               AND expected_active_turn_id =
                   entry.steering_source_turn_id
               AND consuming_model_call_id IS NOT NULL;
            IF NOT FOUND THEN
                RAISE EXCEPTION
                    'semantic steering input lacks consuming call'
                    USING ERRCODE = '23514';
            END IF;
        WHEN 'turn_failed' THEN
            checked_turn_id := entry.failed_turn_id;
        WHEN 'turn_completed' THEN
            checked_turn_id := entry.completed_turn_id;
        WHEN 'turn_cancelled' THEN
            checked_turn_id := entry.cancelled_turn_id;
        WHEN 'assistant_text' THEN
            SELECT turn_id
              INTO checked_turn_id
              FROM model_call
             WHERE model_call_id = entry.producing_model_call_id
               AND state_kind = 'terminal'
               AND terminal_disposition_kind = 'completed';
        WHEN 'assistant_tool_use' THEN
            SELECT request.turn_id
              INTO checked_turn_id
              FROM tool_request AS request
             WHERE request.request_id = entry.assistant_tool_request_id
               AND request.producing_model_call_id =
                   entry.producing_model_call_id
               AND request.session_id = entry.source_session_id;
        WHEN 'tool_execution_result' THEN
            SELECT turn_id
              INTO checked_turn_id
              FROM tool_attempt
             WHERE attempt_id = entry.tool_result_attempt_id
               AND session_id = entry.source_session_id
               AND state_kind = 'terminal'
               AND terminal_disposition_kind IN ('completed', 'known_failed');
        WHEN 'tool_denied' THEN
            SELECT request.turn_id
              INTO checked_turn_id
              FROM tool_request AS request
              JOIN tool_approval_decision AS approval
                ON approval.request_id = request.request_id
               AND approval.decision_kind = 'deny'
             WHERE request.request_id = entry.tool_result_request_id
               AND request.session_id = entry.source_session_id;
        WHEN 'tool_closed_by_turn_end' THEN
            SELECT request.turn_id
              INTO checked_turn_id
              FROM tool_request AS request
              JOIN turn_lifecycle AS lifecycle
                ON lifecycle.turn_id = request.turn_id
               AND lifecycle.session_id = request.session_id
               AND lifecycle.state_kind = 'terminal'
             WHERE request.request_id = entry.tool_result_request_id
               AND request.session_id = entry.source_session_id;
        ELSE
            RAISE EXCEPTION
                'semantic payload kind % lacks construction authority',
                entry.payload_kind
                USING ERRCODE = '23514';
    END CASE;

    IF checked_turn_id IS NULL THEN
        RAISE EXCEPTION 'semantic entry lacks authoritative turn'
            USING ERRCODE = '23514';
    END IF;
    PERFORM assert_turn_lifecycle_final_state(checked_turn_id);
    IF checked_producing_call_id IS NOT NULL THEN
        PERFORM assert_model_call_final_state(checked_producing_call_id);
    END IF;
    RETURN NULL;
END;
$$;
