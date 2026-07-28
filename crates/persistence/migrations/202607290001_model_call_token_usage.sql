-- Provider-reported token usage is terminal model-call evidence. Each count is
-- independently nullable because providers may omit any field; zero remains a
-- reported value rather than absence. Historical terminal calls remain
-- honestly unreported.

ALTER TABLE model_call
    ADD COLUMN usage_input_tokens numeric(20, 0),
    ADD COLUMN usage_output_tokens numeric(20, 0),
    ADD COLUMN usage_cache_creation_input_tokens numeric(20, 0),
    ADD COLUMN usage_cache_read_input_tokens numeric(20, 0),
    ADD CONSTRAINT model_call_usage_input_tokens_u64
        CHECK (
            usage_input_tokens IS NULL
            OR usage_input_tokens BETWEEN 0 AND 18446744073709551615
        ),
    ADD CONSTRAINT model_call_usage_output_tokens_u64
        CHECK (
            usage_output_tokens IS NULL
            OR usage_output_tokens BETWEEN 0 AND 18446744073709551615
        ),
    ADD CONSTRAINT model_call_usage_cache_creation_input_tokens_u64
        CHECK (
            usage_cache_creation_input_tokens IS NULL
            OR usage_cache_creation_input_tokens
                BETWEEN 0 AND 18446744073709551615
        ),
    ADD CONSTRAINT model_call_usage_cache_read_input_tokens_u64
        CHECK (
            usage_cache_read_input_tokens IS NULL
            OR usage_cache_read_input_tokens
                BETWEEN 0 AND 18446744073709551615
        ),
    ADD CONSTRAINT model_call_usage_is_terminal_evidence
        CHECK (
            state_kind = 'terminal'
            OR (
                usage_input_tokens IS NULL
                AND usage_output_tokens IS NULL
                AND usage_cache_creation_input_tokens IS NULL
                AND usage_cache_read_input_tokens IS NULL
            )
        );

-- Transcript usage counts and paging both select terminal calls by session.
-- The trailing correlation keys pair with turn_lifecycle_by_session_position
-- for the pager's acceptance-position/model-call ordering.
CREATE INDEX model_call_usage_by_session_state_turn_call
    ON model_call (session_id, state_kind, turn_id, model_call_id);

-- Prepared-to-terminal transitions are provably unsent, so no provider usage
-- can exist even when the terminal disposition itself is valid. Keep this as a
-- separate additive trigger rather than rewriting an applied migration.
CREATE FUNCTION reject_model_call_unsent_usage()
RETURNS trigger
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

CREATE TRIGGER model_call_unsent_usage_is_unreported
BEFORE UPDATE ON model_call
FOR EACH ROW
EXECUTE FUNCTION reject_model_call_unsent_usage();

-- The existing model_call_changes_are_guarded trigger makes every terminal row
-- immutable. Together with the terminal-only shape above, the four fields can
-- therefore be installed only by the same update that records the terminal
-- disposition and can never be corrected or cleared later. The additive
-- model_call_unsent_usage_is_unreported trigger also rejects evidence on a
-- Prepared-to-terminal transition, for which no provider send occurred.
