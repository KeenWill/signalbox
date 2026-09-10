ALTER TABLE tool_attempt
    ADD COLUMN result_media_reference jsonb,
    DROP CONSTRAINT tool_attempt_result_kind_closed,
    ADD CONSTRAINT tool_attempt_result_kind_closed CHECK (
        result_content_kind IS NULL OR result_content_kind IN ('text', 'media')
    ),
    ADD CONSTRAINT tool_attempt_media_reference_shape CHECK (
        (result_content_kind IS NOT DISTINCT FROM 'media' AND result_media_reference IS NOT NULL
         AND jsonb_typeof(result_media_reference) = 'object'
         AND octet_length(result_media_reference::text) <= 4096)
        OR (result_content_kind IS DISTINCT FROM 'media' AND result_media_reference IS NULL)
    );

DO $$
DECLARE definition text;
BEGIN
    SELECT pg_get_constraintdef(oid) INTO STRICT definition
      FROM pg_constraint
     WHERE conrelid = 'tool_attempt'::regclass
       AND conname = 'tool_attempt_state_payload_shape';
    definition := replace(definition, $old$result_content_kind = 'text'::text$old$, $new$result_content_kind = ANY (ARRAY['text'::text, 'media'::text])$new$);
    ALTER TABLE tool_attempt DROP CONSTRAINT tool_attempt_state_payload_shape;
    EXECUTE 'ALTER TABLE tool_attempt ADD CONSTRAINT tool_attempt_state_payload_shape ' || definition;
END;
$$;

CREATE FUNCTION reject_terminal_media_reference_change() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.state_kind = 'terminal'
       AND OLD.result_media_reference IS DISTINCT FROM NEW.result_media_reference THEN
        RAISE EXCEPTION 'terminal media reference is immutable' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER terminal_media_reference_is_immutable
BEFORE UPDATE ON tool_attempt
FOR EACH ROW EXECUTE FUNCTION reject_terminal_media_reference_change();
