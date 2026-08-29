-- Retain the exact runner-authored failure that refuses one pending managed
-- workspace release. Other operation-failure correlation arms remain closed
-- until their own refused/no-execution transitions are implemented.

CREATE FUNCTION runner_failure_detail_json_is_valid(input_json json, depth integer)
RETURNS boolean
LANGUAGE plpgsql
IMMUTABLE
STRICT
AS $$
DECLARE
    kind text;
    member record;
    member_count integer;
    child_depth integer;
BEGIN
    kind := json_typeof(input_json);
    IF kind = 'object' THEN
        IF depth > 8 THEN
            RETURN false;
        END IF;
        SELECT count(*) INTO member_count FROM json_each(input_json);
        IF member_count > 64 THEN
            RETURN false;
        END IF;
        FOR member IN
            SELECT item.key, item.value FROM json_each(input_json) AS item
        LOOP
            IF octet_length(member.key) NOT BETWEEN 1 AND 64
               OR member.key !~ '^[A-Za-z0-9][A-Za-z0-9._-]*$'
            THEN
                RETURN false;
            END IF;
            child_depth := depth + CASE
                WHEN json_typeof(member.value) IN ('object', 'array') THEN 1
                ELSE 0
            END;
            IF NOT runner_failure_detail_json_is_valid(
                member.value,
                child_depth
            ) THEN
                RETURN false;
            END IF;
        END LOOP;
        RETURN true;
    ELSIF kind = 'array' THEN
        IF depth > 8 THEN
            RETURN false;
        END IF;
        SELECT count(*) INTO member_count FROM json_array_elements(input_json);
        IF member_count > 64 THEN
            RETURN false;
        END IF;
        FOR member IN
            SELECT item.element
              FROM json_array_elements(input_json) AS item(element)
        LOOP
            child_depth := depth + CASE
                WHEN json_typeof(member.element) IN ('object', 'array') THEN 1
                ELSE 0
            END;
            IF NOT runner_failure_detail_json_is_valid(
                member.element,
                child_depth
            ) THEN
                RETURN false;
            END IF;
        END LOOP;
        RETURN true;
    ELSIF kind = 'string' THEN
        -- PostgreSQL text cannot contain U+0000, so substitute an equal-width
        -- JSON escape before materializing the decoded string for its bound.
        RETURN octet_length(
            replace(
                input_json::text,
                chr(92) || 'u0000',
                chr(92) || 'u0001'
            )::json #>> '{}'
        ) <= 1024;
    ELSIF kind = 'number' THEN
        RETURN input_json::text ~ '^(0|[1-9][0-9]*)$'
           AND input_json::text::numeric <= 18446744073709551615;
    END IF;
    RETURN kind IN ('boolean', 'null');
END;
$$;

