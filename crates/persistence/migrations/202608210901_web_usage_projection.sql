-- Dedicated terminal model-call usage projection for bounded browser reads.
-- Token evidence is copied exactly once when its canonical ordinary,
-- approval-judge, or context-compaction call becomes terminal. Dollar cost
-- remains a read-time derivation from versioned deployment rates and is
-- deliberately not stored.

-- Tighten canonical compaction evidence before projection backfill. This is a
-- forward correction because recorded migrations are immutable. The replaced
-- constraint was defined by 202607290401_context_compaction.sql.
ALTER TABLE context_compaction_model_call
    ADD COLUMN usage_input_includes_cache_tokens boolean;
ALTER TABLE context_compaction_model_call
    DROP CONSTRAINT context_compaction_model_call_usage_nonnegative;
ALTER TABLE context_compaction_model_call
    ADD CONSTRAINT context_compaction_model_call_usage_u64
        CHECK (
            (
                input_tokens IS NULL
                OR input_tokens BETWEEN 0 AND 18446744073709551615
            )
            AND (
                output_tokens IS NULL
                OR output_tokens BETWEEN 0 AND 18446744073709551615
            )
            AND (
                cache_read_input_tokens IS NULL
                OR cache_read_input_tokens BETWEEN 0 AND 18446744073709551615
            )
            AND (
                cache_creation_input_tokens IS NULL
                OR cache_creation_input_tokens
                    BETWEEN 0 AND 18446744073709551615
            )
        );

CREATE FUNCTION require_context_compaction_usage_input_semantics()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.usage_input_includes_cache_tokens IS NULL THEN
            RAISE EXCEPTION 'compaction input-token semantics must be pinned'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF NEW.usage_input_includes_cache_tokens IS DISTINCT FROM
       OLD.usage_input_includes_cache_tokens
    THEN
        RAISE EXCEPTION 'compaction input-token semantics are immutable'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER context_compaction_usage_input_semantics_are_pinned
BEFORE INSERT OR UPDATE ON context_compaction_model_call
FOR EACH ROW
EXECUTE FUNCTION require_context_compaction_usage_input_semantics();

CREATE TABLE web_usage_oversized_profile_identity (
    profile_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    reference_digest text NOT NULL,
    exact_reference text NOT NULL,
    CONSTRAINT web_usage_oversized_profile_reference
        CHECK (octet_length(exact_reference) > 250),
    CONSTRAINT web_usage_oversized_profile_digest_shape
        CHECK (reference_digest ~ '^[0-9a-f]{32}$')
);
CREATE INDEX web_usage_oversized_profile_by_digest
    ON web_usage_oversized_profile_identity (reference_digest);

CREATE FUNCTION enforce_web_usage_oversized_profile_identity()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    expected_digest text;
BEGIN
    expected_digest := md5(NEW.exact_reference);
    IF NEW.reference_digest <> expected_digest THEN
        RAISE EXCEPTION 'oversized usage profile digest must match its exact reference'
            USING ERRCODE = '23514';
    END IF;

    -- Serialize the bounded digest bucket so exact-reference uniqueness does
    -- not require an index over the unbounded canonical reference.
    PERFORM pg_advisory_xact_lock(hashtextextended(expected_digest, 0));
    IF EXISTS (
        SELECT 1
          FROM web_usage_oversized_profile_identity
         WHERE reference_digest = expected_digest
           AND exact_reference = NEW.exact_reference
    ) THEN
        RAISE EXCEPTION 'oversized usage profile reference already has an identity'
            USING ERRCODE = '23505';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER web_usage_oversized_profile_identity_is_consistent
BEFORE INSERT ON web_usage_oversized_profile_identity
FOR EACH ROW
EXECUTE FUNCTION enforce_web_usage_oversized_profile_identity();

CREATE FUNCTION bounded_web_usage_profile(value text)
RETURNS text
LANGUAGE plpgsql
STRICT
AS $$
DECLARE
    lookup_digest text;
    mapped_id bigint;
BEGIN
    IF octet_length(value) <= 250 THEN
        RETURN 'exact:' || value;
    END IF;

    lookup_digest := md5(value);
    -- Serialize one bounded digest bucket while retaining exact collision
    -- resolution without indexing the unbounded canonical reference.
    PERFORM pg_advisory_xact_lock(hashtextextended(lookup_digest, 0));
    SELECT profile_id
      INTO mapped_id
      FROM web_usage_oversized_profile_identity
     WHERE reference_digest = lookup_digest
       AND exact_reference = value;
    IF mapped_id IS NULL THEN
        INSERT INTO web_usage_oversized_profile_identity (
            reference_digest, exact_reference
        ) VALUES (lookup_digest, value)
        RETURNING profile_id INTO mapped_id;
    END IF;
    RETURN 'mapped:' || mapped_id::text;
END;
$$;

