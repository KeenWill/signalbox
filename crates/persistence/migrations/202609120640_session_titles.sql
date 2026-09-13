ALTER TABLE replace_session_metadata_command
    DROP CONSTRAINT replace_session_metadata_command_issuer_shape,
    DROP CONSTRAINT replace_session_metadata_command_title_request_shape,
    ADD CONSTRAINT replace_session_metadata_command_title_request_shape CHECK (
        NOT title_only OR (actor_kind IN ('user', 'core') AND replacement_title IS NOT NULL)
    ),
    ADD CONSTRAINT replace_session_metadata_command_issuer_shape CHECK (
        (issuer_kind IN ('user', 'core') AND issuer_tool_request_id IS NULL)
        OR (issuer_kind = 'tool' AND issuer_tool_request_id IS NOT NULL)
    );

ALTER TABLE model_call_identity
    DROP CONSTRAINT model_call_identity_kind_closed,
    ADD CONSTRAINT model_call_identity_kind_closed CHECK (
        call_kind IN ('ordinary', 'context_compaction', 'approval_judge', 'session_title')
    );

CREATE TABLE session_title_model_call (
    model_call_id uuid PRIMARY KEY,
    session_id uuid NOT NULL REFERENCES session(session_id),
    direct_model_selection_id uuid NOT NULL,
    resolved_provider_model_identity_id uuid NOT NULL,
    credential_reference text NOT NULL,
    usage_input_includes_cache_tokens boolean NOT NULL,
    initial_for_turn uuid REFERENCES turn_lifecycle(turn_id),
    state_kind text NOT NULL CHECK (state_kind IN ('prepared', 'in_flight', 'terminal')),
    prepared_at timestamptz NOT NULL DEFAULT statement_timestamp(),
    in_flight_at timestamptz,
    terminal_at timestamptz,
    title text,
    abandoned boolean NOT NULL DEFAULT false,
    input_tokens numeric,
    output_tokens numeric,
    cache_creation_input_tokens numeric,
    cache_read_input_tokens numeric,
    CHECK ((state_kind = 'terminal') = (terminal_at IS NOT NULL)),
    CHECK (title IS NULL OR (state_kind = 'terminal' AND length(title) > 0)),
    CONSTRAINT session_title_abandoned_terminal CHECK (
        NOT abandoned OR (state_kind = 'terminal' AND title IS NULL)
    )
);
CREATE UNIQUE INDEX session_title_initial_call ON session_title_model_call(session_id)
    WHERE initial_for_turn IS NOT NULL AND NOT abandoned;
CREATE FUNCTION guard_session_title_call() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.state_kind = 'terminal'
       OR (NEW.model_call_id, NEW.session_id, NEW.direct_model_selection_id,
           NEW.resolved_provider_model_identity_id, NEW.credential_reference,
           NEW.usage_input_includes_cache_tokens, NEW.initial_for_turn, NEW.prepared_at)
          IS DISTINCT FROM
          (OLD.model_call_id, OLD.session_id, OLD.direct_model_selection_id,
           OLD.resolved_provider_model_identity_id, OLD.credential_reference,
           OLD.usage_input_includes_cache_tokens, OLD.initial_for_turn, OLD.prepared_at)
       OR NOT ((OLD.state_kind = 'prepared' AND NEW.state_kind IN ('in_flight', 'terminal'))
               OR (OLD.state_kind = 'in_flight' AND NEW.state_kind = 'terminal'))
    THEN
        RAISE EXCEPTION 'invalid session title call transition' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER session_title_call_transitions BEFORE UPDATE ON session_title_model_call
    FOR EACH ROW EXECUTE FUNCTION guard_session_title_call();
CREATE TRIGGER session_title_call_cannot_be_deleted BEFORE DELETE ON session_title_model_call
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER session_title_reserves_global_identity BEFORE INSERT ON session_title_model_call
    FOR EACH ROW EXECUTE FUNCTION reserve_model_call_identity('session_title');

