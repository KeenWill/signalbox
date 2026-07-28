-- Durable explicit context-compaction commands. The command row retains the
-- exact caller payload while the append-only compaction tables remain the
-- authority for model-call, summary, range, and frontier provenance.

ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_kind_closed,
    DROP CONSTRAINT durable_command_storage_version_supported;

ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_kind_closed
        CHECK (
            command_kind IN (
                'create_session',
                'create_session_from_imported_frontier',
                'replace_session_defaults',
                'replace_session_metadata',
                'submit_input',
                'decide_tool_request',
                'review_workflow',
                'compact_session'
            )
        ),
    ADD CONSTRAINT durable_command_storage_version_supported
        CHECK (
            (
                command_kind IN (
                    'create_session',
                    'create_session_from_imported_frontier',
                    'replace_session_defaults'
                )
                AND storage_version IN (1, 2, 3)
            )
            OR (
                command_kind IN (
                    'replace_session_metadata',
                    'submit_input',
                    'decide_tool_request',
                    'review_workflow',
                    'compact_session'
                )
                AND storage_version = 1
            )
        );

CREATE TABLE compact_session_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    requested_through_position numeric(20, 0),
    result_kind text NOT NULL,
    result_context_compaction_id uuid,
    model_call_id uuid NOT NULL,
    result_through_position numeric(20, 0),
    result_summary_entry_id uuid,
    result_frontier_id uuid,

    CONSTRAINT compact_session_command_kind_closed
        CHECK (command_kind = 'compact_session'),
    CONSTRAINT compact_session_command_storage_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT compact_session_command_requested_position_u64
        CHECK (
            requested_through_position IS NULL
            OR (
                requested_through_position >= 1
                AND requested_through_position <= 18446744073709551615
            )
        ),
    CONSTRAINT compact_session_command_result_kind_closed
        CHECK (result_kind IN ('pending', 'applied', 'failed')),
    CONSTRAINT compact_session_command_result_position_u64
        CHECK (
            result_through_position IS NULL
            OR (
                result_through_position >= 1
                AND result_through_position <= 18446744073709551615
            )
        ),
    CONSTRAINT compact_session_command_result_shape
        CHECK (
            (
                result_kind IN ('pending', 'failed')
                AND result_context_compaction_id IS NULL
                AND result_through_position IS NULL
                AND result_summary_entry_id IS NULL
                AND result_frontier_id IS NULL
            )
            OR (
                result_kind = 'applied'
                AND result_context_compaction_id IS NOT NULL
                AND result_through_position IS NOT NULL
                AND result_summary_entry_id IS NOT NULL
                AND result_frontier_id IS NOT NULL
            )
        ),
    CONSTRAINT compact_session_command_durable_fk
        FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command (command_id, command_kind, storage_version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT compact_session_command_session_fk
        FOREIGN KEY (session_id)
        REFERENCES session (session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT compact_session_command_compaction_fk
        FOREIGN KEY (result_context_compaction_id, session_id)
        REFERENCES context_compaction (context_compaction_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT compact_session_command_call_fk
        FOREIGN KEY (model_call_id, session_id)
        REFERENCES context_compaction_model_call (model_call_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT compact_session_command_summary_fk
        FOREIGN KEY (session_id, result_summary_entry_id)
        REFERENCES semantic_transcript_entry (source_session_id, semantic_entry_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT compact_session_command_frontier_fk
        FOREIGN KEY (session_id, result_frontier_id)
        REFERENCES context_frontier (owning_session_id, context_frontier_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION reject_compact_session_command_invalid_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.result_kind <> 'pending' THEN
            RAISE EXCEPTION 'compaction command must begin pending'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'compaction command is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF ROW(
        OLD.command_id,
        OLD.command_kind,
        OLD.storage_version,
        OLD.session_id,
        OLD.requested_through_position,
        OLD.model_call_id
    ) IS DISTINCT FROM ROW(
        NEW.command_id,
        NEW.command_kind,
        NEW.storage_version,
        NEW.session_id,
        NEW.requested_through_position,
        NEW.model_call_id
    ) THEN
        RAISE EXCEPTION 'compaction command request is immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.result_kind <> 'pending'
       OR NEW.result_kind NOT IN ('applied', 'failed')
    THEN
        RAISE EXCEPTION 'invalid compaction command result transition'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER compact_session_command_changes_are_guarded
BEFORE INSERT OR UPDATE OR DELETE ON compact_session_command
FOR EACH ROW
EXECUTE FUNCTION reject_compact_session_command_invalid_change();

CREATE OR REPLACE FUNCTION require_durable_command_typed_record()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    matching_records bigint;
BEGIN
    CASE NEW.command_kind
        WHEN 'create_session' THEN
            SELECT count(*) INTO matching_records FROM create_session_command
             WHERE command_id = NEW.command_id;
        WHEN 'create_session_from_imported_frontier' THEN
            SELECT count(*) INTO matching_records
              FROM create_session_from_imported_frontier_command
             WHERE command_id = NEW.command_id;
        WHEN 'replace_session_defaults' THEN
            SELECT count(*) INTO matching_records FROM replace_session_defaults_command
             WHERE command_id = NEW.command_id;
        WHEN 'replace_session_metadata' THEN
            SELECT count(*) INTO matching_records FROM replace_session_metadata_command
             WHERE command_id = NEW.command_id;
        WHEN 'submit_input' THEN
            SELECT count(*) INTO matching_records FROM submit_input_command
             WHERE command_id = NEW.command_id;
        WHEN 'decide_tool_request' THEN
            SELECT count(*) INTO matching_records FROM decide_tool_request_command
             WHERE command_id = NEW.command_id;
        WHEN 'review_workflow' THEN
            SELECT count(*) INTO matching_records FROM review_workflow_command
             WHERE command_id = NEW.command_id;
        WHEN 'compact_session' THEN
            SELECT count(*) INTO matching_records FROM compact_session_command
             WHERE command_id = NEW.command_id;
        ELSE
            RAISE EXCEPTION 'unsupported durable command kind %', NEW.command_kind
                USING ERRCODE = '23514';
    END CASE;

    IF matching_records <> 1 THEN
        RAISE EXCEPTION
            'durable command % requires exactly one % typed record',
            NEW.command_id,
            NEW.command_kind
            USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;

-- A later turn extends the latest compaction result only when that result
-- contains every member of the immediate predecessor's terminal frontier.
-- Otherwise the unprojected predecessor remains the exact base. The
-- compaction chain admits one leaf per session, so this returns one row.
CREATE FUNCTION turn_start_effective_predecessor_frontier(
    checked_session uuid,
    checked_predecessor_frontier uuid
)
RETURNS TABLE (
    context_frontier_id uuid,
    member_count numeric(20, 0)
)
LANGUAGE sql
STABLE
AS $$
    WITH applicable_leaf AS (
        SELECT candidate.result_frontier_id
          FROM context_compaction AS candidate
         WHERE candidate.session_id = checked_session
           AND NOT EXISTS (
                SELECT 1
                  FROM context_compaction AS successor
                 WHERE successor.session_id = candidate.session_id
                   AND successor.predecessor_compaction_id =
                           candidate.context_compaction_id
           )
           AND NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member AS predecessor_member
                  LEFT JOIN context_frontier_member AS result_member
                    ON result_member.owning_session_id = checked_session
                   AND result_member.context_frontier_id =
                           candidate.result_frontier_id
                   AND result_member.member_position =
                           predecessor_member.member_position
                   AND result_member.source_session_id =
                           predecessor_member.source_session_id
                   AND result_member.semantic_entry_id =
                           predecessor_member.semantic_entry_id
                 WHERE predecessor_member.owning_session_id = checked_session
                   AND predecessor_member.context_frontier_id =
                           checked_predecessor_frontier
                   AND result_member.member_position IS NULL
           )
    )
    SELECT frontier.context_frontier_id, frontier.member_count
      FROM context_frontier AS frontier
     WHERE frontier.owning_session_id = checked_session
       AND frontier.context_frontier_id = COALESCE(
            (SELECT result_frontier_id FROM applicable_leaf),
            checked_predecessor_frontier
       )
$$;

-- Extend the live lifecycle validators in place. Earlier migrations compose
-- exact model-identity and other lifecycle shapes into these definitions, so
-- replacing either function from an older body would discard those laws.
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
        'SELECT member_count
          INTO predecessor_terminal_member_count
          FROM context_frontier
         WHERE owning_session_id = checked_session_id
           AND context_frontier_id = predecessor_terminal_frontier;',
        'SELECT effective.context_frontier_id, effective.member_count
          INTO predecessor_terminal_frontier,
               predecessor_terminal_member_count
          FROM turn_start_effective_predecessor_frontier(
                   checked_session_id,
                   predecessor_terminal_frontier
               ) AS effective;'
    );
    IF revised = definition THEN
        RAISE EXCEPTION
            'unable to extend active-turn compaction frontier law';
    END IF;
    EXECUTE revised;

    definition := pg_get_functiondef(
        'assert_terminal_started_turn_common_final_state(uuid)'::regprocedure
    );
    revised := replace(
        definition,
        'SELECT member_count
          INTO predecessor_member_count
          FROM context_frontier
         WHERE owning_session_id = checked_session
           AND context_frontier_id = predecessor_frontier;',
        'SELECT effective.context_frontier_id, effective.member_count
          INTO predecessor_frontier, predecessor_member_count
          FROM turn_start_effective_predecessor_frontier(
                   checked_session,
                   predecessor_frontier
               ) AS effective;'
    );
    IF revised = definition THEN
        RAISE EXCEPTION
            'unable to extend terminal-turn compaction frontier law';
    END IF;
    EXECUTE revised;
END;
$$;
