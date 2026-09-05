-- Retain opaque provider-produced compaction blocks as ordered response parts.

DO $$
DECLARE
    definition text;
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
END
$$;
