ALTER TABLE tool_attempt
    ADD COLUMN context_result_byte_limit bigint
        CHECK (context_result_byte_limit >= 0),
    ADD COLUMN context_result_text text,
    ADD CONSTRAINT tool_attempt_context_result_shape CHECK (
        (terminal_disposition_kind = 'completed' AND context_result_text IS NOT NULL)
        OR (terminal_disposition_kind IS DISTINCT FROM 'completed' AND context_result_text IS NULL)
    );

CREATE FUNCTION reject_tool_result_limit_change() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.state_kind <> 'prepared'
       AND OLD.context_result_byte_limit IS DISTINCT FROM NEW.context_result_byte_limit THEN
        RAISE EXCEPTION 'issued tool result context limit is immutable'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER tool_result_limit_is_immutable
BEFORE UPDATE ON tool_attempt
FOR EACH ROW EXECUTE FUNCTION reject_tool_result_limit_change();