ALTER TABLE web_usage_call_projection
    DROP CONSTRAINT web_usage_call_kind_closed,
    DROP CONSTRAINT web_usage_turn_shape,
    ADD CONSTRAINT web_usage_call_kind_closed CHECK (
        call_kind IN ('model_call', 'approval_judge', 'context_compaction', 'session_title')
    ),
    ADD CONSTRAINT web_usage_turn_shape CHECK (
        (call_kind IN ('context_compaction', 'session_title')) = (turn_id IS NULL)
    );

CREATE OR REPLACE FUNCTION require_web_usage_source_correlation() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    identity_kind text;
    projected_identity_kind text;
    source record;
BEGIN
    SELECT call_kind INTO identity_kind
      FROM model_call_identity
     WHERE model_call_id = NEW.model_call_id;
    projected_identity_kind := identity_kind;
    IF projected_identity_kind = 'ordinary' THEN
        projected_identity_kind := 'model_call';
    END IF;
    IF NEW.call_kind IS DISTINCT FROM projected_identity_kind THEN
        RAISE EXCEPTION
            'usage projection call kind % contradicts identity kind %',
            NEW.call_kind, identity_kind
            USING ERRCODE = '23514';
    END IF;
    IF identity_kind = 'ordinary' THEN
        SELECT session_id, turn_id, resolved_provider_model_identity_id,
               bounded_web_usage_profile(credential_reference)
                   AS credential_profile_label,
               usage_provenance_kind, usage_input_includes_cache_tokens,
               usage_input_tokens AS input_tokens,
               usage_output_tokens AS output_tokens,
               usage_cache_creation_input_tokens
                   AS cache_creation_input_tokens,
               usage_cache_read_input_tokens AS cache_read_input_tokens
          INTO source
          FROM model_call
         WHERE model_call_id = NEW.model_call_id
           AND state_kind = 'terminal';
    ELSIF identity_kind = 'approval_judge' THEN
        SELECT session_id, turn_id, resolved_provider_model_identity_id,
               bounded_web_usage_profile(credential_reference)
                   AS credential_profile_label,
               usage_provenance_kind, usage_input_includes_cache_tokens,
               input_tokens, output_tokens,
               cache_creation_input_tokens, cache_read_input_tokens
          INTO source
          FROM tool_approval_judge_model_call
         WHERE model_call_id = NEW.model_call_id
           AND state_kind = 'terminal';
    ELSIF identity_kind = 'session_title' THEN
        SELECT session_id, NULL::uuid AS turn_id,
               resolved_provider_model_identity_id,
               bounded_web_usage_profile(credential_reference) AS credential_profile_label,
               'reported' AS usage_provenance_kind, usage_input_includes_cache_tokens,
               input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens
          INTO source FROM session_title_model_call
         WHERE model_call_id = NEW.model_call_id AND state_kind = 'terminal';
    ELSE
        SELECT session_id, NULL::uuid AS turn_id,
               resolved_provider_model_identity_id,
               bounded_web_usage_profile(credential_reference)
                   AS credential_profile_label,
               'reported' AS usage_provenance_kind,
               usage_input_includes_cache_tokens,
               input_tokens, output_tokens,
               cache_creation_input_tokens, cache_read_input_tokens
          INTO source
          FROM context_compaction_model_call
         WHERE model_call_id = NEW.model_call_id
           AND state_kind = 'terminal';
    END IF;
    IF NOT FOUND THEN
        RAISE EXCEPTION
            'usage projection call % has no terminal source record',
            NEW.model_call_id
            USING ERRCODE = '23514';
    END IF;
    IF NEW.session_id IS DISTINCT FROM source.session_id THEN
        RAISE EXCEPTION
            'usage projection session % contradicts source session %',
            NEW.session_id, source.session_id
            USING ERRCODE = '23514';
    END IF;
    IF NEW.turn_id IS DISTINCT FROM source.turn_id THEN
        RAISE EXCEPTION
            'usage projection turn % contradicts source turn %',
            NEW.turn_id, source.turn_id
            USING ERRCODE = '23514';
    END IF;
    IF NEW.resolved_provider_model_identity_id
           IS DISTINCT FROM source.resolved_provider_model_identity_id
       OR NEW.credential_profile_label
           IS DISTINCT FROM source.credential_profile_label
       OR NEW.usage_provenance_kind
           IS DISTINCT FROM source.usage_provenance_kind
       OR NEW.usage_input_includes_cache_tokens
           IS DISTINCT FROM source.usage_input_includes_cache_tokens
       OR NEW.input_tokens IS DISTINCT FROM source.input_tokens
       OR NEW.output_tokens IS DISTINCT FROM source.output_tokens
       OR NEW.cache_creation_input_tokens
           IS DISTINCT FROM source.cache_creation_input_tokens
       OR NEW.cache_read_input_tokens
           IS DISTINCT FROM source.cache_read_input_tokens
    THEN
        RAISE EXCEPTION
            'usage projection evidence for call % contradicts its terminal source record',
            NEW.model_call_id
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE FUNCTION project_terminal_session_title_usage() RETURNS trigger
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
        NEW.model_call_id, 'session_title', NEW.session_id, NULL,
        NEW.resolved_provider_model_identity_id,
        bounded_web_usage_profile(NEW.credential_reference),
        'reported', NEW.usage_input_includes_cache_tokens,
        NEW.input_tokens, NEW.output_tokens,
        NEW.cache_creation_input_tokens, NEW.cache_read_input_tokens
    );
    RETURN NEW;
