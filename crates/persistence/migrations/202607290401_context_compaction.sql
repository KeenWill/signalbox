-- Append-only context compaction. A dedicated physical model call records the
-- selected model, target, source frontier, terminal disposition, and any
-- provider-reported usage. A completed compaction then appends one semantic
-- summary entry and one complete result frontier; no transcript row is
-- deleted or rewritten.

CREATE TABLE model_call_identity (
    model_call_id uuid PRIMARY KEY,
    call_kind text NOT NULL,

    CONSTRAINT model_call_identity_kind_closed
        CHECK (call_kind IN ('ordinary', 'context_compaction'))
);

INSERT INTO model_call_identity (model_call_id, call_kind)
SELECT model_call_id, 'ordinary'
  FROM model_call;

CREATE FUNCTION reserve_model_call_identity()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO model_call_identity (model_call_id, call_kind)
    VALUES (NEW.model_call_id, TG_ARGV[0]);
    RETURN NEW;
END;
$$;

CREATE TRIGGER model_call_reserves_global_identity
BEFORE INSERT ON model_call
FOR EACH ROW
EXECUTE FUNCTION reserve_model_call_identity('ordinary');

CREATE TRIGGER model_call_identity_is_append_only
BEFORE UPDATE OR DELETE ON model_call_identity
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE context_compaction_model_call (
    model_call_id uuid PRIMARY KEY,
    session_id uuid NOT NULL,
    direct_model_selection_id uuid NOT NULL,
    resolved_provider_model_identity_id uuid NOT NULL,
    source_frontier_id uuid NOT NULL,
    credential_reference text NOT NULL,
    state_kind text NOT NULL,
    terminal_disposition_kind text,
    input_tokens numeric(20, 0),
    output_tokens numeric(20, 0),
    cache_read_input_tokens numeric(20, 0),
    cache_creation_input_tokens numeric(20, 0),

    CONSTRAINT context_compaction_model_call_state_closed
        CHECK (state_kind IN ('prepared', 'in_flight', 'terminal')),
    CONSTRAINT context_compaction_model_call_disposition_closed
        CHECK (
            terminal_disposition_kind IS NULL
            OR terminal_disposition_kind IN (
                'completed',
                'known_failed',
                'refused',
                'cancelled',
                'ambiguous'
            )
        ),
    CONSTRAINT context_compaction_model_call_credential_reference_nonempty
        CHECK (char_length(credential_reference) > 0),
    CONSTRAINT context_compaction_model_call_state_shape
        CHECK (
            (state_kind <> 'terminal' AND terminal_disposition_kind IS NULL)
            OR
            (state_kind = 'terminal' AND terminal_disposition_kind IS NOT NULL)
        ),
    CONSTRAINT context_compaction_model_call_usage_nonnegative
        CHECK (
            (input_tokens IS NULL OR input_tokens >= 0)
            AND (output_tokens IS NULL OR output_tokens >= 0)
            AND (
                cache_read_input_tokens IS NULL
                OR cache_read_input_tokens >= 0
            )
            AND (
                cache_creation_input_tokens IS NULL
                OR cache_creation_input_tokens >= 0
            )
        ),
    CONSTRAINT context_compaction_model_call_session_key
        UNIQUE (model_call_id, session_id),
    CONSTRAINT context_compaction_model_call_session_fk
        FOREIGN KEY (session_id)
        REFERENCES session (session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT context_compaction_model_call_frontier_fk
        FOREIGN KEY (session_id, source_frontier_id)
        REFERENCES context_frontier (owning_session_id, context_frontier_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER context_compaction_call_reserves_global_identity
BEFORE INSERT ON context_compaction_model_call
FOR EACH ROW
EXECUTE FUNCTION reserve_model_call_identity('context_compaction');

CREATE FUNCTION reject_context_compaction_model_call_invalid_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.state_kind <> 'prepared'
            OR NEW.terminal_disposition_kind IS NOT NULL
            OR NEW.input_tokens IS NOT NULL
            OR NEW.output_tokens IS NOT NULL
            OR NEW.cache_read_input_tokens IS NOT NULL
            OR NEW.cache_creation_input_tokens IS NOT NULL
        THEN
            RAISE EXCEPTION 'compaction model call must be inserted as Prepared'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'compaction model call is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF ROW(
        OLD.model_call_id,
        OLD.session_id,
        OLD.direct_model_selection_id,
        OLD.resolved_provider_model_identity_id,
        OLD.source_frontier_id,
        OLD.credential_reference
    ) IS DISTINCT FROM ROW(
        NEW.model_call_id,
        NEW.session_id,
        NEW.direct_model_selection_id,
        NEW.resolved_provider_model_identity_id,
        NEW.source_frontier_id,
        NEW.credential_reference
    ) THEN
        RAISE EXCEPTION 'compaction model call authorization facts are immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'terminal' THEN
        RAISE EXCEPTION 'terminal compaction model call is immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'prepared'
       AND NEW.state_kind = 'terminal'
       AND NEW.terminal_disposition_kind NOT IN ('known_failed', 'cancelled')
    THEN
        RAISE EXCEPTION 'prepared compaction call cannot record provider outcome'
            USING ERRCODE = '23514';
    END IF;

    IF NOT (
        (OLD.state_kind = 'prepared' AND NEW.state_kind IN ('in_flight', 'terminal'))
        OR (OLD.state_kind = 'in_flight' AND NEW.state_kind = 'terminal')
    ) THEN
        RAISE EXCEPTION 'invalid compaction model call transition'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.state_kind <> 'terminal' AND (
        NEW.input_tokens IS NOT NULL
        OR NEW.output_tokens IS NOT NULL
        OR NEW.cache_read_input_tokens IS NOT NULL
        OR NEW.cache_creation_input_tokens IS NOT NULL
    ) THEN
        RAISE EXCEPTION 'compaction usage is terminal evidence'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER context_compaction_model_call_changes_are_guarded
BEFORE INSERT OR UPDATE OR DELETE ON context_compaction_model_call
FOR EACH ROW
EXECUTE FUNCTION reject_context_compaction_model_call_invalid_change();

ALTER TABLE semantic_transcript_entry
    ADD COLUMN context_summary_value text,
    ADD COLUMN context_summary_producing_call_id uuid,
    ADD COLUMN context_summary_first_source_session_id uuid,
    ADD COLUMN context_summary_first_entry_id uuid,
    ADD COLUMN context_summary_through_source_session_id uuid,
    ADD COLUMN context_summary_through_entry_id uuid;

DO $$
DECLARE
    legacy_shape text;
BEGIN
    SELECT pg_get_expr(constraint_record.conbin, constraint_record.conrelid)
      INTO legacy_shape
      FROM pg_constraint AS constraint_record
     WHERE constraint_record.conrelid = 'semantic_transcript_entry'::regclass
       AND constraint_record.conname = 'semantic_transcript_entry_payload_shape';

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
                    'context_summary',
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
                     payload_kind = ''context_summary''
                     AND context_summary_value IS NOT NULL
                     AND context_summary_value <> ''''
                     AND context_summary_producing_call_id IS NOT NULL
                     AND context_summary_first_source_session_id IS NOT NULL
                     AND context_summary_first_entry_id IS NOT NULL
                     AND context_summary_through_source_session_id IS NOT NULL
                     AND context_summary_through_entry_id IS NOT NULL
                     AND origin_accepted_input_id IS NULL
                     AND steering_source_turn_id IS NULL
                     AND model_identity_turn_id IS NULL
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
                     payload_kind <> ''context_summary''
                     AND context_summary_value IS NULL
                     AND context_summary_producing_call_id IS NULL
                     AND context_summary_first_source_session_id IS NULL
                     AND context_summary_first_entry_id IS NULL
                     AND context_summary_through_source_session_id IS NULL
                     AND context_summary_through_entry_id IS NULL
                     AND (%s)
                 )
             )',
        legacy_shape
    );
END;
$$;

ALTER TABLE semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_context_summary_call_fk
        FOREIGN KEY (
            context_summary_producing_call_id,
            source_session_id
        )
        REFERENCES context_compaction_model_call (model_call_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT semantic_transcript_entry_context_summary_first_fk
        FOREIGN KEY (
            context_summary_first_source_session_id,
            context_summary_first_entry_id
        )
        REFERENCES semantic_transcript_entry (source_session_id, semantic_entry_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT semantic_transcript_entry_context_summary_through_fk
        FOREIGN KEY (
            context_summary_through_source_session_id,
            context_summary_through_entry_id
        )
        REFERENCES semantic_transcript_entry (source_session_id, semantic_entry_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

CREATE TABLE context_compaction (
    context_compaction_id uuid PRIMARY KEY,
    session_id uuid NOT NULL,
    predecessor_compaction_id uuid,
    source_frontier_id uuid NOT NULL,
    result_frontier_id uuid NOT NULL,
    producing_call_id uuid NOT NULL,
    first_source_session_id uuid NOT NULL,
    first_entry_id uuid NOT NULL,
    through_source_session_id uuid NOT NULL,
    through_entry_id uuid NOT NULL,
    summary_entry_id uuid NOT NULL,

    CONSTRAINT context_compaction_call_once UNIQUE (producing_call_id),
    CONSTRAINT context_compaction_result_once UNIQUE (result_frontier_id),
    CONSTRAINT context_compaction_summary_once UNIQUE (summary_entry_id),
    CONSTRAINT context_compaction_session_key UNIQUE (context_compaction_id, session_id),
    CONSTRAINT context_compaction_not_same_frontier
        CHECK (source_frontier_id <> result_frontier_id),
    CONSTRAINT context_compaction_session_fk
        FOREIGN KEY (session_id) REFERENCES session (session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CONSTRAINT context_compaction_predecessor_fk
        FOREIGN KEY (predecessor_compaction_id, session_id)
        REFERENCES context_compaction (context_compaction_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT context_compaction_source_frontier_fk
        FOREIGN KEY (session_id, source_frontier_id)
        REFERENCES context_frontier (owning_session_id, context_frontier_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT context_compaction_result_frontier_fk
        FOREIGN KEY (session_id, result_frontier_id)
        REFERENCES context_frontier (owning_session_id, context_frontier_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT context_compaction_call_fk
        FOREIGN KEY (producing_call_id, session_id)
        REFERENCES context_compaction_model_call (model_call_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT context_compaction_first_entry_fk
        FOREIGN KEY (first_source_session_id, first_entry_id)
        REFERENCES semantic_transcript_entry (source_session_id, semantic_entry_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT context_compaction_through_entry_fk
        FOREIGN KEY (through_source_session_id, through_entry_id)
        REFERENCES semantic_transcript_entry (source_session_id, semantic_entry_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT context_compaction_summary_entry_fk
        FOREIGN KEY (session_id, summary_entry_id)
        REFERENCES semantic_transcript_entry (source_session_id, semantic_entry_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE UNIQUE INDEX context_compaction_one_root_per_session
    ON context_compaction (session_id)
    WHERE predecessor_compaction_id IS NULL;

CREATE UNIQUE INDEX context_compaction_one_successor_per_predecessor
    ON context_compaction (session_id, predecessor_compaction_id)
    WHERE predecessor_compaction_id IS NOT NULL;

CREATE FUNCTION require_context_compaction_exact_evidence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    source_count numeric(20, 0);
    result_count numeric(20, 0);
    first_position numeric(20, 0);
    through_position numeric(20, 0);
    predecessor_result uuid;
    predecessor_summary_entry uuid;
    mismatch_count bigint;
BEGIN
    SELECT count(*)
      INTO mismatch_count
      FROM context_compaction_model_call AS call
     WHERE call.model_call_id = NEW.producing_call_id
       AND call.session_id = NEW.session_id
       AND call.source_frontier_id = NEW.source_frontier_id
       AND call.state_kind = 'terminal'
       AND call.terminal_disposition_kind = 'completed';
    IF mismatch_count <> 1 THEN
        RAISE EXCEPTION 'compaction requires its exact completed dedicated call'
            USING ERRCODE = '23514';
    END IF;

    SELECT count(*)
      INTO mismatch_count
      FROM semantic_transcript_entry AS summary
     WHERE summary.source_session_id = NEW.session_id
       AND summary.semantic_entry_id = NEW.summary_entry_id
       AND summary.payload_kind = 'context_summary'
       AND summary.context_summary_producing_call_id = NEW.producing_call_id
       AND summary.context_summary_first_source_session_id =
           NEW.first_source_session_id
       AND summary.context_summary_first_entry_id = NEW.first_entry_id
       AND summary.context_summary_through_source_session_id =
           NEW.through_source_session_id
       AND summary.context_summary_through_entry_id = NEW.through_entry_id;
    IF mismatch_count <> 1 THEN
        RAISE EXCEPTION 'compaction requires its exact summary provenance'
            USING ERRCODE = '23514';
    END IF;

    SELECT member_count
      INTO source_count
      FROM context_frontier
     WHERE owning_session_id = NEW.session_id
       AND context_frontier_id = NEW.source_frontier_id;
    SELECT member_count
      INTO result_count
      FROM context_frontier
     WHERE owning_session_id = NEW.session_id
       AND context_frontier_id = NEW.result_frontier_id;
    SELECT member_position
      INTO first_position
      FROM context_frontier_member
     WHERE owning_session_id = NEW.session_id
       AND context_frontier_id = NEW.source_frontier_id
       AND source_session_id = NEW.first_source_session_id
       AND semantic_entry_id = NEW.first_entry_id;
    SELECT member_position
      INTO through_position
      FROM context_frontier_member
     WHERE owning_session_id = NEW.session_id
       AND context_frontier_id = NEW.source_frontier_id
       AND source_session_id = NEW.through_source_session_id
       AND semantic_entry_id = NEW.through_entry_id;
    IF source_count IS NULL
       OR result_count IS NULL
       OR first_position IS NULL
       OR through_position IS NULL
       OR first_position > through_position
       OR result_count <> source_count + 1
    THEN
        RAISE EXCEPTION 'compaction range or frontier cardinality is invalid'
            USING ERRCODE = '23514';
    END IF;

    SELECT
        count(*) FILTER (WHERE entry.payload_kind = 'assistant_tool_use')
        - count(*) FILTER (
            WHERE entry.payload_kind IN (
                'tool_execution_result',
                'tool_denied',
                'tool_closed_by_turn_end'
            )
        )
      INTO mismatch_count
      FROM context_frontier_member AS member
      JOIN semantic_transcript_entry AS entry
        ON entry.source_session_id = member.source_session_id
       AND entry.semantic_entry_id = member.semantic_entry_id
     WHERE member.owning_session_id = NEW.session_id
       AND member.context_frontier_id = NEW.source_frontier_id
       AND member.member_position BETWEEN first_position AND through_position;
    IF mismatch_count <> 0 THEN
        RAISE EXCEPTION 'compaction boundary leaves a tool exchange open'
            USING ERRCODE = '23514';
    END IF;

    SELECT count(*)
      INTO mismatch_count
      FROM context_frontier_member AS source_member
      LEFT JOIN context_frontier_member AS result_member
        ON result_member.owning_session_id = NEW.session_id
       AND result_member.context_frontier_id = NEW.result_frontier_id
       AND result_member.member_position = source_member.member_position
       AND result_member.source_session_id = source_member.source_session_id
       AND result_member.semantic_entry_id = source_member.semantic_entry_id
     WHERE source_member.owning_session_id = NEW.session_id
       AND source_member.context_frontier_id = NEW.source_frontier_id
       AND result_member.member_position IS NULL;
    IF mismatch_count <> 0 OR NOT EXISTS (
        SELECT 1
          FROM context_frontier_member AS result_member
         WHERE result_member.owning_session_id = NEW.session_id
           AND result_member.context_frontier_id = NEW.result_frontier_id
           AND result_member.member_position = result_count
           AND result_member.source_session_id = NEW.session_id
           AND result_member.semantic_entry_id = NEW.summary_entry_id
    ) THEN
        RAISE EXCEPTION 'compaction result must be the source plus its summary'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.predecessor_compaction_id IS NULL THEN
        IF first_position <> 1 THEN
            RAISE EXCEPTION 'root compaction range must start at the visible frontier start'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        SELECT result_frontier_id, summary_entry_id
          INTO predecessor_result, predecessor_summary_entry
          FROM context_compaction
         WHERE context_compaction_id = NEW.predecessor_compaction_id
           AND session_id = NEW.session_id;
        SELECT count(*)
          INTO mismatch_count
          FROM context_frontier_member AS predecessor_member
          LEFT JOIN context_frontier_member AS source_member
            ON source_member.owning_session_id = NEW.session_id
           AND source_member.context_frontier_id = NEW.source_frontier_id
           AND source_member.member_position = predecessor_member.member_position
           AND source_member.source_session_id = predecessor_member.source_session_id
           AND source_member.semantic_entry_id = predecessor_member.semantic_entry_id
         WHERE predecessor_member.owning_session_id = NEW.session_id
           AND predecessor_member.context_frontier_id = predecessor_result
           AND source_member.member_position IS NULL;
        IF predecessor_result IS NULL
           OR mismatch_count <> 0
           OR NEW.first_source_session_id <> NEW.session_id
           OR NEW.first_entry_id <> predecessor_summary_entry
        THEN
            RAISE EXCEPTION 'compaction predecessor result and visible start must match'
                USING ERRCODE = '23514';
        END IF;
    END IF;

    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER context_compaction_requires_exact_evidence
AFTER INSERT ON context_compaction
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_context_compaction_exact_evidence();

CREATE TRIGGER context_compaction_is_append_only
BEFORE UPDATE OR DELETE ON context_compaction
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();
