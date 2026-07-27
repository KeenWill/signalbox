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
    prior_grant_runner uuid;
    prior_grant_revision numeric;
BEGIN
    IF NEW.state_kind IN ('pinned', 'runner_lost')
       AND NEW.pinned_credential_profile_name IS NULL
       AND NEW.credential_grant_revision IS NOT NULL
    THEN
        IF NOT EXISTS (
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
        ) THEN
            RAISE EXCEPTION 'profileless grant authority must be a revoked tombstone'
                USING ERRCODE = '23514';
        END IF;
        IF NEW.event_kind = 'runner_replaced' THEN
            SELECT credential_grant_runner_id, credential_grant_revision
              INTO prior_grant_runner, prior_grant_revision
              FROM runner_session_placement_record
             WHERE session_id = NEW.session_id
               AND event_ordinal = NEW.event_ordinal - 1;
            IF NOT FOUND
               OR NEW.credential_grant_runner_id IS DISTINCT FROM
                    prior_grant_runner
               OR NEW.credential_grant_revision IS DISTINCT FROM
                    prior_grant_revision + 1
            THEN
                RAISE EXCEPTION 'profileless grant tombstone does not succeed the immediate prior grant'
                    USING ERRCODE = '23514';
            END IF;
        ELSIF NEW.event_kind <> 'runner_lost' THEN
            RAISE EXCEPTION 'profileless grant tombstone lacks a canonical transition'
                USING ERRCODE = '23514';
        END IF;
    END IF;

    IF NEW.event_kind = 'runner_lost' THEN
        SELECT credential_grant_runner_id, credential_grant_revision
          INTO prior_grant_runner, prior_grant_revision
          FROM runner_session_placement_record
         WHERE session_id = NEW.session_id
           AND event_ordinal = NEW.event_ordinal - 1;
        IF NEW.credential_grant_runner_id IS DISTINCT FROM prior_grant_runner
           OR NEW.credential_grant_revision IS DISTINCT FROM
                prior_grant_revision
        THEN
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


-- Recheck grant completeness against the lineage pointer added above.
CREATE OR REPLACE FUNCTION assert_runner_grant_complete(
    checked_session uuid,
    checked_runner uuid,
    checked_revision numeric
)
RETURNS void
LANGUAGE plpgsql
AS $function$
DECLARE
    grant_row runner_credential_grant%ROWTYPE;
    actual_tools bigint;
    invalid_tools bigint;
    initial_audit bigint;
BEGIN
    SELECT * INTO grant_row
      FROM runner_credential_grant
     WHERE session_id = checked_session
       AND runner_id = checked_runner
       AND grant_revision = checked_revision;
    IF NOT FOUND THEN
        RETURN;
    END IF;
    SELECT count(*) INTO actual_tools
      FROM runner_credential_grant_tool
     WHERE session_id = checked_session
       AND runner_id = checked_runner
       AND grant_revision = checked_revision;
    SELECT count(*) INTO invalid_tools
      FROM runner_credential_grant_tool AS granted
      LEFT JOIN runner_registration_profile_approval AS policy
        ON policy.enrollment_id = grant_row.registration_enrollment_id
       AND policy.registration_revision = grant_row.registration_revision
       AND policy.credential_profile_name = grant_row.credential_profile_name
       AND policy.tool_name = granted.tool_name
      LEFT JOIN runner_registration_tool AS available
        ON available.enrollment_id = grant_row.registration_enrollment_id
       AND available.registration_revision = grant_row.registration_revision
       AND available.tool_name = granted.tool_name
     WHERE granted.session_id = checked_session
       AND granted.runner_id = checked_runner
       AND granted.grant_revision = checked_revision
       AND (
            available.tool_name IS NULL
            OR (
                policy.tool_name IS NULL
                AND granted.approval_kind <> $kind$session_policy$kind$
            )
            OR (
                policy.tool_name IS NOT NULL
                AND policy.approval_kind <> granted.approval_kind
            )
       );
    SELECT count(*) INTO initial_audit
      FROM runner_credential_grant_audit
     WHERE session_id = checked_session
       AND runner_id = checked_runner
       AND grant_revision = checked_revision
       AND audit_ordinal = 1;
    IF grant_row.tool_count <> actual_tools
       OR invalid_tools <> 0
       OR initial_audit <> 1
       OR (
            grant_row.grant_revision > 1
            AND NOT EXISTS (
                SELECT 1
                  FROM runner_session_placement_record AS prior_placement
                 WHERE prior_placement.session_id = grant_row.session_id
                   AND prior_placement.event_ordinal =
                        grant_row.placement_event_ordinal - 1
                   AND prior_placement.credential_grant_runner_id =
                        grant_row.prior_runner_id
                   AND prior_placement.credential_grant_revision =
                        grant_row.prior_grant_revision
            )
       )
       OR EXISTS (
            SELECT 1
              FROM runner_session_placement_record AS placement
             WHERE placement.session_id = grant_row.session_id
               AND placement.event_ordinal = grant_row.placement_event_ordinal
               AND placement.event_kind IN (
                    $kind$pinned$kind$,
                    $kind$runner_replaced$kind$
               )
               AND placement.pinned_credential_profile_name IS NOT NULL
               AND EXISTS (
                    SELECT 1
                      FROM runner_registration_tool AS available
                     WHERE available.enrollment_id =
                            grant_row.registration_enrollment_id
                       AND available.registration_revision =
                            grant_row.registration_revision
                       AND NOT EXISTS (
                            SELECT 1
                              FROM runner_credential_grant_tool AS granted
                             WHERE granted.session_id = grant_row.session_id
                               AND granted.runner_id = grant_row.runner_id
                               AND granted.grant_revision =
                                    grant_row.grant_revision
                               AND granted.tool_name = available.tool_name
                       )
               )
       )
       OR NOT EXISTS (
            SELECT 1
              FROM runner_session_placement_record AS placement
             WHERE placement.session_id = grant_row.session_id
               AND placement.event_ordinal = grant_row.placement_event_ordinal
               AND placement.state_kind = $kind$pinned$kind$
               AND placement.credential_grant_runner_id = grant_row.runner_id
               AND placement.credential_grant_revision =
                    grant_row.grant_revision
               AND (
                    (
                        placement.pinned_runner_id = grant_row.runner_id
                        AND placement.registration_enrollment_id =
                            grant_row.registration_enrollment_id
                        AND placement.registration_revision =
                            grant_row.registration_revision
                        AND placement.pinned_credential_profile_name =
                            grant_row.credential_profile_name
                    )
                    OR (
                        placement.pinned_credential_profile_name IS NULL
                        AND EXISTS (
                            SELECT 1
                              FROM runner_credential_grant_audit AS revoked
                             WHERE revoked.session_id = grant_row.session_id
                               AND revoked.runner_id = grant_row.runner_id
                               AND revoked.grant_revision =
                                    grant_row.grant_revision
                               AND revoked.event_kind = $kind$revoked$kind$
                        )
                    )
               )
       )
    THEN
        RAISE EXCEPTION $message$runner credential grant evidence is incomplete$message$
            USING ERRCODE = $code$23514$code$;
    END IF;
END;
$function$;
