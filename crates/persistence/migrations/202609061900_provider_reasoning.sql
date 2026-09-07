-- Retain provider reasoning alongside the ordered assistant response suffix.
DO $$
DECLARE
    constraint_name text;
    function_signature text;
    definition text;
    revised_definition text;
BEGIN
    FOREACH constraint_name IN ARRAY ARRAY[
        'semantic_transcript_entry_payload_kind_closed',
        'semantic_transcript_entry_payload_shape',
        'semantic_transcript_entry_response_part_ordinal_shape'
    ] LOOP
        SELECT pg_get_constraintdef(oid) INTO definition
          FROM pg_constraint
         WHERE conrelid = 'semantic_transcript_entry'::regclass
           AND conname = constraint_name;
        revised_definition := replace(
            definition,
            '''provider_compaction''::text',
            '''provider_compaction''::text, ''provider_reasoning''::text'
        );
        IF revised_definition IS NULL OR revised_definition = definition THEN
            RAISE EXCEPTION 'response-part constraint % has no provider compaction member', constraint_name;
        END IF;
        EXECUTE format('ALTER TABLE semantic_transcript_entry DROP CONSTRAINT %I', constraint_name);
        EXECUTE format('ALTER TABLE semantic_transcript_entry ADD CONSTRAINT %I %s', constraint_name, revised_definition);
    END LOOP;

    FOREACH function_signature IN ARRAY ARRAY[
        'assert_turn_lifecycle_final_state_without_steering(uuid)',
        'assert_steering_turn_terminal_final_state(uuid)',
        'assert_tool_round_final_state(uuid)',
        'require_semantic_entry_turn_state()'
    ] LOOP
        SELECT pg_get_functiondef(function_signature::regprocedure) INTO definition;
        revised_definition := replace(
            definition,
            '''assistant_text'', ''provider_compaction''',
            '''assistant_text'', ''provider_compaction'', ''provider_reasoning'''
        );
        revised_definition := replace(
            revised_definition,
            E'''provider_compaction'',\n',
            E'''provider_compaction'', ''provider_reasoning'',\n'
        );
        revised_definition := replace(
            revised_definition,
            'WHEN ''assistant_text'' THEN',
            'WHEN ''assistant_text'', ''provider_reasoning'' THEN'
        );
        IF revised_definition = definition THEN
            RAISE EXCEPTION 'response-part function % has no provider compaction predicate', function_signature;
        END IF;
        EXECUTE revised_definition;
    END LOOP;
END
$$;
