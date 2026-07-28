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
    result_model_call_id uuid,
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
                AND result_model_call_id IS NULL
                AND result_through_position IS NULL
                AND result_summary_entry_id IS NULL
                AND result_frontier_id IS NULL
            )
            OR (
                result_kind = 'applied'
                AND result_context_compaction_id IS NOT NULL
                AND result_model_call_id IS NOT NULL
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
        FOREIGN KEY (result_model_call_id, session_id)
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
        OLD.requested_through_position
    ) IS DISTINCT FROM ROW(
        NEW.command_id,
        NEW.command_kind,
        NEW.storage_version,
        NEW.session_id,
        NEW.requested_through_position
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

CREATE OR REPLACE FUNCTION assert_turn_lifecycle_final_state_without_steering(
    checked_turn_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    checked_session_id uuid;
    checked_origin_input_id uuid;
    checked_position numeric(20, 0);
    checked_attempt_history_present boolean;
    checked_state text;
    checked_lineage text;
    checked_predecessor uuid;
    checked_starting_frontier uuid;
    checked_terminal_frontier uuid;
    checked_active_phase text;
    checked_current_attempt uuid;
    checked_recovery_call uuid;
    checked_terminal_attempt uuid;
    checked_terminal_call uuid;
    checked_terminal_disposition text;
    attempt_count bigint;
    live_attempt_count bigint;
    exact_attempt_count bigint;
    contradictory_failed_attempt_count bigint;
    origin_entry_count bigint;
    origin_entry_id uuid;
    failure_entry_count bigint;
    failure_entry_id uuid;
    completion_entry_count bigint;
    completion_entry_id uuid;
    assistant_entry_count bigint;
    assistant_member_count bigint;
    origin_member_count bigint;
    origin_member_position numeric(20, 0);
    last_member_position numeric(20, 0);
    failure_member_count bigint;
    starting_member_count numeric(20, 0);
    terminal_member_count numeric(20, 0);
    predecessor_terminal_frontier uuid;
    predecessor_terminal_member_count numeric(20, 0);
    latest_compaction_result_frontier uuid;
    latest_compaction_result_member_count numeric(20, 0);
    prefix_mismatch_count bigint;
    predecessor_state text;
    predecessor_position numeric(20, 0);
    expected_predecessor_position numeric(20, 0);
BEGIN
    SELECT
        session_id,
        origin_accepted_input_id,
        acceptance_position,
        attempt_history_present,
        state_kind,
        start_lineage_kind,
        immediate_predecessor_turn_id,
        starting_frontier_id,
        terminal_frontier_id,
        active_phase_kind,
        current_attempt_id,
        recovery_model_call_id,
        terminal_attempt_id,
        terminal_model_call_id,
        terminal_disposition_kind
      INTO
        checked_session_id,
        checked_origin_input_id,
        checked_position,
        checked_attempt_history_present,
        checked_state,
        checked_lineage,
        checked_predecessor,
        checked_starting_frontier,
        checked_terminal_frontier,
        checked_active_phase,
        checked_current_attempt,
        checked_recovery_call,
        checked_terminal_attempt,
        checked_terminal_call,
        checked_terminal_disposition
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id;

    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT
        count(*),
        count(*) FILTER (WHERE state_kind <> 'ended'),
        count(*) FILTER (
            WHERE turn_attempt_id = COALESCE(
                checked_current_attempt,
                checked_terminal_attempt
            )
        ),
        count(*) FILTER (
            WHERE state_kind <> 'ended'
               OR end_disposition NOT IN ('known_failure', 'lost')
        )
      INTO
        attempt_count,
        live_attempt_count,
        exact_attempt_count,
        contradictory_failed_attempt_count
      FROM turn_attempt
     WHERE turn_id = checked_turn_id
       AND session_id = checked_session_id;

    IF checked_attempt_history_present IS DISTINCT FROM (attempt_count > 0) THEN
        RAISE EXCEPTION 'turn % attempt marker disagrees with durable attempts', checked_turn_id
            USING ERRCODE = '23514';
    END IF;

    SELECT count(*)
      INTO origin_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'origin_accepted_input'
       AND origin_accepted_input_id = checked_origin_input_id;

    SELECT semantic_entry_id
      INTO origin_entry_id
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'origin_accepted_input'
       AND origin_accepted_input_id = checked_origin_input_id;

    SELECT count(*)
      INTO failure_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'turn_failed'
       AND failed_turn_id = checked_turn_id;

    SELECT semantic_entry_id
      INTO failure_entry_id
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'turn_failed'
       AND failed_turn_id = checked_turn_id;

    SELECT count(*)
      INTO completion_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'turn_completed'
       AND completed_turn_id = checked_turn_id;

    SELECT semantic_entry_id
      INTO completion_entry_id
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'turn_completed'
       AND completed_turn_id = checked_turn_id;

    IF checked_state = 'queued' THEN
        IF attempt_count <> 0
           OR origin_entry_count <> 0
           OR failure_entry_count <> 0
           OR completion_entry_count <> 0
        THEN
            RAISE EXCEPTION 'queued turn % carries started or terminal facts', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
        RETURN;
    END IF;

    IF origin_entry_count <> 1 THEN
        RAISE EXCEPTION 'started turn % requires its exact origin entry', checked_turn_id
            USING ERRCODE = '23503';
    END IF;

    SELECT member_count
      INTO starting_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session_id
       AND context_frontier_id = checked_starting_frontier;

    SELECT max(member_position)
      INTO last_member_position
      FROM context_frontier_member
     WHERE owning_session_id = checked_session_id
       AND context_frontier_id = checked_starting_frontier;

    SELECT count(*), max(member_position)
      INTO origin_member_count, origin_member_position
      FROM context_frontier_member
     WHERE owning_session_id = checked_session_id
       AND context_frontier_id = checked_starting_frontier
       AND source_session_id = checked_session_id
       AND semantic_entry_id = origin_entry_id;

    IF origin_member_count <> 1
       OR origin_member_position IS DISTINCT FROM last_member_position
    THEN
        RAISE EXCEPTION 'turn % starting frontier does not end in its origin', checked_turn_id
            USING ERRCODE = '23503';
    END IF;

    IF checked_lineage = 'first_in_session' THEN
        IF NOT first_native_starting_frontier_matches_seed(
            checked_session_id,
            checked_starting_frontier
        )
           OR EXISTS (
            SELECT 1
              FROM turn_lifecycle AS earlier
             WHERE earlier.session_id = checked_session_id
               AND earlier.turn_id <> checked_turn_id
               AND earlier.acceptance_position < checked_position
        ) THEN
            RAISE EXCEPTION 'turn % has invalid first lineage', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    ELSE
        SELECT state_kind, acceptance_position, terminal_frontier_id
          INTO predecessor_state, predecessor_position, predecessor_terminal_frontier
          FROM turn_lifecycle
         WHERE turn_id = checked_predecessor
           AND session_id = checked_session_id;

        SELECT acceptance_position
          INTO expected_predecessor_position
          FROM turn_lifecycle
         WHERE session_id = checked_session_id
           AND turn_id = accepted_input_turn_queue_predecessor(
                checked_session_id,
                checked_turn_id
           );

        IF predecessor_state IS DISTINCT FROM 'terminal'
           OR predecessor_position IS DISTINCT FROM expected_predecessor_position
        THEN
            RAISE EXCEPTION 'turn % does not follow its immediate terminal predecessor', checked_turn_id
                USING ERRCODE = '23514';
        END IF;

        SELECT member_count
          INTO predecessor_terminal_member_count
          FROM context_frontier
         WHERE owning_session_id = checked_session_id
           AND context_frontier_id = predecessor_terminal_frontier;

        SELECT candidate.result_frontier_id, result.member_count
          INTO latest_compaction_result_frontier,
               latest_compaction_result_member_count
          FROM context_compaction AS candidate
          JOIN context_frontier AS result
            ON result.owning_session_id = candidate.session_id
           AND result.context_frontier_id = candidate.result_frontier_id
         WHERE candidate.session_id = checked_session_id
           AND result.member_count >= predecessor_terminal_member_count
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
                    ON result_member.owning_session_id = checked_session_id
                   AND result_member.context_frontier_id =
                           candidate.result_frontier_id
                   AND result_member.member_position =
                           predecessor_member.member_position
                   AND result_member.source_session_id =
                           predecessor_member.source_session_id
                   AND result_member.semantic_entry_id =
                           predecessor_member.semantic_entry_id
                 WHERE predecessor_member.owning_session_id = checked_session_id
                   AND predecessor_member.context_frontier_id =
                           predecessor_terminal_frontier
                   AND result_member.member_position IS NULL
           );

        IF latest_compaction_result_frontier IS NOT NULL THEN
            predecessor_terminal_frontier := latest_compaction_result_frontier;
            predecessor_terminal_member_count :=
                latest_compaction_result_member_count;
        END IF;

        SELECT count(*)
          INTO prefix_mismatch_count
          FROM context_frontier_member AS predecessor_member
          LEFT JOIN context_frontier_member AS starting_member
            ON starting_member.owning_session_id = checked_session_id
           AND starting_member.context_frontier_id = checked_starting_frontier
           AND starting_member.member_position = predecessor_member.member_position
           AND starting_member.source_session_id = predecessor_member.source_session_id
           AND starting_member.semantic_entry_id = predecessor_member.semantic_entry_id
         WHERE predecessor_member.owning_session_id = checked_session_id
           AND predecessor_member.context_frontier_id = predecessor_terminal_frontier
           AND starting_member.member_position IS NULL;

        IF starting_member_count IS DISTINCT FROM predecessor_terminal_member_count + 1
           OR prefix_mismatch_count <> 0
        THEN
            RAISE EXCEPTION 'turn % starting frontier is not predecessor prefix plus origin', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    END IF;

    IF checked_state = 'active' THEN
        IF failure_entry_count <> 0 OR completion_entry_count <> 0 THEN
            RAISE EXCEPTION 'active turn % carries a terminal semantic marker', checked_turn_id
                USING ERRCODE = '23514';
        END IF;

        IF checked_active_phase = 'running' THEN
            IF live_attempt_count <> 1 OR exact_attempt_count <> 1 THEN
                RAISE EXCEPTION 'running turn % requires its exact live attempt', checked_turn_id
                    USING ERRCODE = '23514';
            END IF;
        ELSE
            IF live_attempt_count <> 0
               OR exact_attempt_count <> 1
               OR NOT EXISTS (
                    SELECT 1
                      FROM turn_attempt
                     WHERE turn_attempt_id = checked_current_attempt
                       AND turn_id = checked_turn_id
                       AND session_id = checked_session_id
                       AND state_kind = 'ended'
                       AND end_disposition IN ('ambiguous', 'lost')
               )
               OR NOT EXISTS (
                    SELECT 1
                      FROM model_call
                     WHERE model_call_id = checked_recovery_call
                       AND turn_attempt_id = checked_current_attempt
                       AND turn_id = checked_turn_id
                       AND session_id = checked_session_id
                       AND state_kind = 'terminal'
                       AND terminal_disposition_kind = 'ambiguous'
               )
            THEN
                RAISE EXCEPTION 'turn % has an incomplete model-call recovery wait', checked_turn_id
                    USING ERRCODE = '23514';
            END IF;
        END IF;
        RETURN;
    END IF;

    IF live_attempt_count <> 0 THEN
        RAISE EXCEPTION 'terminal turn % retains a live attempt', checked_turn_id
            USING ERRCODE = '23514';
    END IF;

    SELECT member_count
      INTO terminal_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session_id
       AND context_frontier_id = checked_terminal_frontier;

    SELECT count(*)
      INTO prefix_mismatch_count
      FROM context_frontier_member AS starting_member
      LEFT JOIN context_frontier_member AS terminal_member
        ON terminal_member.owning_session_id = checked_session_id
       AND terminal_member.context_frontier_id = checked_terminal_frontier
       AND terminal_member.member_position = starting_member.member_position
       AND terminal_member.source_session_id = starting_member.source_session_id
       AND terminal_member.semantic_entry_id = starting_member.semantic_entry_id
     WHERE starting_member.owning_session_id = checked_session_id
       AND starting_member.context_frontier_id = checked_starting_frontier
       AND terminal_member.member_position IS NULL;

    IF checked_terminal_disposition = 'failed' THEN
        IF contradictory_failed_attempt_count <> 0 THEN
            RAISE EXCEPTION
                'failed terminal turn % permits only known_failure or lost ended attempts',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;

        IF failure_entry_count <> 1
           OR completion_entry_count <> 0
           OR EXISTS (
                SELECT 1
                  FROM model_call
                 WHERE turn_id = checked_turn_id
                   AND session_id = checked_session_id
                   AND (
                        state_kind <> 'terminal'
                        OR terminal_disposition_kind NOT IN (
                            'known_failed',
                            'cancelled'
                        )
                   )
           )
        THEN
            RAISE EXCEPTION 'failed turn % has contradictory terminal facts', checked_turn_id
                USING ERRCODE = '23514';
        END IF;

        SELECT count(*)
          INTO failure_member_count
          FROM context_frontier_member
         WHERE owning_session_id = checked_session_id
           AND context_frontier_id = checked_terminal_frontier
           AND source_session_id = checked_session_id
           AND semantic_entry_id = failure_entry_id;

        IF terminal_member_count IS DISTINCT FROM starting_member_count + 1
           OR prefix_mismatch_count <> 0
           OR failure_member_count <> 1
           OR NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member
                 WHERE owning_session_id = checked_session_id
                   AND context_frontier_id = checked_terminal_frontier
                   AND member_position = terminal_member_count
                   AND source_session_id = checked_session_id
                   AND semantic_entry_id = failure_entry_id
           )
        THEN
            RAISE EXCEPTION 'failed turn % terminal frontier is not prefix plus failure', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    ELSIF checked_terminal_disposition = 'refused' THEN
        IF failure_entry_count <> 0
           OR completion_entry_count <> 0
           OR checked_terminal_frontier = checked_starting_frontier
           OR terminal_member_count IS DISTINCT FROM starting_member_count
           OR prefix_mismatch_count <> 0
           OR NOT EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE turn_attempt_id = checked_terminal_attempt
                   AND end_disposition IN ('turn_refused', 'lost')
           )
           OR NOT EXISTS (
                SELECT 1
                  FROM model_call
                 WHERE model_call_id = checked_terminal_call
                   AND turn_attempt_id = checked_terminal_attempt
                   AND terminal_disposition_kind = 'refused'
           )
        THEN
            RAISE EXCEPTION 'refused turn % lacks its exact equal-content boundary', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    ELSE
        SELECT count(*)
          INTO assistant_entry_count
          FROM semantic_transcript_entry
         WHERE source_session_id = checked_session_id
           AND payload_kind = 'assistant_text'
           AND producing_model_call_id = checked_terminal_call;

        SELECT count(*)
          INTO assistant_member_count
          FROM context_frontier_member AS member
          JOIN semantic_transcript_entry AS entry
            ON entry.source_session_id = member.source_session_id
           AND entry.semantic_entry_id = member.semantic_entry_id
         WHERE member.owning_session_id = checked_session_id
           AND member.context_frontier_id = checked_terminal_frontier
           AND member.member_position > starting_member_count
           AND member.member_position < terminal_member_count
           AND entry.payload_kind = 'assistant_text'
           AND entry.producing_model_call_id = checked_terminal_call;

        IF failure_entry_count <> 0
           OR completion_entry_count <> 1
           OR terminal_member_count
                IS DISTINCT FROM starting_member_count + assistant_entry_count + 1
           OR prefix_mismatch_count <> 0
           OR assistant_member_count <> assistant_entry_count
           OR NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member
                 WHERE owning_session_id = checked_session_id
                   AND context_frontier_id = checked_terminal_frontier
                   AND member_position = terminal_member_count
                   AND source_session_id = checked_session_id
                   AND semantic_entry_id = completion_entry_id
           )
           OR NOT EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE turn_attempt_id = checked_terminal_attempt
                   AND end_disposition IN ('turn_completed', 'lost')
           )
           OR NOT EXISTS (
                SELECT 1
                  FROM model_call
                 WHERE model_call_id = checked_terminal_call
                   AND turn_attempt_id = checked_terminal_attempt
                   AND terminal_disposition_kind = 'completed'
           )
        THEN
            RAISE EXCEPTION 'completed turn % lacks its atomic ordered response boundary', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    END IF;
END;
$$;