CREATE TABLE runner_operation_failure (
    operation_kind text NOT NULL,
    runner_id uuid NOT NULL,
    release_session_id uuid NOT NULL,
    release_placement_revision numeric(20, 0) NOT NULL,
    release_manifest_id uuid NOT NULL,
    category_kind text NOT NULL,
    detail_code text NOT NULL,
    detail_message text NOT NULL,
    detail_payload_json text NOT NULL,

    CONSTRAINT runner_operation_failure_pk PRIMARY KEY (
        operation_kind,
        release_session_id,
        release_placement_revision
    ),
    CONSTRAINT runner_operation_failure_release_correlation_key UNIQUE (
        operation_kind,
        release_session_id,
        release_placement_revision,
        runner_id,
        release_manifest_id
    ),
    CONSTRAINT runner_operation_failure_release_arm CHECK (
        operation_kind = 'workspace_release'
        AND category_kind = 'workspace_cleanup_failed'
        AND release_placement_revision BETWEEN 1 AND 18446744073709551615
    ),
    CONSTRAINT runner_operation_failure_detail_code CHECK (
        octet_length(detail_code) BETWEEN 1 AND 64
        AND detail_code ~ '^[A-Za-z0-9][A-Za-z0-9._-]*$'
    ),
    CONSTRAINT runner_operation_failure_detail_message CHECK (
        octet_length(detail_message) BETWEEN 1 AND 1024
    ),
    CONSTRAINT runner_operation_failure_detail_payload CHECK (
        octet_length(detail_payload_json) <= 2048
        AND json_typeof(detail_payload_json::json) = 'object'
        AND runner_failure_detail_json_is_valid(
            detail_payload_json::json,
            1
        )
    ),
    CONSTRAINT runner_operation_failure_detail_total CHECK (
        octet_length(
            '{"code":' || to_json(detail_code)::text
            || ',"message":' || to_json(detail_message)::text
            || ',"payload":' || detail_payload_json || '}'
        ) <= 4096
    ),
    CONSTRAINT runner_operation_failure_release_fk FOREIGN KEY (
        release_session_id,
        release_placement_revision,
        runner_id,
        release_manifest_id
    ) REFERENCES runner_workspace_release (
        session_id,
        placement_revision,
        runner_id,
        manifest_id
    )
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION require_runner_workspace_cleanup_failure_authority()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    release runner_workspace_release%ROWTYPE;
BEGIN
    SELECT * INTO release
      FROM runner_workspace_release
     WHERE session_id = NEW.release_session_id
       AND placement_revision = NEW.release_placement_revision
       FOR UPDATE;

    IF NEW.operation_kind <> 'workspace_release'
       OR NEW.category_kind <> 'workspace_cleanup_failed'
       OR release.state_kind IS DISTINCT FROM 'pending'
       OR release.runner_id IS DISTINCT FROM NEW.runner_id
       OR release.manifest_id IS DISTINCT FROM NEW.release_manifest_id
       OR EXISTS (
            SELECT 1
              FROM runner_connection_loss_epoch AS loss
             WHERE loss.enrollment_id = release.enrollment_id
               AND loss.connection_epoch = release.connection_epoch
       )
       OR EXISTS (
            SELECT 1
              FROM runner_workspace_release_acknowledgement AS acknowledgement
             WHERE acknowledgement.session_id = NEW.release_session_id
               AND acknowledgement.placement_revision =
                    NEW.release_placement_revision
       )
       OR EXISTS (
            SELECT 1
              FROM runner_workspace_release_loss_retirement AS retirement
             WHERE retirement.session_id = NEW.release_session_id
               AND retirement.placement_revision =
                    NEW.release_placement_revision
       )
    THEN
        RAISE EXCEPTION 'workspace cleanup failure lacks live pending release authority'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_workspace_cleanup_failure_is_checked
AFTER INSERT ON runner_operation_failure
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_workspace_cleanup_failure_authority();

CREATE TRIGGER runner_operation_failure_is_append_only
BEFORE UPDATE OR DELETE ON runner_operation_failure
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_operation_failure_rejects_truncate
BEFORE TRUNCATE ON runner_operation_failure
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();

-- Completion, source loss, and cleanup refusal are the only mutually
-- exclusive terminal proofs for a release.
CREATE OR REPLACE FUNCTION require_runner_workspace_release_acknowledgement_authority()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    release runner_workspace_release%ROWTYPE;
BEGIN
    SELECT * INTO release
      FROM runner_workspace_release
     WHERE session_id = NEW.session_id
       AND placement_revision = NEW.placement_revision
       FOR UPDATE;

    IF release.state_kind IS DISTINCT FROM 'pending'
       OR release.runner_id IS DISTINCT FROM NEW.runner_id
       OR release.manifest_id IS DISTINCT FROM NEW.manifest_id
       OR EXISTS (
            SELECT 1
              FROM runner_connection_loss_epoch AS loss
             WHERE loss.enrollment_id = release.enrollment_id
               AND loss.connection_epoch = release.connection_epoch
       )
       OR EXISTS (
            SELECT 1
              FROM runner_workspace_release_loss_retirement AS retirement
             WHERE retirement.session_id = NEW.session_id
               AND retirement.placement_revision = NEW.placement_revision
       )
       OR EXISTS (
            SELECT 1
              FROM runner_operation_failure AS failure
             WHERE failure.operation_kind = 'workspace_release'
               AND failure.release_session_id = NEW.session_id
               AND failure.release_placement_revision = NEW.placement_revision
       )
    THEN
        RAISE EXCEPTION 'workspace release acknowledgement lacks live pending authority'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE OR REPLACE FUNCTION require_runner_workspace_release_loss_retirement_authority()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    release runner_workspace_release%ROWTYPE;
    loss runner_connection_loss_epoch%ROWTYPE;
BEGIN
    SELECT * INTO release
      FROM runner_workspace_release
     WHERE session_id = NEW.session_id
       AND placement_revision = NEW.placement_revision
       FOR UPDATE;
    SELECT * INTO loss
      FROM runner_connection_loss_epoch
     WHERE enrollment_id = NEW.enrollment_id
       AND loss_epoch = NEW.loss_epoch;

    IF release.state_kind IS DISTINCT FROM 'pending'
       OR release.runner_id IS DISTINCT FROM NEW.runner_id
       OR release.manifest_id IS DISTINCT FROM NEW.manifest_id
       OR release.enrollment_id IS DISTINCT FROM NEW.enrollment_id
       OR release.connection_epoch IS DISTINCT FROM NEW.connection_epoch
       OR loss.connection_epoch IS DISTINCT FROM NEW.connection_epoch
       OR loss.connection_event_ordinal IS DISTINCT FROM
            NEW.connection_event_ordinal
       OR EXISTS (
            SELECT 1
              FROM runner_workspace_release_acknowledgement AS acknowledgement
             WHERE acknowledgement.session_id = NEW.session_id
               AND acknowledgement.placement_revision = NEW.placement_revision
       )
       OR EXISTS (
            SELECT 1
              FROM runner_operation_failure AS failure
             WHERE failure.operation_kind = 'workspace_release'
               AND failure.release_session_id = NEW.session_id
               AND failure.release_placement_revision = NEW.placement_revision
       )
    THEN
        RAISE EXCEPTION 'workspace release loss retirement lacks exact pending authority'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE OR REPLACE FUNCTION guard_runner_connection_loss_release_propagation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF (
        NEW.state_kind = 'completed'
        OR (
            NEW.state_kind = 'pending'
            AND NEW.propagated_through_session_id IS NOT NULL
        )
    )
       AND EXISTS (
            SELECT 1
              FROM runner_workspace_release AS release
              JOIN runner_connection_loss_epoch AS loss
                ON loss.enrollment_id = NEW.enrollment_id
               AND loss.loss_epoch = NEW.loss_epoch
              LEFT JOIN runner_workspace_release_acknowledgement AS acknowledgement
                ON acknowledgement.session_id = release.session_id
               AND acknowledgement.placement_revision =
                    release.placement_revision
              LEFT JOIN runner_workspace_release_loss_retirement AS retirement
                ON retirement.session_id = release.session_id
               AND retirement.placement_revision = release.placement_revision
              LEFT JOIN runner_operation_failure AS failure
                ON failure.operation_kind = 'workspace_release'
               AND failure.release_session_id = release.session_id
               AND failure.release_placement_revision =
                    release.placement_revision
             WHERE release.enrollment_id = NEW.enrollment_id
               AND release.connection_epoch = loss.connection_epoch
               AND acknowledgement.session_id IS NULL
               AND retirement.session_id IS NULL
               AND failure.release_session_id IS NULL
               AND (
                    NEW.state_kind = 'completed'
                    OR release.session_id <=
                        NEW.propagated_through_session_id
               )
       )
    THEN
        RAISE EXCEPTION 'runner connection loss cursor skipped a pending workspace release'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;
