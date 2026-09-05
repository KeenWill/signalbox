-- Retain opaque provider-produced compaction blocks as ordered response parts.

ALTER TABLE model_call
    ADD COLUMN retained_input_tokens numeric(20,0),
    ADD CONSTRAINT model_call_retained_input_tokens_u64
        CHECK (retained_input_tokens IS NULL OR
               retained_input_tokens BETWEEN 0 AND 18446744073709551615),
    ADD CONSTRAINT model_call_retained_input_tokens_is_terminal_completion
        CHECK (retained_input_tokens IS NULL OR
               (state_kind = 'terminal' AND terminal_disposition_kind = 'completed'));

CREATE OR REPLACE FUNCTION reject_model_call_unsent_usage() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF OLD.state_kind = 'prepared'
       AND NEW.state_kind = 'terminal'
       AND (
           NEW.usage_input_tokens IS NOT NULL
           OR NEW.usage_output_tokens IS NOT NULL
           OR NEW.usage_cache_creation_input_tokens IS NOT NULL
           OR NEW.usage_cache_read_input_tokens IS NOT NULL
           OR NEW.retained_input_tokens IS NOT NULL
       )
    THEN
        RAISE EXCEPTION 'an unsent call cannot carry provider-reported token usage'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'model_call_unsent_usage_unreported';
    END IF;

    RETURN NEW;
END;
$$;

DO $$
DECLARE
    definition text;
    revised_definition text;
BEGIN
    SELECT pg_get_constraintdef(oid) INTO definition
      FROM pg_constraint
     WHERE conrelid = 'semantic_transcript_entry'::regclass
       AND conname = 'semantic_transcript_entry_payload_kind_closed';
    EXECUTE 'ALTER TABLE semantic_transcript_entry DROP CONSTRAINT semantic_transcript_entry_payload_kind_closed';
    definition := replace(definition, '''assistant_text''::text,', '''assistant_text''::text, ''provider_compaction''::text,');
    EXECUTE 'ALTER TABLE semantic_transcript_entry ADD CONSTRAINT semantic_transcript_entry_payload_kind_closed ' || definition;

    SELECT pg_get_constraintdef(oid) INTO definition
      FROM pg_constraint
     WHERE conrelid = 'semantic_transcript_entry'::regclass
       AND conname = 'semantic_transcript_entry_payload_shape';
    EXECUTE 'ALTER TABLE semantic_transcript_entry DROP CONSTRAINT semantic_transcript_entry_payload_shape';
    definition := replace(definition, 'payload_kind = ''assistant_text''::text', 'payload_kind = ANY (ARRAY[''assistant_text''::text, ''provider_compaction''::text])');
    EXECUTE 'ALTER TABLE semantic_transcript_entry ADD CONSTRAINT semantic_transcript_entry_payload_shape ' || definition;

    SELECT pg_get_constraintdef(oid) INTO definition
      FROM pg_constraint
     WHERE conrelid = 'semantic_transcript_entry'::regclass
       AND conname = 'semantic_transcript_entry_response_part_ordinal_shape';
    EXECUTE 'ALTER TABLE semantic_transcript_entry DROP CONSTRAINT semantic_transcript_entry_response_part_ordinal_shape';
    definition := replace(definition, '''assistant_text''::text, ''assistant_tool_use''::text', '''assistant_text''::text, ''provider_compaction''::text, ''assistant_tool_use''::text');
    EXECUTE 'ALTER TABLE semantic_transcript_entry ADD CONSTRAINT semantic_transcript_entry_response_part_ordinal_shape ' || definition;

    SELECT pg_get_functiondef(
        'assert_turn_lifecycle_final_state_without_steering(uuid)'::regprocedure
    ) INTO definition;
    revised_definition := replace(
        definition,
        'payload_kind = ''assistant_text''',
        'payload_kind IN (''assistant_text'', ''provider_compaction'')'
    );
    IF revised_definition = definition THEN
        RAISE EXCEPTION 'turn final-state definition has no assistant response predicate';
    END IF;
    EXECUTE revised_definition;

    SELECT pg_get_functiondef(
        'assert_steering_turn_terminal_final_state(uuid)'::regprocedure
    ) INTO definition;
    revised_definition := replace(
        definition,
        'payload_kind = ''assistant_text''',
        'payload_kind IN (''assistant_text'', ''provider_compaction'')'
    );
    IF revised_definition = definition THEN
        RAISE EXCEPTION 'steering final-state definition has no assistant response predicate';
    END IF;
    EXECUTE revised_definition;

    SELECT pg_get_functiondef(
        'assert_tool_round_final_state(uuid)'::regprocedure
    ) INTO definition;
    revised_definition := replace(
        definition,
        'payload_kind IN (''assistant_text'', ''assistant_tool_use'')',
        'payload_kind IN (''assistant_text'', ''provider_compaction'', ''assistant_tool_use'')'
    );
    revised_definition := replace(
        revised_definition,
        $needle$                    'assistant_text',
                    'assistant_tool_use'$needle$,
        $replacement$                    'assistant_text',
                    'provider_compaction',
                    'assistant_tool_use'$replacement$
    );
    IF revised_definition = definition THEN
        RAISE EXCEPTION 'tool-round definition has no assistant response predicate';
    END IF;
    EXECUTE revised_definition;

    SELECT pg_get_functiondef(
        'require_semantic_entry_turn_state()'::regprocedure
    ) INTO definition;
    revised_definition := replace(
        definition,
        $needle$    CASE entry.payload_kind$needle$,
        $replacement$    IF entry.payload_kind = 'provider_compaction' THEN
        SELECT turn_id
          INTO checked_turn_id
          FROM model_call
         WHERE model_call_id = entry.producing_model_call_id
           AND state_kind = 'terminal'
           AND terminal_disposition_kind = 'completed';
    END IF;

    CASE entry.payload_kind$replacement$
    );
    revised_definition := replace(
        revised_definition,
        $needle$        WHEN 'assistant_text' THEN$needle$,
        $replacement$        WHEN 'provider_compaction' THEN
            NULL;
        WHEN 'assistant_text' THEN$replacement$
    );
    IF revised_definition = definition THEN
        RAISE EXCEPTION 'semantic-entry authority definition has no payload dispatch';
    END IF;
    EXECUTE revised_definition;
END
$$;

-- Opaque replay bytes are not projected human-readable transcript text.
CREATE OR REPLACE FUNCTION append_session_timeline_transcript_bytes() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.payload_kind NOT IN ('assistant_text', 'context_summary') THEN
        RETURN NULL;
    END IF;
    PERFORM 1 FROM outbox_sequence_state WHERE singleton FOR UPDATE;
    UPDATE session_timeline_fact
       SET projected_text_bytes = projected_text_bytes
           + coalesce(octet_length(convert_to(NEW.assistant_text_value, 'UTF8')), 0)
           + coalesce(octet_length(convert_to(NEW.context_summary_value, 'UTF8')), 0)
     WHERE session_id = NEW.source_session_id;
    RETURN NULL;
END
$$;
