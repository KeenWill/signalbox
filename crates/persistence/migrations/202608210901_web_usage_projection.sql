-- Dedicated terminal model-call usage projection for bounded browser reads.
-- Token evidence is copied exactly once when its canonical ordinary,
-- approval-judge, or context-compaction call becomes terminal. Dollar cost
-- remains a read-time derivation from versioned deployment rates and is
-- deliberately not stored.

CREATE FUNCTION bounded_web_usage_profile(value text)
RETURNS text
LANGUAGE sql
IMMUTABLE
STRICT
PARALLEL SAFE
AS $$
    SELECT CASE
        WHEN octet_length(value) <= 256 THEN value
        ELSE 'oversized-md5:' || md5(value)
    END
$$;

CREATE TABLE web_usage_call_projection (
    model_call_id uuid PRIMARY KEY,
    call_kind text NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid,
    resolved_provider_model_identity_id uuid NOT NULL,
    credential_reference text NOT NULL,
    usage_provenance_kind text NOT NULL,
    usage_input_includes_cache_tokens boolean,
    input_tokens numeric,
    output_tokens numeric,
    cache_creation_input_tokens numeric,
    cache_read_input_tokens numeric,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    CONSTRAINT web_usage_call_kind_closed
        CHECK (call_kind IN ('model_call', 'approval_judge', 'context_compaction')),
    CONSTRAINT web_usage_provenance_closed
        CHECK (usage_provenance_kind IN ('reported', 'estimated')),
    CONSTRAINT web_usage_credential_reference_nonempty
        CHECK (char_length(credential_reference) > 0),
    CONSTRAINT web_usage_turn_shape
        CHECK ((call_kind = 'context_compaction') = (turn_id IS NULL)),
    CONSTRAINT web_usage_token_axes_u64
        CHECK (
            (
                input_tokens IS NULL
                OR (
                    input_tokens = trunc(input_tokens)
                    AND input_tokens BETWEEN 0 AND 18446744073709551615
                )
            )
            AND (
                output_tokens IS NULL
                OR (
                    output_tokens = trunc(output_tokens)
                    AND output_tokens BETWEEN 0 AND 18446744073709551615
                )
            )
            AND (
                cache_creation_input_tokens IS NULL
                OR (
                    cache_creation_input_tokens = trunc(cache_creation_input_tokens)
                    AND cache_creation_input_tokens
                        BETWEEN 0 AND 18446744073709551615
                )
            )
            AND (
                cache_read_input_tokens IS NULL
                OR (
                    cache_read_input_tokens = trunc(cache_read_input_tokens)
                    AND cache_read_input_tokens
                        BETWEEN 0 AND 18446744073709551615
                )
            )
        ),
    CONSTRAINT web_usage_call_identity_fk
        FOREIGN KEY (model_call_id)
        REFERENCES model_call_identity (model_call_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CONSTRAINT web_usage_turn_fk
        FOREIGN KEY (turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE INDEX web_usage_by_recorded_call
    ON web_usage_call_projection (recorded_at DESC, model_call_id DESC);
CREATE INDEX web_usage_by_session_recorded_call
    ON web_usage_call_projection
       (session_id, recorded_at DESC, model_call_id DESC);
CREATE INDEX web_usage_by_turn_recorded_call
    ON web_usage_call_projection
       (turn_id, recorded_at DESC, model_call_id DESC);
CREATE INDEX web_usage_by_model_recorded_call
    ON web_usage_call_projection
       (resolved_provider_model_identity_id, recorded_at DESC, model_call_id DESC);
CREATE INDEX web_usage_by_provenance_recorded_call
    ON web_usage_call_projection
       (usage_provenance_kind, recorded_at DESC, model_call_id DESC);
CREATE INDEX web_usage_by_kind_recorded_call
    ON web_usage_call_projection
       (call_kind, recorded_at DESC, model_call_id DESC);
CREATE INDEX web_usage_by_provenance_kind_recorded_call
    ON web_usage_call_projection
       (usage_provenance_kind, call_kind, recorded_at DESC, model_call_id DESC);

CREATE FUNCTION project_terminal_model_call_usage()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO web_usage_call_projection (
        model_call_id, call_kind, session_id, turn_id,
        resolved_provider_model_identity_id, credential_reference,
        usage_provenance_kind, usage_input_includes_cache_tokens,
        input_tokens, output_tokens,
        cache_creation_input_tokens, cache_read_input_tokens
    ) VALUES (
        NEW.model_call_id, 'model_call', NEW.session_id, NEW.turn_id,
        NEW.resolved_provider_model_identity_id, NEW.credential_reference,
        NEW.usage_provenance_kind, NEW.usage_input_includes_cache_tokens,
        NEW.usage_input_tokens, NEW.usage_output_tokens,
        NEW.usage_cache_creation_input_tokens,
        NEW.usage_cache_read_input_tokens
    );
    RETURN NEW;
END;
$$;

CREATE FUNCTION project_terminal_approval_judge_usage()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO web_usage_call_projection (
        model_call_id, call_kind, session_id, turn_id,
        resolved_provider_model_identity_id, credential_reference,
        usage_provenance_kind, usage_input_includes_cache_tokens,
        input_tokens, output_tokens,
        cache_creation_input_tokens, cache_read_input_tokens
    ) VALUES (
        NEW.model_call_id, 'approval_judge', NEW.session_id, NEW.turn_id,
        NEW.resolved_provider_model_identity_id, NEW.credential_reference,
        NEW.usage_provenance_kind, NEW.usage_input_includes_cache_tokens,
        NEW.input_tokens, NEW.output_tokens,
        NEW.cache_creation_input_tokens, NEW.cache_read_input_tokens
    );
    RETURN NEW;
END;
$$;

CREATE FUNCTION project_terminal_context_compaction_usage()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO web_usage_call_projection (
        model_call_id, call_kind, session_id, turn_id,
        resolved_provider_model_identity_id, credential_reference,
        usage_provenance_kind, usage_input_includes_cache_tokens,
        input_tokens, output_tokens,
        cache_creation_input_tokens, cache_read_input_tokens
    ) VALUES (
        NEW.model_call_id, 'context_compaction', NEW.session_id, NULL,
        NEW.resolved_provider_model_identity_id, NEW.credential_reference,
        'reported', NULL, NEW.input_tokens, NEW.output_tokens,
        NEW.cache_creation_input_tokens, NEW.cache_read_input_tokens
    );
    RETURN NEW;
END;
$$;

-- Existing terminal rows are projected at the migration transaction's exact
-- timestamp. Signalbox is pre-alpha, so no deployed historical time is
-- fabricated; all subsequent rows record their terminal transaction time.
INSERT INTO web_usage_call_projection (
    model_call_id, call_kind, session_id, turn_id,
    resolved_provider_model_identity_id, credential_reference,
    usage_provenance_kind, usage_input_includes_cache_tokens,
    input_tokens, output_tokens,
    cache_creation_input_tokens, cache_read_input_tokens
)
SELECT model_call_id, 'model_call', session_id, turn_id,
       resolved_provider_model_identity_id, credential_reference,
       usage_provenance_kind, usage_input_includes_cache_tokens,
       usage_input_tokens, usage_output_tokens,
       usage_cache_creation_input_tokens, usage_cache_read_input_tokens
  FROM model_call
 WHERE state_kind = 'terminal';

INSERT INTO web_usage_call_projection (
    model_call_id, call_kind, session_id, turn_id,
    resolved_provider_model_identity_id, credential_reference,
    usage_provenance_kind, usage_input_includes_cache_tokens,
    input_tokens, output_tokens,
    cache_creation_input_tokens, cache_read_input_tokens
)
SELECT model_call_id, 'approval_judge', session_id, turn_id,
       resolved_provider_model_identity_id, credential_reference,
       usage_provenance_kind, usage_input_includes_cache_tokens,
       input_tokens, output_tokens,
       cache_creation_input_tokens, cache_read_input_tokens
  FROM tool_approval_judge_model_call
 WHERE state_kind = 'terminal';

INSERT INTO web_usage_call_projection (
    model_call_id, call_kind, session_id, turn_id,
    resolved_provider_model_identity_id, credential_reference,
    usage_provenance_kind, usage_input_includes_cache_tokens,
    input_tokens, output_tokens,
    cache_creation_input_tokens, cache_read_input_tokens
)
SELECT model_call_id, 'context_compaction', session_id, NULL,
       resolved_provider_model_identity_id, credential_reference,
       'reported', NULL, input_tokens, output_tokens,
       cache_creation_input_tokens, cache_read_input_tokens
  FROM context_compaction_model_call
 WHERE state_kind = 'terminal';

CREATE TRIGGER model_call_projects_terminal_usage
AFTER INSERT OR UPDATE ON model_call
FOR EACH ROW
WHEN (NEW.state_kind = 'terminal')
EXECUTE FUNCTION project_terminal_model_call_usage();

CREATE TRIGGER approval_judge_projects_terminal_usage
AFTER INSERT OR UPDATE ON tool_approval_judge_model_call
FOR EACH ROW
WHEN (NEW.state_kind = 'terminal')
EXECUTE FUNCTION project_terminal_approval_judge_usage();

CREATE TRIGGER context_compaction_projects_terminal_usage
AFTER INSERT OR UPDATE ON context_compaction_model_call
FOR EACH ROW
WHEN (NEW.state_kind = 'terminal')
EXECUTE FUNCTION project_terminal_context_compaction_usage();

CREATE TRIGGER web_usage_call_projection_is_append_only
BEFORE UPDATE OR DELETE ON web_usage_call_projection
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION reject_web_usage_projection_truncate()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'web usage projection cannot be truncated'
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER web_usage_call_projection_cannot_be_truncated
BEFORE TRUNCATE ON web_usage_call_projection
FOR EACH STATEMENT
EXECUTE FUNCTION reject_web_usage_projection_truncate();