CREATE TABLE web_usage_call_projection (
    model_call_id uuid PRIMARY KEY,
    call_kind text NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid,
    resolved_provider_model_identity_id uuid NOT NULL,
    credential_profile_label text NOT NULL,
    usage_provenance_kind text NOT NULL,
    usage_input_includes_cache_tokens boolean,
    input_tokens numeric,
    output_tokens numeric,
    cache_creation_input_tokens numeric,
    cache_read_input_tokens numeric,
    recorded_at timestamptz NOT NULL DEFAULT statement_timestamp(),

    CONSTRAINT web_usage_call_kind_closed
        CHECK (call_kind IN ('model_call', 'approval_judge', 'context_compaction')),
    CONSTRAINT web_usage_provenance_closed
        CHECK (usage_provenance_kind IN ('reported', 'estimated')),
    CONSTRAINT web_usage_credential_profile_label_bounded
        CHECK (
            char_length(credential_profile_label) > 0
            AND octet_length(credential_profile_label) <= 256
        ),
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
CREATE INDEX web_usage_by_session_kind_recorded_call
    ON web_usage_call_projection
       (session_id, call_kind, recorded_at DESC, model_call_id DESC);
CREATE INDEX web_usage_by_session_provenance_recorded_call
    ON web_usage_call_projection
       (session_id, usage_provenance_kind, recorded_at DESC, model_call_id DESC);
CREATE INDEX web_usage_by_session_model_recorded_call
    ON web_usage_call_projection
       (session_id, resolved_provider_model_identity_id,
        recorded_at DESC, model_call_id DESC);
CREATE INDEX web_usage_by_turn_recorded_call
    ON web_usage_call_projection
       (turn_id, recorded_at DESC, model_call_id DESC);
CREATE INDEX web_usage_by_model_recorded_call
    ON web_usage_call_projection
       (resolved_provider_model_identity_id, recorded_at DESC, model_call_id DESC);
CREATE INDEX web_usage_by_model_provenance_recorded_call
    ON web_usage_call_projection
       (resolved_provider_model_identity_id, usage_provenance_kind,
        recorded_at DESC, model_call_id DESC);
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
        resolved_provider_model_identity_id, credential_profile_label,
        usage_provenance_kind, usage_input_includes_cache_tokens,
        input_tokens, output_tokens,
        cache_creation_input_tokens, cache_read_input_tokens
    ) VALUES (
        NEW.model_call_id, 'model_call', NEW.session_id, NEW.turn_id,
        NEW.resolved_provider_model_identity_id,
        bounded_web_usage_profile(NEW.credential_reference),
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
        resolved_provider_model_identity_id, credential_profile_label,
        usage_provenance_kind, usage_input_includes_cache_tokens,
        input_tokens, output_tokens,
        cache_creation_input_tokens, cache_read_input_tokens
    ) VALUES (
        NEW.model_call_id, 'approval_judge', NEW.session_id, NEW.turn_id,
        NEW.resolved_provider_model_identity_id,
        bounded_web_usage_profile(NEW.credential_reference),
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
        resolved_provider_model_identity_id, credential_profile_label,
        usage_provenance_kind, usage_input_includes_cache_tokens,
        input_tokens, output_tokens,
        cache_creation_input_tokens, cache_read_input_tokens
    ) VALUES (
        NEW.model_call_id, 'context_compaction', NEW.session_id, NULL,
        NEW.resolved_provider_model_identity_id,
        bounded_web_usage_profile(NEW.credential_reference),
        'reported', NEW.usage_input_includes_cache_tokens,
        NEW.input_tokens, NEW.output_tokens,
        NEW.cache_creation_input_tokens, NEW.cache_read_input_tokens
    );
    RETURN NEW;
END;
$$;

-- Existing terminal rows are projected at each backfill statement's exact
-- timestamp. Signalbox is pre-alpha, so no deployed historical time is
-- fabricated; all subsequent rows record their terminal statement time.
INSERT INTO web_usage_call_projection (
    model_call_id, call_kind, session_id, turn_id,
    resolved_provider_model_identity_id, credential_profile_label,
    usage_provenance_kind, usage_input_includes_cache_tokens,
    input_tokens, output_tokens,
    cache_creation_input_tokens, cache_read_input_tokens
)
SELECT model_call_id, 'model_call', session_id, turn_id,
       resolved_provider_model_identity_id,
       bounded_web_usage_profile(credential_reference),
       usage_provenance_kind, usage_input_includes_cache_tokens,
       usage_input_tokens, usage_output_tokens,
       usage_cache_creation_input_tokens, usage_cache_read_input_tokens
  FROM model_call
 WHERE state_kind = 'terminal';

INSERT INTO web_usage_call_projection (
    model_call_id, call_kind, session_id, turn_id,
    resolved_provider_model_identity_id, credential_profile_label,
    usage_provenance_kind, usage_input_includes_cache_tokens,
    input_tokens, output_tokens,
    cache_creation_input_tokens, cache_read_input_tokens
)
SELECT model_call_id, 'approval_judge', session_id, turn_id,
       resolved_provider_model_identity_id,
       bounded_web_usage_profile(credential_reference),
       usage_provenance_kind, usage_input_includes_cache_tokens,
       input_tokens, output_tokens,
       cache_creation_input_tokens, cache_read_input_tokens
  FROM tool_approval_judge_model_call
 WHERE state_kind = 'terminal';

INSERT INTO web_usage_call_projection (
    model_call_id, call_kind, session_id, turn_id,
    resolved_provider_model_identity_id, credential_profile_label,
    usage_provenance_kind, usage_input_includes_cache_tokens,
    input_tokens, output_tokens,
    cache_creation_input_tokens, cache_read_input_tokens
)
SELECT model_call_id, 'context_compaction', session_id, NULL,
       resolved_provider_model_identity_id,
       bounded_web_usage_profile(credential_reference),
       'reported', usage_input_includes_cache_tokens, input_tokens, output_tokens,
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

CREATE TRIGGER web_usage_oversized_profile_identity_is_append_only
BEFORE UPDATE OR DELETE ON web_usage_oversized_profile_identity
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

CREATE TRIGGER web_usage_oversized_profile_identity_cannot_be_truncated
BEFORE TRUNCATE ON web_usage_oversized_profile_identity
FOR EACH STATEMENT
EXECUTE FUNCTION reject_web_usage_projection_truncate();
