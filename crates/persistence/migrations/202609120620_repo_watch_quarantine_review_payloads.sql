SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

DO $$
DECLARE
    candidate record;
BEGIN
    FOR candidate IN
        SELECT event_id, normalized_payload FROM gh_event
        WHERE event_kind = 'review_submitted' AND decode_error IS NULL
    LOOP
        BEGIN
            PERFORM convert_from(candidate.normalized_payload, 'UTF8')::jsonb;
        EXCEPTION WHEN data_exception THEN
            UPDATE gh_event
            SET decode_error = 'repository-watch retained event is invalid'
            WHERE event_id = candidate.event_id;
            RAISE WARNING 'repository-watch retained event is invalid: %', candidate.event_id;
        END;
    END LOOP;
END $$;

RESET search_path;
RESET ROLE;