END;
$$;

CREATE TRIGGER session_title_projects_terminal_usage AFTER UPDATE ON session_title_model_call
    FOR EACH ROW WHEN (NEW.state_kind = 'terminal' AND OLD.state_kind <> 'terminal')
    EXECUTE FUNCTION project_terminal_session_title_usage();

ALTER TABLE credential_invocation_reservation
    DROP CONSTRAINT credential_invocation_reservation_model_call_id_fkey,
    ADD CONSTRAINT credential_invocation_reservation_model_call_id_fkey
        FOREIGN KEY (model_call_id) REFERENCES model_call_identity (model_call_id);

CREATE OR REPLACE FUNCTION guard_credential_invocation_reservation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        PERFORM 1 FROM credential_invocation_capacity WHERE profile = NEW.profile FOR UPDATE;
        IF EXISTS (SELECT 1 FROM credential_invocation_capacity capacity WHERE capacity.profile = NEW.profile
            AND capacity.max_concurrent_invocations <= (SELECT count(*) FROM credential_invocation_reservation
                WHERE profile = NEW.profile AND released_at IS NULL)) THEN
            RAISE EXCEPTION 'credential invocation bound is saturated' USING ERRCODE = '23514';
        END IF;
        IF NOT EXISTS (SELECT 1 FROM model_call WHERE model_call_id = NEW.model_call_id
            AND credential_reference = NEW.profile AND state_kind = 'prepared')
           AND NOT EXISTS (SELECT 1 FROM session_title_model_call WHERE model_call_id = NEW.model_call_id
            AND credential_reference = NEW.profile AND state_kind = 'prepared') THEN
            RAISE EXCEPTION 'invocation reservation lacks selected prepared call' USING ERRCODE = '23514';
        END IF;
    ELSIF TG_OP = 'DELETE' OR OLD.released_at IS NOT NULL
       OR (NEW.model_call_id, NEW.profile) IS DISTINCT FROM (OLD.model_call_id, OLD.profile)
       OR (OLD.process_group_id IS NOT NULL AND (NEW.process_group_id, NEW.process_group_start_time)
           IS DISTINCT FROM (OLD.process_group_id, OLD.process_group_start_time)) THEN
        RAISE EXCEPTION 'invocation reservation identity is immutable' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE FUNCTION release_terminal_title_reservation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.state_kind = 'terminal' AND NOT EXISTS (
        SELECT 1 FROM credential_invocation_reservation
         WHERE model_call_id = NEW.model_call_id AND process_group_id IS NOT NULL
    ) THEN
        PERFORM release_credential_invocation(NEW.model_call_id);
    END IF;
    RETURN NULL;
END;
$$;
CREATE TRIGGER session_title_invocation_terminal AFTER UPDATE OF state_kind ON session_title_model_call
    FOR EACH ROW EXECUTE FUNCTION release_terminal_title_reservation();
