-- Preserve credential-grant lineage across profileless runner replacements.

ALTER TABLE runner_session_placement_record
    ADD COLUMN credential_grant_runner_id uuid;

ALTER TABLE runner_session_placement_record
    DISABLE TRIGGER runner_session_placement_record_is_append_only;

UPDATE runner_session_placement_record
   SET credential_grant_runner_id = pinned_runner_id
 WHERE credential_grant_revision IS NOT NULL;

ALTER TABLE runner_session_placement_record
    ENABLE TRIGGER runner_session_placement_record_is_append_only;

ALTER TABLE runner_session_placement_record
    DROP CONSTRAINT runner_session_placement_state_shape;

ALTER TABLE runner_session_placement_record
    ADD CONSTRAINT runner_session_placement_state_shape
        CHECK (
            (
                state_kind = 'unpinned'
                AND event_kind = 'created'
                AND event_ordinal = 1
                AND placement_revision = 1
                AND pinned_runner_id IS NULL
                AND pinned_working_directory IS NULL
                AND pinned_credential_profile_name IS NULL
                AND registration_enrollment_id IS NULL
                AND registration_revision IS NULL
                AND pinned_tool_count = 0
                AND workspace_repository_key IS NULL
                AND workspace_working_directory IS NULL
                AND credential_grant_runner_id IS NULL
                AND credential_grant_revision IS NULL
            )
            OR (
                state_kind IN ('pinned', 'runner_lost')
                AND pinned_runner_id IS NOT NULL
                AND pinned_working_directory IS NOT NULL
                AND pinned_credential_profile_name IS NOT DISTINCT FROM
                    requested_credential_profile_name
                AND registration_enrollment_id IS NOT NULL
                AND registration_revision IS NOT NULL
                AND (
                    (
                        pinned_credential_profile_name IS NULL
                        AND (
                            (
                                credential_grant_runner_id IS NULL
                                AND credential_grant_revision IS NULL
                            )
                            OR (
                                credential_grant_runner_id IS NOT NULL
                                AND credential_grant_revision IS NOT NULL
                            )
                        )
                    )
                    OR (
                        pinned_credential_profile_name IS NOT NULL
                        AND credential_grant_runner_id = pinned_runner_id
                        AND credential_grant_revision IS NOT NULL
                    )
                )
            )
        ),
    ADD CONSTRAINT runner_session_placement_grant_pointer_shape
        CHECK (
            (credential_grant_runner_id IS NULL) =
                (credential_grant_revision IS NULL)
        ),
    ADD CONSTRAINT runner_session_placement_grant_fk
        FOREIGN KEY (
            session_id,
            credential_grant_runner_id,
            credential_grant_revision
        )
        REFERENCES runner_credential_grant (
            session_id,
            runner_id,
            grant_revision
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION require_runner_profileless_grant_tombstone()
RETURNS trigger
LANGUAGE plpgsql
AS $function$
DECLARE
    prior_runner uuid;
BEGIN
    IF NEW.state_kind IN ('pinned', 'runner_lost')
       AND NEW.pinned_credential_profile_name IS NULL
       AND NEW.credential_grant_revision IS NOT NULL
       AND NOT EXISTS (
            SELECT 1
              FROM runner_credential_grant AS grant_record
              JOIN runner_credential_grant_audit AS audit
                ON audit.session_id = grant_record.session_id
               AND audit.runner_id = grant_record.runner_id
               AND audit.grant_revision = grant_record.grant_revision
               AND audit.event_kind = 'revoked'
             WHERE grant_record.session_id = NEW.session_id
               AND grant_record.runner_id = NEW.credential_grant_runner_id
               AND grant_record.grant_revision = NEW.credential_grant_revision
       )
    THEN
        RAISE EXCEPTION 'profileless grant authority must be a revoked tombstone'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.event_kind = 'runner_lost' THEN
        SELECT credential_grant_runner_id INTO prior_runner
          FROM runner_session_placement_record
         WHERE session_id = NEW.session_id
           AND event_ordinal = NEW.event_ordinal - 1;
        IF NEW.credential_grant_runner_id IS DISTINCT FROM prior_runner THEN
            RAISE EXCEPTION 'runner loss changed grant authority'
                USING ERRCODE = '23514';
        END IF;
    END IF;
    RETURN NEW;
END;
$function$;

CREATE CONSTRAINT TRIGGER runner_profileless_grant_is_terminal
AFTER INSERT ON runner_session_placement_record
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_profileless_grant_tombstone();
