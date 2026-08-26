-- A single state transition can touch several rows whose deferred constraint
-- triggers all validate the same final turn, model call, or tool round. Every
-- trigger still fires at commit, but an identical validator runs only once in
-- that transaction after all writes have reached their final state.

CREATE FUNCTION claim_deferred_final_state_validation(
    validation_kind text,
    checked_identity uuid
)
RETURNS boolean
LANGUAGE plpgsql
AS $$
DECLARE
    claims text := COALESCE(
        current_setting(
            'signalbox.deferred_final_state_validation_claims',
            true
        ),
        ''
    );
    claim text;
BEGIN
    IF validation_kind NOT IN ('turn_lifecycle', 'model_call', 'tool_round')
    THEN
        RAISE EXCEPTION 'unsupported deferred validation kind %',
            validation_kind
            USING ERRCODE = '23514';
    END IF;

    claim := validation_kind || ':' || checked_identity::text || ';';
    IF strpos(claims, claim) <> 0 THEN
        RETURN false;
    END IF;

    PERFORM set_config(
        'signalbox.deferred_final_state_validation_claims',
        claims || claim,
        true
    );
    RETURN true;
END;
$$;

DO $migration$
DECLARE
    target record;
    definition text;
    updated_definition text;
    begin_marker CONSTANT text := E'\nBEGIN\n';
    begin_at integer;
    guard text;
BEGIN
    FOR target IN
        SELECT *
          FROM (
                VALUES
                    (
                        'assert_turn_lifecycle_final_state(uuid)',
                        'turn_lifecycle',
                        'checked_turn_id'
                    ),
                    (
                        'assert_model_call_final_state(uuid)',
                        'model_call',
                        'checked_model_call_id'
                    ),
                    (
                        'assert_tool_round_final_state(uuid)',
                        'tool_round',
                        'checked_model_call_id'
                    )
          ) AS configured(signature, validation_kind, identity_argument)
    LOOP
        SELECT pg_get_functiondef(target.signature::regprocedure)
          INTO definition;
        begin_at := strpos(definition, begin_marker);
        IF begin_at = 0 THEN
            RAISE EXCEPTION
                'deferred validation deduplication could not locate % body',
                target.signature;
        END IF;
        guard := format(
            E'    IF NOT claim_deferred_final_state_validation(%L, %I) THEN\n        RETURN;\n    END IF;\n\n',
            target.validation_kind,
            target.identity_argument
        );
        updated_definition :=
            left(definition, begin_at + length(begin_marker) - 1)
            || guard
            || substr(definition, begin_at + length(begin_marker));
        EXECUTE updated_definition;
    END LOOP;
END;
$migration$;
