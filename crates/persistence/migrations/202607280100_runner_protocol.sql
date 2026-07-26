-- Durable runner enrollment, validated availability, affinity, grants, and leases.

CREATE DOMAIN runner_catalog_name AS text
CHECK (
    octet_length(VALUE) BETWEEN 1 AND 64
    AND VALUE ~ '^[A-Za-z0-9][A-Za-z0-9._-]*$'
);

CREATE DOMAIN runner_exact_text AS text
CHECK (
    octet_length(VALUE) BETWEEN 1 AND 4096
);

CREATE DOMAIN runner_tool_schema AS text
CHECK (
    octet_length(VALUE) BETWEEN 1 AND 1048576
);

ALTER TABLE tool_attempt
    DROP CONSTRAINT tool_attempt_request_id_key;

CREATE INDEX tool_attempt_request_id_idx
    ON tool_attempt (request_id);

CREATE TABLE runner_enrollment_audit (
    enrollment_id uuid NOT NULL,
    revision numeric(20, 0) NOT NULL,
    runner_id uuid NOT NULL,
    authentication_reference_id uuid NOT NULL,
    allowed_class_count numeric(20, 0) NOT NULL,
    state_kind text NOT NULL,

    CONSTRAINT runner_enrollment_audit_pk
        PRIMARY KEY (enrollment_id, revision),
    CONSTRAINT runner_enrollment_audit_revision_positive_u64
        CHECK (
            revision BETWEEN 1 AND 18446744073709551615
            AND allowed_class_count BETWEEN 0 AND 18446744073709551615
        ),
    CONSTRAINT runner_enrollment_audit_identity_key
        UNIQUE (
            enrollment_id,
            revision,
            runner_id,
            authentication_reference_id,
            allowed_class_count
        ),
    CONSTRAINT runner_enrollment_audit_state_closed
        CHECK (state_kind IN ('active', 'revoked')),
    CONSTRAINT runner_enrollment_audit_state_shape
        CHECK (
            (revision = 1 AND state_kind = 'active')
            OR (revision = 2 AND state_kind = 'revoked')
        )
);

CREATE TABLE runner_enrollment (
    enrollment_id uuid PRIMARY KEY,
    runner_id uuid NOT NULL UNIQUE,
    authentication_reference_id uuid NOT NULL UNIQUE,
    allowed_class_count numeric(20, 0) NOT NULL,
    revision numeric(20, 0) NOT NULL,
    state_kind text NOT NULL,

    CONSTRAINT runner_enrollment_identity_key
        UNIQUE (
            enrollment_id,
            runner_id,
            authentication_reference_id
        ),
    CONSTRAINT runner_enrollment_class_count_u64
        CHECK (
            allowed_class_count BETWEEN 0 AND 18446744073709551615
        ),
    CONSTRAINT runner_enrollment_state_shape
        CHECK (
            (revision = 1 AND state_kind = 'active')
            OR (revision = 2 AND state_kind = 'revoked')
        ),
    CONSTRAINT runner_enrollment_audit_fk
        FOREIGN KEY (
            enrollment_id,
            revision,
            runner_id,
            authentication_reference_id,
            allowed_class_count
        )
        REFERENCES runner_enrollment_audit (
            enrollment_id,
            revision,
            runner_id,
            authentication_reference_id,
            allowed_class_count
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE runner_enrollment_audit_allowed_class (
    enrollment_id uuid NOT NULL,
    revision numeric(20, 0) NOT NULL,
    capability_class runner_catalog_name NOT NULL,

    CONSTRAINT runner_enrollment_audit_allowed_class_pk
        PRIMARY KEY (enrollment_id, revision, capability_class),
    CONSTRAINT runner_enrollment_audit_allowed_class_audit_fk
        FOREIGN KEY (enrollment_id, revision)
        REFERENCES runner_enrollment_audit (enrollment_id, revision)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE runner_enrollment_allowed_class (
    enrollment_id uuid NOT NULL,
    capability_class runner_catalog_name NOT NULL,

    CONSTRAINT runner_enrollment_allowed_class_pk
        PRIMARY KEY (enrollment_id, capability_class),
    CONSTRAINT runner_enrollment_allowed_class_fk
        FOREIGN KEY (enrollment_id)
        REFERENCES runner_enrollment (enrollment_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION guard_runner_enrollment_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.revision <> 1 OR NEW.state_kind <> 'active' THEN
            RAISE EXCEPTION 'runner enrollment must begin active at revision one'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'runner enrollment is not deletable'
            USING ERRCODE = '23514';
    END IF;
    IF ROW(
        OLD.enrollment_id,
        OLD.runner_id,
        OLD.authentication_reference_id,
        OLD.allowed_class_count
    ) IS DISTINCT FROM ROW(
        NEW.enrollment_id,
        NEW.runner_id,
        NEW.authentication_reference_id,
        NEW.allowed_class_count
    )
       OR OLD.revision <> 1
       OR OLD.state_kind <> 'active'
       OR NEW.revision <> 2
       OR NEW.state_kind <> 'revoked'
    THEN
        RAISE EXCEPTION 'runner enrollment transition is not terminal revocation'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_enrollment_changes_are_guarded
BEFORE INSERT OR UPDATE OR DELETE ON runner_enrollment
FOR EACH ROW
EXECUTE FUNCTION guard_runner_enrollment_change();

CREATE TRIGGER runner_enrollment_audit_is_append_only
BEFORE UPDATE OR DELETE ON runner_enrollment_audit
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_enrollment_allowed_class_is_append_only
BEFORE UPDATE OR DELETE ON runner_enrollment_allowed_class
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_enrollment_audit_allowed_class_is_append_only
BEFORE UPDATE OR DELETE ON runner_enrollment_audit_allowed_class
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION require_runner_enrollment_audit_installed()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    audit runner_enrollment_audit%ROWTYPE :=
        COALESCE(NEW, OLD);
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM runner_enrollment AS enrollment
         WHERE enrollment.enrollment_id = audit.enrollment_id
           AND enrollment.revision = audit.revision
           AND enrollment.runner_id = audit.runner_id
           AND enrollment.authentication_reference_id =
                audit.authentication_reference_id
           AND enrollment.allowed_class_count =
                audit.allowed_class_count
           AND enrollment.state_kind = audit.state_kind
    )
    THEN
        RAISE EXCEPTION 'runner enrollment audit is not canonically installed'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_enrollment_audit_requires_installation
AFTER INSERT OR UPDATE OR DELETE ON runner_enrollment_audit
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_enrollment_audit_installed();

CREATE FUNCTION require_runner_enrollment_complete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    checked_enrollment uuid :=
        COALESCE(NEW.enrollment_id, OLD.enrollment_id);
    declared_count numeric;
    actual_count bigint;
    audit_count bigint;
    mismatched_classes bigint;
BEGIN
    SELECT allowed_class_count INTO declared_count
      FROM runner_enrollment
     WHERE enrollment_id = checked_enrollment;
    IF NOT FOUND THEN
        RETURN NULL;
    END IF;
    SELECT count(*) INTO actual_count
      FROM runner_enrollment_allowed_class
     WHERE enrollment_id = checked_enrollment;
    SELECT count(*) INTO audit_count
      FROM runner_enrollment AS enrollment
      JOIN runner_enrollment_audit_allowed_class AS audited
        ON audited.enrollment_id = enrollment.enrollment_id
       AND audited.revision = enrollment.revision
     WHERE enrollment.enrollment_id = checked_enrollment;
    SELECT count(*) INTO mismatched_classes
      FROM (
            (
                SELECT capability_class
                  FROM runner_enrollment_allowed_class
                 WHERE enrollment_id = checked_enrollment
                EXCEPT
                SELECT audited.capability_class
                  FROM runner_enrollment AS enrollment
                  JOIN runner_enrollment_audit_allowed_class AS audited
                    ON audited.enrollment_id = enrollment.enrollment_id
                   AND audited.revision = enrollment.revision
                 WHERE enrollment.enrollment_id = checked_enrollment
            )
            UNION ALL
            (
                SELECT audited.capability_class
                  FROM runner_enrollment AS enrollment
                  JOIN runner_enrollment_audit_allowed_class AS audited
                    ON audited.enrollment_id = enrollment.enrollment_id
                   AND audited.revision = enrollment.revision
                 WHERE enrollment.enrollment_id = checked_enrollment
                EXCEPT
                SELECT capability_class
                  FROM runner_enrollment_allowed_class
                 WHERE enrollment_id = checked_enrollment
            )
      ) AS mismatch;
    IF declared_count <> actual_count
       OR declared_count <> audit_count
       OR mismatched_classes <> 0
    THEN
        RAISE EXCEPTION 'runner enrollment class inventory is incomplete'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_enrollment_requires_complete_classes
AFTER INSERT OR UPDATE OR DELETE ON runner_enrollment
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_enrollment_complete();

CREATE CONSTRAINT TRIGGER runner_enrollment_class_rechecks_inventory
AFTER INSERT OR UPDATE OR DELETE ON runner_enrollment_allowed_class
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_enrollment_complete();

CREATE CONSTRAINT TRIGGER runner_enrollment_audit_class_rechecks_inventory
AFTER INSERT OR UPDATE OR DELETE ON runner_enrollment_audit_allowed_class
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_enrollment_complete();

CREATE TABLE runner_registration (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20, 0) NOT NULL,
    runner_id uuid NOT NULL,
    authentication_reference_id uuid NOT NULL,
    class_count numeric(20, 0) NOT NULL,
    tool_count numeric(20, 0) NOT NULL,
    profile_count numeric(20, 0) NOT NULL,
    workspace_count numeric(20, 0) NOT NULL,

    CONSTRAINT runner_registration_pk
        PRIMARY KEY (enrollment_id, registration_revision),
    CONSTRAINT runner_registration_identity_key
        UNIQUE (
            enrollment_id,
            registration_revision,
            runner_id,
            authentication_reference_id
        ),
    CONSTRAINT runner_registration_runner_key
        UNIQUE (
            enrollment_id,
            registration_revision,
            runner_id
        ),
    CONSTRAINT runner_registration_revision_positive_u64
        CHECK (
            registration_revision BETWEEN 1 AND 18446744073709551615
        ),
    CONSTRAINT runner_registration_counts_u64
        CHECK (
            class_count BETWEEN 0 AND 18446744073709551615
            AND tool_count BETWEEN 0 AND 18446744073709551615
            AND profile_count BETWEEN 0 AND 18446744073709551615
            AND workspace_count BETWEEN 0 AND 18446744073709551615
        ),
    CONSTRAINT runner_registration_enrollment_fk
        FOREIGN KEY (
            enrollment_id,
            runner_id,
            authentication_reference_id
        )
        REFERENCES runner_enrollment (
            enrollment_id,
            runner_id,
            authentication_reference_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE TABLE runner_registration_class (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20, 0) NOT NULL,
    capability_class runner_catalog_name NOT NULL,

    CONSTRAINT runner_registration_class_pk
        PRIMARY KEY (
            enrollment_id,
            registration_revision,
            capability_class
        ),
    CONSTRAINT runner_registration_class_registration_fk
        FOREIGN KEY (enrollment_id, registration_revision)
        REFERENCES runner_registration (
            enrollment_id,
            registration_revision
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT runner_registration_class_enrollment_fk
        FOREIGN KEY (enrollment_id, capability_class)
        REFERENCES runner_enrollment_allowed_class (
            enrollment_id,
            capability_class
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE TABLE runner_registration_tool (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20, 0) NOT NULL,
    tool_name text NOT NULL,
    model_description runner_exact_text NOT NULL,
    model_input_schema runner_tool_schema NOT NULL,
    permission_kind text NOT NULL,
    effect_class text NOT NULL,
    loci_kind text NOT NULL,
    selector_kind text,
    selector_runner_id uuid,
    selector_capability_class runner_catalog_name,

    CONSTRAINT runner_registration_tool_pk
        PRIMARY KEY (
            enrollment_id,
            registration_revision,
            tool_name
        ),
    CONSTRAINT runner_registration_tool_name_shape
        CHECK (
            octet_length(tool_name) BETWEEN 1 AND 64
            AND tool_name ~ '^[A-Za-z0-9_-]+$'
        ),
    CONSTRAINT runner_registration_tool_permission_closed
        CHECK (permission_kind IN ('auto', 'confirm')),
    CONSTRAINT runner_registration_tool_effect_closed
        CHECK (effect_class IN ('pure', 'idempotent', 'side_effecting')),
    CONSTRAINT runner_registration_tool_loci_closed
        CHECK (loci_kind IN ('runner_only', 'daemon_or_runner')),
    CONSTRAINT runner_registration_tool_idempotent_runner_only
        CHECK (
            effect_class <> 'idempotent'
            OR loci_kind = 'runner_only'
        ),
    CONSTRAINT runner_registration_tool_selector_shape
        CHECK (
            (
                selector_kind = 'identity'
                AND selector_runner_id IS NOT NULL
                AND selector_capability_class IS NULL
            )
            OR (
                selector_kind = 'capability_class'
                AND selector_runner_id IS NULL
                AND selector_capability_class IS NOT NULL
            )
        ),
    CONSTRAINT runner_registration_tool_model_schema
        CHECK (
            left(model_input_schema, 1) = '{'
            AND jsonb_typeof(model_input_schema::jsonb) = 'object'
        ),
    CONSTRAINT runner_registration_tool_registration_fk
        FOREIGN KEY (enrollment_id, registration_revision)
        REFERENCES runner_registration (
            enrollment_id,
            registration_revision
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT runner_registration_tool_identity_selector_fk
        FOREIGN KEY (
            enrollment_id,
            registration_revision,
            selector_runner_id
        )
        REFERENCES runner_registration (
            enrollment_id,
            registration_revision,
            runner_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT runner_registration_tool_class_selector_fk
        FOREIGN KEY (
            enrollment_id,
            registration_revision,
            selector_capability_class
        )
        REFERENCES runner_registration_class (
            enrollment_id,
            registration_revision,
            capability_class
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE TABLE runner_registration_profile (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20, 0) NOT NULL,
    credential_profile_name runner_catalog_name NOT NULL,
    approval_count numeric(20, 0) NOT NULL,

    CONSTRAINT runner_registration_profile_pk
        PRIMARY KEY (
            enrollment_id,
            registration_revision,
            credential_profile_name
        ),
    CONSTRAINT runner_registration_profile_approval_count_u64
        CHECK (approval_count BETWEEN 0 AND 18446744073709551615),
    CONSTRAINT runner_registration_profile_registration_fk
        FOREIGN KEY (enrollment_id, registration_revision)
        REFERENCES runner_registration (
            enrollment_id,
            registration_revision
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE runner_registration_profile_approval (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20, 0) NOT NULL,
    credential_profile_name runner_catalog_name NOT NULL,
    tool_name text NOT NULL,
    approval_kind text NOT NULL,

    CONSTRAINT runner_registration_profile_approval_pk
        PRIMARY KEY (
            enrollment_id,
            registration_revision,
            credential_profile_name,
            tool_name
        ),
    CONSTRAINT runner_registration_profile_approval_closed
        CHECK (approval_kind IN ('automatic', 'session_policy')),
    CONSTRAINT runner_registration_profile_approval_profile_fk
        FOREIGN KEY (
            enrollment_id,
            registration_revision,
            credential_profile_name
        )
        REFERENCES runner_registration_profile (
            enrollment_id,
            registration_revision,
            credential_profile_name
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE runner_registration_workspace (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20, 0) NOT NULL,
    workspace_kind text NOT NULL,

    CONSTRAINT runner_registration_workspace_pk
        PRIMARY KEY (
            enrollment_id,
            registration_revision,
            workspace_kind
        ),
    CONSTRAINT runner_registration_workspace_closed
        CHECK (workspace_kind = 'worktree_per_session'),
    CONSTRAINT runner_registration_workspace_registration_fk
        FOREIGN KEY (enrollment_id, registration_revision)
        REFERENCES runner_registration (
            enrollment_id,
            registration_revision
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE runner_current_registration (
    enrollment_id uuid PRIMARY KEY,
    registration_revision numeric(20, 0) NOT NULL,

    CONSTRAINT runner_current_registration_fk
        FOREIGN KEY (enrollment_id, registration_revision)
        REFERENCES runner_registration (
            enrollment_id,
            registration_revision
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION guard_runner_current_registration()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    latest_revision numeric(20, 0);
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'runner registration head is not deletable'
            USING ERRCODE = '23514';
    END IF;
    SELECT max(registration_revision) INTO latest_revision
      FROM runner_registration
     WHERE enrollment_id = NEW.enrollment_id;
    IF NEW.registration_revision IS DISTINCT FROM latest_revision
       OR (
            TG_OP = 'INSERT'
            AND NEW.registration_revision <> 1
            AND NOT EXISTS (
                SELECT 1
                  FROM runner_current_registration AS current_registration
                 WHERE current_registration.enrollment_id =
                        NEW.enrollment_id
            )
       )
       OR (
            TG_OP = 'UPDATE'
            AND (
                NEW.enrollment_id <> OLD.enrollment_id
                OR NEW.registration_revision <>
                    OLD.registration_revision + 1
            )
       )
    THEN
        RAISE EXCEPTION 'runner registration head must advance to latest'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_current_registration_advances
BEFORE INSERT OR UPDATE OR DELETE ON runner_current_registration
FOR EACH ROW
EXECUTE FUNCTION guard_runner_current_registration();

CREATE TRIGGER runner_registration_is_append_only
BEFORE UPDATE OR DELETE ON runner_registration
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_registration_class_is_append_only
BEFORE UPDATE OR DELETE ON runner_registration_class
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_registration_tool_is_append_only
BEFORE UPDATE OR DELETE ON runner_registration_tool
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_registration_profile_is_append_only
BEFORE UPDATE OR DELETE ON runner_registration_profile
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_registration_profile_approval_is_append_only
BEFORE UPDATE OR DELETE ON runner_registration_profile_approval
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_registration_workspace_is_append_only
BEFORE UPDATE OR DELETE ON runner_registration_workspace
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION guard_runner_registration_insert()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    enrollment_state text;
    latest_revision numeric;
BEGIN
    SELECT state_kind INTO enrollment_state
      FROM runner_enrollment
     WHERE enrollment_id = NEW.enrollment_id
     FOR SHARE;
    SELECT max(registration_revision) INTO latest_revision
      FROM runner_registration
     WHERE enrollment_id = NEW.enrollment_id;
    IF enrollment_state <> 'active'
       OR NEW.registration_revision <>
            COALESCE(latest_revision + 1, 1)
    THEN
        RAISE EXCEPTION 'runner registration lacks active successor authority'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_registration_insert_is_guarded
BEFORE INSERT ON runner_registration
FOR EACH ROW
EXECUTE FUNCTION guard_runner_registration_insert();

CREATE FUNCTION assert_runner_registration_complete(
    checked_enrollment uuid,
    checked_revision numeric
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    declared_classes numeric;
    declared_tools numeric;
    declared_profiles numeric;
    declared_workspaces numeric;
    actual_classes bigint;
    actual_tools bigint;
    actual_profiles bigint;
    actual_workspaces bigint;
    incomplete_profiles bigint;
BEGIN
    SELECT class_count, tool_count, profile_count, workspace_count
      INTO declared_classes, declared_tools, declared_profiles, declared_workspaces
      FROM runner_registration
     WHERE enrollment_id = checked_enrollment
       AND registration_revision = checked_revision;
    IF NOT FOUND THEN
        RETURN;
    END IF;
    SELECT count(*) INTO actual_classes
      FROM runner_registration_class
     WHERE enrollment_id = checked_enrollment
       AND registration_revision = checked_revision;
    SELECT count(*) INTO actual_tools
      FROM runner_registration_tool
     WHERE enrollment_id = checked_enrollment
       AND registration_revision = checked_revision;
    SELECT count(*) INTO actual_profiles
      FROM runner_registration_profile
     WHERE enrollment_id = checked_enrollment
       AND registration_revision = checked_revision;
    SELECT count(*) INTO actual_workspaces
      FROM runner_registration_workspace
     WHERE enrollment_id = checked_enrollment
       AND registration_revision = checked_revision;
    SELECT count(*) INTO incomplete_profiles
      FROM runner_registration_profile AS profile
     WHERE profile.enrollment_id = checked_enrollment
       AND profile.registration_revision = checked_revision
       AND profile.approval_count <> (
            SELECT count(*)
              FROM runner_registration_profile_approval AS approval
             WHERE approval.enrollment_id = profile.enrollment_id
               AND approval.registration_revision =
                    profile.registration_revision
               AND approval.credential_profile_name =
                    profile.credential_profile_name
       );
    IF ROW(
        declared_classes,
        declared_tools,
        declared_profiles,
        declared_workspaces
    ) IS DISTINCT FROM ROW(
        actual_classes,
        actual_tools,
        actual_profiles,
        actual_workspaces
    )
       OR incomplete_profiles <> 0
    THEN
        RAISE EXCEPTION 'runner registration inventory is incomplete'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION require_runner_registration_complete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM assert_runner_registration_complete(
        COALESCE(NEW.enrollment_id, OLD.enrollment_id),
        COALESCE(NEW.registration_revision, OLD.registration_revision)
    );

    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_registration_requires_complete_inventory
AFTER INSERT OR UPDATE OR DELETE ON runner_registration
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_registration_complete();

CREATE CONSTRAINT TRIGGER runner_registration_class_rechecks_inventory
AFTER INSERT OR UPDATE OR DELETE ON runner_registration_class
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_registration_complete();

CREATE CONSTRAINT TRIGGER runner_registration_tool_rechecks_inventory
AFTER INSERT OR UPDATE OR DELETE ON runner_registration_tool
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_registration_complete();

CREATE CONSTRAINT TRIGGER runner_registration_profile_rechecks_inventory
AFTER INSERT OR UPDATE OR DELETE ON runner_registration_profile
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_registration_complete();

CREATE CONSTRAINT TRIGGER runner_registration_profile_approval_rechecks_inventory
AFTER INSERT OR UPDATE OR DELETE ON runner_registration_profile_approval
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_registration_complete();

CREATE CONSTRAINT TRIGGER runner_registration_workspace_rechecks_inventory
AFTER INSERT OR UPDATE OR DELETE ON runner_registration_workspace
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_registration_complete();

CREATE TABLE runner_session_placement_record (
    session_id uuid NOT NULL,
    event_ordinal numeric(20, 0) NOT NULL,
    placement_revision numeric(20, 0) NOT NULL,
    event_kind text NOT NULL,
    selector_kind text NOT NULL,
    selector_runner_id uuid,
    selector_capability_class runner_catalog_name,
    directory_selection_kind text NOT NULL,
    requested_working_directory runner_exact_text,
    requested_credential_profile_name runner_catalog_name,
    workspace_requirement_kind text NOT NULL,
    requested_repository_key runner_exact_text,
    state_kind text NOT NULL,
    pinned_runner_id uuid,
    pinned_working_directory runner_exact_text,
    pinned_credential_profile_name runner_catalog_name,
    registration_enrollment_id uuid,
    registration_revision numeric(20, 0),
    pinned_tool_count numeric(20, 0) NOT NULL,
    workspace_repository_key runner_exact_text,
    workspace_working_directory runner_exact_text,
    credential_grant_revision numeric(20, 0),

    CONSTRAINT runner_session_placement_record_pk
        PRIMARY KEY (session_id, event_ordinal),
    CONSTRAINT runner_session_placement_record_revision_key
        UNIQUE (session_id, event_ordinal, placement_revision),
    CONSTRAINT runner_session_placement_record_positive_u64
        CHECK (
            event_ordinal BETWEEN 1 AND 18446744073709551615
            AND placement_revision BETWEEN 1 AND 18446744073709551615
            AND pinned_tool_count BETWEEN 0 AND 18446744073709551615
            AND (
                credential_grant_revision IS NULL
                OR credential_grant_revision
                    BETWEEN 1 AND 18446744073709551615
            )
        ),
    CONSTRAINT runner_session_placement_event_closed
        CHECK (
            event_kind IN (
                'created',
                'pinned',
                'runner_lost',
                'runner_replaced',
                'profile_replaced'
            )
        ),
    CONSTRAINT runner_session_placement_selector_shape
        CHECK (
            (
                selector_kind = 'identity'
                AND selector_runner_id IS NOT NULL
                AND selector_capability_class IS NULL
            )
            OR (
                selector_kind = 'capability_class'
                AND selector_runner_id IS NULL
                AND selector_capability_class IS NOT NULL
            )
        ),
    CONSTRAINT runner_session_placement_directory_shape
        CHECK (
            (
                directory_selection_kind = 'runner_default'
                AND requested_working_directory IS NULL
            )
            OR (
                directory_selection_kind = 'exact'
                AND requested_working_directory IS NOT NULL
            )
        ),
    CONSTRAINT runner_session_placement_workspace_shape
        CHECK (
            (
                workspace_requirement_kind = 'none'
                AND requested_repository_key IS NULL
                AND workspace_repository_key IS NULL
                AND workspace_working_directory IS NULL
            )
            OR (
                workspace_requirement_kind = 'repository_worktree'
                AND requested_repository_key IS NOT NULL
                AND (
                    state_kind = 'unpinned'
                    OR (
                        workspace_repository_key IS NOT NULL
                        AND workspace_working_directory IS NOT NULL
                        AND workspace_repository_key =
                            requested_repository_key
                        AND workspace_working_directory =
                            pinned_working_directory
                    )
                )
            )
        ),
    CONSTRAINT runner_session_placement_state_shape
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
                        AND credential_grant_revision IS NULL
                    )
                    OR (
                        pinned_credential_profile_name IS NOT NULL
                        AND credential_grant_revision IS NOT NULL
                    )
                )
            )
        ),
    CONSTRAINT runner_session_placement_session_fk
        FOREIGN KEY (session_id)
        REFERENCES session (session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT runner_session_placement_registration_fk
        FOREIGN KEY (
            registration_enrollment_id,
            registration_revision,
            pinned_runner_id
        )
        REFERENCES runner_registration (
            enrollment_id,
            registration_revision,
            runner_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE TABLE runner_session_placement_tool (
    session_id uuid NOT NULL,
    event_ordinal numeric(20, 0) NOT NULL,
    tool_name text NOT NULL,
    runner_required boolean NOT NULL,

    CONSTRAINT runner_session_placement_tool_pk
        PRIMARY KEY (session_id, event_ordinal, tool_name),
    CONSTRAINT runner_session_placement_tool_record_fk
        FOREIGN KEY (session_id, event_ordinal)
        REFERENCES runner_session_placement_record (
            session_id,
            event_ordinal
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE runner_current_session_placement (
    session_id uuid PRIMARY KEY,
    event_ordinal numeric(20, 0) NOT NULL,

    CONSTRAINT runner_current_session_placement_fk
        FOREIGN KEY (session_id, event_ordinal)
        REFERENCES runner_session_placement_record (
            session_id,
            event_ordinal
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION guard_runner_current_placement()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    latest_ordinal numeric(20, 0);
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'runner placement head is not deletable'
            USING ERRCODE = '23514';
    END IF;
    SELECT max(event_ordinal) INTO latest_ordinal
      FROM runner_session_placement_record
     WHERE session_id = NEW.session_id;
    IF NEW.event_ordinal IS DISTINCT FROM latest_ordinal
       OR (
            TG_OP = 'INSERT'
            AND NEW.event_ordinal <> 1
            AND NOT EXISTS (
                SELECT 1
                  FROM runner_current_session_placement AS current_placement
                 WHERE current_placement.session_id = NEW.session_id
            )
       )
       OR (
            TG_OP = 'UPDATE'
            AND (
                NEW.session_id <> OLD.session_id
                OR NEW.event_ordinal <> OLD.event_ordinal + 1
            )
       )
    THEN
        RAISE EXCEPTION 'runner placement head must advance to latest'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_current_session_placement_advances
BEFORE INSERT OR UPDATE OR DELETE ON runner_current_session_placement
FOR EACH ROW
EXECUTE FUNCTION guard_runner_current_placement();

CREATE TRIGGER runner_session_placement_record_is_append_only
BEFORE UPDATE OR DELETE ON runner_session_placement_record
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_session_placement_tool_is_append_only
BEFORE UPDATE OR DELETE ON runner_session_placement_tool
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION guard_runner_placement_record()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    prior runner_session_placement_record%ROWTYPE;
BEGIN
    IF NEW.event_ordinal = 1 THEN
        IF NEW.event_kind <> 'created'
           OR NEW.state_kind <> 'unpinned'
           OR NEW.placement_revision <> 1
        THEN
            RAISE EXCEPTION 'first runner placement must be created unpinned'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;
    SELECT *
      INTO prior
      FROM runner_session_placement_record
     WHERE session_id = NEW.session_id
       AND event_ordinal = NEW.event_ordinal - 1;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner placement history is not contiguous'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.event_kind = 'pinned' THEN
        IF prior.state_kind <> 'unpinned'
           OR NEW.state_kind <> 'pinned'
           OR NEW.placement_revision <> prior.placement_revision
           OR ROW(
                NEW.selector_kind,
                NEW.selector_runner_id,
                NEW.selector_capability_class,
                NEW.directory_selection_kind,
                NEW.requested_working_directory,
                NEW.requested_credential_profile_name,
                NEW.workspace_requirement_kind,
                NEW.requested_repository_key
           ) IS DISTINCT FROM ROW(
                prior.selector_kind,
                prior.selector_runner_id,
                prior.selector_capability_class,
                prior.directory_selection_kind,
                prior.requested_working_directory,
                prior.requested_credential_profile_name,
                prior.workspace_requirement_kind,
                prior.requested_repository_key
           )
        THEN
            RAISE EXCEPTION 'runner placement pin is not canonical'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.event_kind = 'runner_lost' THEN
        IF prior.state_kind <> 'pinned'
           OR NEW.state_kind <> 'runner_lost'
           OR NEW.placement_revision <> prior.placement_revision
           OR ROW(
                NEW.pinned_runner_id,
                NEW.pinned_working_directory,
                NEW.pinned_credential_profile_name,
                NEW.registration_enrollment_id,
                NEW.registration_revision,
                NEW.pinned_tool_count,
                NEW.workspace_repository_key,
                NEW.workspace_working_directory,
                NEW.credential_grant_revision,
                NEW.selector_kind,
                NEW.selector_runner_id,
                NEW.selector_capability_class,
                NEW.directory_selection_kind,
                NEW.requested_working_directory,
                NEW.requested_credential_profile_name,
                NEW.workspace_requirement_kind,
                NEW.requested_repository_key
           ) IS DISTINCT FROM ROW(
                prior.pinned_runner_id,
                prior.pinned_working_directory,
                prior.pinned_credential_profile_name,
                prior.registration_enrollment_id,
                prior.registration_revision,
                prior.pinned_tool_count,
                prior.workspace_repository_key,
                prior.workspace_working_directory,
                prior.credential_grant_revision,
                prior.selector_kind,
                prior.selector_runner_id,
                prior.selector_capability_class,
                prior.directory_selection_kind,
                prior.requested_working_directory,
                prior.requested_credential_profile_name,
                prior.workspace_requirement_kind,
                prior.requested_repository_key
           )
        THEN
            RAISE EXCEPTION 'runner loss changed affinity facts'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.event_kind = 'runner_replaced' THEN
        IF prior.state_kind <> 'runner_lost'
           OR NEW.state_kind <> 'pinned'
           OR NEW.placement_revision <> prior.placement_revision + 1
           OR (
                NEW.credential_grant_revision IS NOT NULL
                AND NEW.credential_grant_revision IS DISTINCT FROM
                    COALESCE(prior.credential_grant_revision + 1, 1)
           )
        THEN
            RAISE EXCEPTION 'runner replacement is not a checked successor'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.event_kind = 'profile_replaced' THEN
        IF prior.state_kind <> 'pinned'
           OR NEW.state_kind <> 'pinned'
           OR NEW.placement_revision <> prior.placement_revision + 1
           OR NEW.pinned_runner_id <> prior.pinned_runner_id
           OR NEW.pinned_working_directory <>
                prior.pinned_working_directory
           OR NEW.registration_enrollment_id <>
                prior.registration_enrollment_id
           OR NEW.registration_revision <> prior.registration_revision
           OR NEW.credential_grant_revision IS DISTINCT FROM
                prior.credential_grant_revision + 1
           OR NEW.workspace_repository_key IS DISTINCT FROM
                prior.workspace_repository_key
           OR NEW.workspace_working_directory IS DISTINCT FROM
                prior.workspace_working_directory
           OR ROW(
                NEW.selector_kind,
                NEW.selector_runner_id,
                NEW.selector_capability_class,
                NEW.directory_selection_kind,
                NEW.requested_working_directory,
                NEW.workspace_requirement_kind,
                NEW.requested_repository_key
           ) IS DISTINCT FROM ROW(
                prior.selector_kind,
                prior.selector_runner_id,
                prior.selector_capability_class,
                prior.directory_selection_kind,
                prior.requested_working_directory,
                prior.workspace_requirement_kind,
                prior.requested_repository_key
           )
        THEN
            RAISE EXCEPTION 'credential profile replacement changed another axis'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        RAISE EXCEPTION 'created is only valid for the first placement record'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_session_placement_records_are_guarded
BEFORE INSERT ON runner_session_placement_record
FOR EACH ROW
EXECUTE FUNCTION guard_runner_placement_record();

CREATE FUNCTION assert_runner_placement_complete(
    checked_session uuid,
    checked_event numeric
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    placement runner_session_placement_record%ROWTYPE;
    actual_tools bigint;
    foreign_tools bigint;
BEGIN
    SELECT * INTO placement
      FROM runner_session_placement_record
     WHERE session_id = checked_session
       AND event_ordinal = checked_event;
    IF NOT FOUND THEN
        RETURN;
    END IF;
    SELECT count(*) INTO actual_tools
      FROM runner_session_placement_tool
     WHERE session_id = checked_session
       AND event_ordinal = checked_event;
    SELECT count(*) INTO foreign_tools
      FROM runner_session_placement_tool AS pinned
      LEFT JOIN runner_registration_tool AS registered
        ON registered.enrollment_id =
            placement.registration_enrollment_id
       AND registered.registration_revision =
            placement.registration_revision
       AND registered.tool_name = pinned.tool_name
     WHERE pinned.session_id = checked_session
       AND pinned.event_ordinal = checked_event
       AND registered.tool_name IS NULL;
    IF placement.pinned_tool_count <> actual_tools
       OR foreign_tools <> 0
       OR (
            placement.state_kind <> 'unpinned'
            AND (
                (
                    placement.selector_kind = 'identity'
                    AND placement.selector_runner_id <>
                        placement.pinned_runner_id
                )
                OR (
                    placement.selector_kind = 'capability_class'
                    AND NOT EXISTS (
                        SELECT 1
                          FROM runner_registration_class
                         WHERE enrollment_id =
                            placement.registration_enrollment_id
                           AND registration_revision =
                            placement.registration_revision
                           AND capability_class =
                            placement.selector_capability_class
                    )
                )
                OR (
                    placement.directory_selection_kind = 'exact'
                    AND placement.requested_working_directory <>
                        placement.pinned_working_directory
                )
                OR (
                    placement.pinned_credential_profile_name IS NOT NULL
                    AND (
                        NOT EXISTS (
                            SELECT 1
                              FROM runner_registration_profile
                             WHERE enrollment_id =
                                placement.registration_enrollment_id
                               AND registration_revision =
                                placement.registration_revision
                               AND credential_profile_name =
                                placement.pinned_credential_profile_name
                        )
                        OR NOT EXISTS (
                            SELECT 1
                              FROM runner_credential_grant AS grant_record
                             WHERE grant_record.session_id =
                                placement.session_id
                               AND grant_record.runner_id =
                                placement.pinned_runner_id
                               AND grant_record.grant_revision =
                                placement.credential_grant_revision
                               AND grant_record.credential_profile_name =
                                placement.pinned_credential_profile_name
                               AND grant_record.registration_enrollment_id =
                                placement.registration_enrollment_id
                               AND grant_record.registration_revision =
                                placement.registration_revision
                        )
                    )
                )
                OR (
                    placement.workspace_requirement_kind =
                        'repository_worktree'
                    AND NOT EXISTS (
                        SELECT 1
                          FROM runner_registration_workspace
                         WHERE enrollment_id =
                            placement.registration_enrollment_id
                           AND registration_revision =
                            placement.registration_revision
                           AND workspace_kind = 'worktree_per_session'
                    )
                )
            )
       )
       OR (
            placement.state_kind <> 'unpinned'
            AND actual_tools <> (
                SELECT count(*)
                  FROM runner_registration_tool
                 WHERE enrollment_id =
                    placement.registration_enrollment_id
                   AND registration_revision =
                    placement.registration_revision
            )
       )
    THEN
        RAISE EXCEPTION 'runner placement tool inventory is not canonical'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION require_runner_placement_complete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM assert_runner_placement_complete(
        COALESCE(NEW.session_id, OLD.session_id),
        COALESCE(NEW.event_ordinal, OLD.event_ordinal)
    );
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_session_placement_requires_tools
AFTER INSERT OR UPDATE OR DELETE ON runner_session_placement_record
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_placement_complete();

CREATE CONSTRAINT TRIGGER runner_session_placement_tool_rechecks_inventory
AFTER INSERT OR UPDATE OR DELETE ON runner_session_placement_tool
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_placement_complete();

CREATE TABLE runner_credential_grant (
    session_id uuid NOT NULL,
    runner_id uuid NOT NULL,
    grant_revision numeric(20, 0) NOT NULL,
    credential_profile_name runner_catalog_name NOT NULL,
    registration_enrollment_id uuid NOT NULL,
    registration_revision numeric(20, 0) NOT NULL,
    placement_event_ordinal numeric(20, 0) NOT NULL,
    prior_runner_id uuid,
    prior_grant_revision numeric(20, 0),
    tool_count numeric(20, 0) NOT NULL,

    CONSTRAINT runner_credential_grant_pk
        PRIMARY KEY (session_id, runner_id, grant_revision),
    CONSTRAINT runner_credential_grant_profile_key
        UNIQUE (
            session_id,
            runner_id,
            grant_revision,
            credential_profile_name
        ),
    CONSTRAINT runner_credential_grant_revision_shape
        CHECK (
            grant_revision BETWEEN 1 AND 18446744073709551615
            AND tool_count BETWEEN 0 AND 18446744073709551615
            AND (
                (
                    grant_revision = 1
                    AND prior_runner_id IS NULL
                    AND prior_grant_revision IS NULL
                )
                OR (
                    prior_runner_id IS NOT NULL
                    AND prior_grant_revision = grant_revision - 1
                )
            )
        ),
    CONSTRAINT runner_credential_grant_registration_profile_fk
        FOREIGN KEY (
            registration_enrollment_id,
            registration_revision,
            credential_profile_name
        )
        REFERENCES runner_registration_profile (
            enrollment_id,
            registration_revision,
            credential_profile_name
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT runner_credential_grant_placement_fk
        FOREIGN KEY (
            session_id,
            placement_event_ordinal
        )
        REFERENCES runner_session_placement_record (
            session_id,
            event_ordinal
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT runner_credential_grant_prior_fk
        FOREIGN KEY (
            session_id,
            prior_runner_id,
            prior_grant_revision
        )
        REFERENCES runner_credential_grant (
            session_id,
            runner_id,
            grant_revision
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE runner_credential_grant_tool (
    session_id uuid NOT NULL,
    runner_id uuid NOT NULL,
    grant_revision numeric(20, 0) NOT NULL,
    credential_profile_name runner_catalog_name NOT NULL,
    tool_name text NOT NULL,
    approval_kind text NOT NULL,

    CONSTRAINT runner_credential_grant_tool_pk
        PRIMARY KEY (
            session_id,
            runner_id,
            grant_revision,
            tool_name
        ),
    CONSTRAINT runner_credential_grant_tool_lease_key
        UNIQUE (
            session_id,
            runner_id,
            grant_revision,
            credential_profile_name,
            tool_name,
            approval_kind
        ),
    CONSTRAINT runner_credential_grant_tool_approval_closed
        CHECK (approval_kind IN ('automatic', 'session_policy')),
    CONSTRAINT runner_credential_grant_tool_grant_fk
        FOREIGN KEY (
            session_id,
            runner_id,
            grant_revision,
            credential_profile_name
        )
        REFERENCES runner_credential_grant (
            session_id,
            runner_id,
            grant_revision,
            credential_profile_name
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE runner_credential_grant_audit (
    session_id uuid NOT NULL,
    runner_id uuid NOT NULL,
    grant_revision numeric(20, 0) NOT NULL,
    audit_ordinal numeric(20, 0) NOT NULL,
    event_kind text NOT NULL,
    credential_profile_name runner_catalog_name NOT NULL,

    CONSTRAINT runner_credential_grant_audit_pk
        PRIMARY KEY (
            session_id,
            runner_id,
            grant_revision,
            audit_ordinal
        ),
    CONSTRAINT runner_credential_grant_audit_shape
        CHECK (
            (
                audit_ordinal = 1
                AND (
                    (
                        grant_revision = 1
                        AND event_kind = 'issued'
                    )
                    OR (
                        grant_revision > 1
                        AND event_kind = 'replaced'
                    )
                )
            )
            OR (
                audit_ordinal = 2
                AND event_kind = 'revoked'
            )
        ),
    CONSTRAINT runner_credential_grant_audit_grant_fk
        FOREIGN KEY (
            session_id,
            runner_id,
            grant_revision,
            credential_profile_name
        )
        REFERENCES runner_credential_grant (
            session_id,
            runner_id,
            grant_revision,
            credential_profile_name
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER runner_credential_grant_is_append_only
BEFORE UPDATE OR DELETE ON runner_credential_grant
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_credential_grant_tool_is_append_only
BEFORE UPDATE OR DELETE ON runner_credential_grant_tool
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_credential_grant_audit_is_append_only
BEFORE UPDATE OR DELETE ON runner_credential_grant_audit
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION assert_runner_grant_complete(
    checked_session uuid,
    checked_runner uuid,
    checked_revision numeric
)
RETURNS void
LANGUAGE plpgsql
AS $$
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
        ON policy.enrollment_id =
            grant_row.registration_enrollment_id
       AND policy.registration_revision =
            grant_row.registration_revision
       AND policy.credential_profile_name =
            grant_row.credential_profile_name
       AND policy.tool_name = granted.tool_name
       AND policy.approval_kind = granted.approval_kind
      LEFT JOIN runner_registration_tool AS available
        ON available.enrollment_id =
            grant_row.registration_enrollment_id
       AND available.registration_revision =
            grant_row.registration_revision
       AND available.tool_name = granted.tool_name
     WHERE granted.session_id = checked_session
       AND granted.runner_id = checked_runner
       AND granted.grant_revision = checked_revision
       AND (
            available.tool_name IS NULL
            OR (
                policy.tool_name IS NULL
                AND granted.approval_kind <> 'session_policy'
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
                 WHERE prior_placement.session_id =
                        grant_row.session_id
                   AND prior_placement.event_ordinal =
                        grant_row.placement_event_ordinal - 1
                   AND prior_placement.pinned_runner_id =
                        grant_row.prior_runner_id
                   AND prior_placement.credential_grant_revision =
                        grant_row.prior_grant_revision
            )
       )
       OR EXISTS (
            SELECT 1
              FROM runner_session_placement_record AS placement
             WHERE placement.session_id = grant_row.session_id
               AND placement.event_ordinal =
                    grant_row.placement_event_ordinal
               AND placement.event_kind IN (
                    'pinned',
                    'runner_replaced'
               )
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
                             WHERE granted.session_id =
                                    grant_row.session_id
                               AND granted.runner_id =
                                    grant_row.runner_id
                               AND granted.grant_revision =
                                    grant_row.grant_revision
                               AND granted.tool_name =
                                    available.tool_name
                       )
               )
       )
       OR NOT EXISTS (
            SELECT 1
              FROM runner_session_placement_record AS placement
             WHERE placement.session_id = grant_row.session_id
               AND placement.event_ordinal =
                    grant_row.placement_event_ordinal
               AND placement.state_kind = 'pinned'
               AND placement.pinned_runner_id = grant_row.runner_id
               AND placement.pinned_credential_profile_name =
                    grant_row.credential_profile_name
               AND placement.registration_enrollment_id =
                    grant_row.registration_enrollment_id
               AND placement.registration_revision =
                    grant_row.registration_revision
               AND placement.credential_grant_revision =
                    grant_row.grant_revision
       )
    THEN
        RAISE EXCEPTION 'runner credential grant evidence is incomplete'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION require_runner_grant_complete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM assert_runner_grant_complete(
        COALESCE(NEW.session_id, OLD.session_id),
        COALESCE(NEW.runner_id, OLD.runner_id),
        COALESCE(NEW.grant_revision, OLD.grant_revision)
    );
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_credential_grant_requires_complete_evidence
AFTER INSERT OR UPDATE OR DELETE ON runner_credential_grant
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_grant_complete();

CREATE CONSTRAINT TRIGGER runner_credential_grant_tool_rechecks_evidence
AFTER INSERT OR UPDATE OR DELETE ON runner_credential_grant_tool
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_grant_complete();

CREATE CONSTRAINT TRIGGER runner_credential_grant_audit_rechecks_evidence
AFTER INSERT OR UPDATE OR DELETE ON runner_credential_grant_audit
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_grant_complete();

CREATE TABLE runner_physical_attempt_lease_binding (
    attempt_id uuid PRIMARY KEY,
    lease_id uuid NOT NULL,

    CONSTRAINT runner_physical_attempt_lease_binding_attempt_fk
        FOREIGN KEY (attempt_id)
        REFERENCES tool_attempt (attempt_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE TABLE runner_tool_request_lease_binding (
    request_id uuid PRIMARY KEY,
    lease_id uuid NOT NULL,

    CONSTRAINT runner_tool_request_lease_binding_request_fk
        FOREIGN KEY (request_id)
        REFERENCES tool_request (request_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE TABLE runner_lease_generation (
    lease_id uuid NOT NULL,
    generation numeric(20, 0) NOT NULL,
    attempt_id uuid NOT NULL,
    session_id uuid NOT NULL,
    runner_id uuid NOT NULL,
    tool_name text NOT NULL,
    effect_class text NOT NULL,
    placement_event_ordinal numeric(20, 0) NOT NULL,
    registration_enrollment_id uuid NOT NULL,
    registration_revision numeric(20, 0) NOT NULL,
    credential_profile_name runner_catalog_name,
    credential_grant_revision numeric(20, 0),
    credential_approval_kind text,
    predecessor_generation numeric(20, 0),

    CONSTRAINT runner_lease_generation_pk
        PRIMARY KEY (lease_id, generation),
    CONSTRAINT runner_lease_generation_correlation_key
        UNIQUE (
            lease_id,
            generation,
            attempt_id,
            session_id,
            runner_id,
            tool_name
        ),
    CONSTRAINT runner_lease_generation_positive_u64
        CHECK (generation BETWEEN 1 AND 18446744073709551615),
    CONSTRAINT runner_lease_effect_closed
        CHECK (effect_class IN ('pure', 'idempotent', 'side_effecting')),
    CONSTRAINT runner_lease_predecessor_shape
        CHECK (
            (generation = 1 AND predecessor_generation IS NULL)
            OR predecessor_generation = generation - 1
        ),
    CONSTRAINT runner_lease_credential_shape
        CHECK (
            (
                credential_profile_name IS NULL
                AND credential_grant_revision IS NULL
                AND credential_approval_kind IS NULL
            )
            OR (
                credential_profile_name IS NOT NULL
                AND credential_grant_revision IS NOT NULL
                AND credential_approval_kind
                    IN ('automatic', 'session_policy')
            )
        ),
    CONSTRAINT runner_lease_attempt_fk
        FOREIGN KEY (attempt_id, session_id)
        REFERENCES tool_attempt (attempt_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT runner_lease_registration_tool_fk
        FOREIGN KEY (
            registration_enrollment_id,
            registration_revision,
            tool_name
        )
        REFERENCES runner_registration_tool (
            enrollment_id,
            registration_revision,
            tool_name
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT runner_lease_placement_tool_fk
        FOREIGN KEY (
            session_id,
            placement_event_ordinal,
            tool_name
        )
        REFERENCES runner_session_placement_tool (
            session_id,
            event_ordinal,
            tool_name
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT runner_lease_grant_tool_fk
        FOREIGN KEY (
            session_id,
            runner_id,
            credential_grant_revision,
            credential_profile_name,
            tool_name,
            credential_approval_kind
        )
        REFERENCES runner_credential_grant_tool (
            session_id,
            runner_id,
            grant_revision,
            credential_profile_name,
            tool_name,
            approval_kind
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE TABLE runner_lease_event (
    lease_id uuid NOT NULL,
    generation numeric(20, 0) NOT NULL,
    event_ordinal numeric(20, 0) NOT NULL,
    state_kind text NOT NULL,

    CONSTRAINT runner_lease_event_pk
        PRIMARY KEY (lease_id, generation, event_ordinal),
    CONSTRAINT runner_lease_event_state_shape
        CHECK (
            (event_ordinal = 1 AND state_kind = 'offered')
            OR (event_ordinal = 2 AND state_kind IN ('claimed', 'lost_unclaimed'))
            OR (event_ordinal = 3 AND state_kind IN ('completed', 'lost_claimed'))
        ),
    CONSTRAINT runner_lease_event_generation_fk
        FOREIGN KEY (lease_id, generation)
        REFERENCES runner_lease_generation (lease_id, generation)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE runner_current_lease_event (
    lease_id uuid NOT NULL,
    generation numeric(20, 0) NOT NULL,
    event_ordinal numeric(20, 0) NOT NULL,

    CONSTRAINT runner_current_lease_event_pk
        PRIMARY KEY (lease_id, generation),
    CONSTRAINT runner_current_lease_event_fk
        FOREIGN KEY (lease_id, generation, event_ordinal)
        REFERENCES runner_lease_event (
            lease_id,
            generation,
            event_ordinal
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION guard_runner_current_lease_event()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    latest_ordinal numeric(20, 0);
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'runner lease event head is not deletable'
            USING ERRCODE = '23514';
    END IF;
    SELECT max(event_ordinal) INTO latest_ordinal
      FROM runner_lease_event
     WHERE lease_id = NEW.lease_id
       AND generation = NEW.generation;
    IF NEW.event_ordinal IS DISTINCT FROM latest_ordinal
       OR (
            TG_OP = 'INSERT'
            AND NEW.event_ordinal <> 1
            AND NOT EXISTS (
                SELECT 1
                  FROM runner_current_lease_event AS current_event
                 WHERE current_event.lease_id = NEW.lease_id
                   AND current_event.generation = NEW.generation
            )
       )
       OR (
            TG_OP = 'UPDATE'
            AND (
                NEW.lease_id <> OLD.lease_id
                OR NEW.generation <> OLD.generation
                OR NEW.event_ordinal <> OLD.event_ordinal + 1
            )
       )
    THEN
        RAISE EXCEPTION 'runner lease event head must advance to latest'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_current_lease_event_advances
BEFORE INSERT OR UPDATE OR DELETE ON runner_current_lease_event
FOR EACH ROW
EXECUTE FUNCTION guard_runner_current_lease_event();

CREATE FUNCTION assert_runner_lease_generation_complete(
    checked_lease uuid,
    checked_generation numeric
)
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM runner_lease_generation
         WHERE lease_id = checked_lease
           AND generation = checked_generation
    )
       AND (
            NOT EXISTS (
                SELECT 1
                  FROM runner_lease_event
                 WHERE lease_id = checked_lease
                   AND generation = checked_generation
                   AND event_ordinal = 1
                   AND state_kind = 'offered'
            )
            OR NOT EXISTS (
                SELECT 1
                  FROM runner_current_lease_event AS current_event
                  JOIN runner_lease_event AS event
                    ON event.lease_id = current_event.lease_id
                   AND event.generation = current_event.generation
                   AND event.event_ordinal =
                        current_event.event_ordinal
                 WHERE current_event.lease_id = checked_lease
                   AND current_event.generation = checked_generation
            )
       )
    THEN
        RAISE EXCEPTION 'runner lease generation lacks canonical event evidence'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION require_runner_lease_generation_complete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM assert_runner_lease_generation_complete(
        COALESCE(NEW.lease_id, OLD.lease_id),
        COALESCE(NEW.generation, OLD.generation)
    );
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_lease_generation_requires_events
AFTER INSERT OR UPDATE OR DELETE ON runner_lease_generation
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_lease_generation_complete();

CREATE CONSTRAINT TRIGGER runner_lease_event_rechecks_generation
AFTER INSERT OR UPDATE OR DELETE ON runner_lease_event
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_lease_generation_complete();

CREATE CONSTRAINT TRIGGER runner_current_lease_event_rechecks_generation
AFTER INSERT OR UPDATE OR DELETE ON runner_current_lease_event
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_lease_generation_complete();

CREATE VIEW runner_current_tool_attempt AS
SELECT attempt.*
  FROM tool_attempt AS attempt
 WHERE NOT EXISTS (
        SELECT 1
          FROM runner_lease_generation AS generation
          JOIN runner_current_lease_event AS current_event
            ON current_event.lease_id = generation.lease_id
           AND current_event.generation = generation.generation
          JOIN runner_lease_event AS event
            ON event.lease_id = current_event.lease_id
           AND event.generation = current_event.generation
           AND event.event_ordinal = current_event.event_ordinal
         WHERE generation.attempt_id = attempt.attempt_id
           AND generation.effect_class IN ('pure', 'idempotent')
           AND event.state_kind = 'lost_claimed'
 );

CREATE FUNCTION require_runner_initial_pin_has_lease()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.event_kind = 'pinned'
       AND NOT EXISTS (
            SELECT 1
              FROM runner_lease_generation AS lease
              JOIN runner_lease_event AS offered
                ON offered.lease_id = lease.lease_id
               AND offered.generation = lease.generation
               AND offered.event_ordinal = 1
               AND offered.state_kind = 'offered'
              JOIN runner_current_lease_event AS current_event
                ON current_event.lease_id = offered.lease_id
               AND current_event.generation = offered.generation
               AND current_event.event_ordinal = offered.event_ordinal
             WHERE lease.session_id = NEW.session_id
               AND lease.placement_event_ordinal = NEW.event_ordinal
               AND lease.generation = 1
       )
    THEN
        RAISE EXCEPTION 'initial runner pin lacks its atomic lease offer'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_initial_pin_requires_lease
AFTER INSERT ON runner_session_placement_record
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_initial_pin_has_lease();

CREATE TRIGGER runner_lease_generation_is_append_only
BEFORE UPDATE OR DELETE ON runner_lease_generation
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_physical_attempt_lease_binding_is_append_only
BEFORE UPDATE OR DELETE ON runner_physical_attempt_lease_binding
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_tool_request_lease_binding_is_append_only
BEFORE UPDATE OR DELETE ON runner_tool_request_lease_binding
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_lease_event_is_append_only
BEFORE UPDATE OR DELETE ON runner_lease_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION guard_runner_lease_generation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    placement runner_session_placement_record%ROWTYPE;
    enrollment_state text;
    attempted_tool text;
    attempted_effect text;
    attempted_state text;
    attempted_request uuid;
    current_registration_revision numeric;
    current_registration_runner uuid;
    registered_effect text;
    bound_lease uuid;
    bound_request_lease uuid;
    prior runner_lease_generation%ROWTYPE;
    prior_state text;
    prior_request uuid;
BEGIN
    SELECT record.* INTO placement
      FROM runner_current_session_placement AS current_placement
      JOIN runner_session_placement_record AS record
        ON record.session_id = current_placement.session_id
       AND record.event_ordinal = current_placement.event_ordinal
     WHERE current_placement.session_id = NEW.session_id;
    SELECT state_kind INTO enrollment_state
      FROM runner_enrollment
     WHERE enrollment_id = NEW.registration_enrollment_id;
    SELECT request.tool_name, attempt.effect_class, attempt.state_kind,
           attempt.request_id
      INTO attempted_tool, attempted_effect, attempted_state, attempted_request
      FROM tool_attempt AS attempt
      JOIN tool_request AS request
        ON request.request_id = attempt.request_id
     WHERE attempt.attempt_id = NEW.attempt_id
       AND attempt.session_id = NEW.session_id
       FOR UPDATE OF attempt;
    SELECT current_registration.registration_revision,
           registration.runner_id,
           registered.effect_class
      INTO current_registration_revision,
           current_registration_runner,
           registered_effect
      FROM runner_current_registration AS current_registration
      JOIN runner_registration AS registration
        ON registration.enrollment_id =
            current_registration.enrollment_id
       AND registration.registration_revision =
            current_registration.registration_revision
      JOIN runner_registration_tool AS registered
        ON registered.enrollment_id =
            current_registration.enrollment_id
       AND registered.registration_revision =
            current_registration.registration_revision
     WHERE current_registration.enrollment_id =
            NEW.registration_enrollment_id
       AND registered.tool_name = NEW.tool_name
       FOR SHARE OF current_registration;
    INSERT INTO runner_tool_request_lease_binding
        (request_id, lease_id)
    VALUES (attempted_request, NEW.lease_id)
    ON CONFLICT (request_id) DO NOTHING;
    SELECT lease_id INTO bound_request_lease
      FROM runner_tool_request_lease_binding
     WHERE request_id = attempted_request;
    INSERT INTO runner_physical_attempt_lease_binding
        (attempt_id, lease_id)
    VALUES (NEW.attempt_id, NEW.lease_id)
    ON CONFLICT (attempt_id) DO NOTHING;
    SELECT lease_id INTO bound_lease
      FROM runner_physical_attempt_lease_binding
     WHERE attempt_id = NEW.attempt_id;
    IF registered_effect IS NULL
       OR attempted_request IS NULL
       OR bound_request_lease IS DISTINCT FROM NEW.lease_id
       OR bound_lease IS DISTINCT FROM NEW.lease_id
       OR placement.state_kind IS DISTINCT FROM 'pinned'
       OR placement.event_ordinal IS DISTINCT FROM
            NEW.placement_event_ordinal
       OR placement.pinned_runner_id IS DISTINCT FROM NEW.runner_id
       OR placement.registration_enrollment_id IS DISTINCT FROM
            NEW.registration_enrollment_id
       OR placement.registration_revision IS DISTINCT FROM
            NEW.registration_revision
       OR placement.pinned_credential_profile_name IS DISTINCT FROM
            NEW.credential_profile_name
       OR placement.credential_grant_revision IS DISTINCT FROM
            NEW.credential_grant_revision
       OR current_registration_runner IS DISTINCT FROM NEW.runner_id
       OR (
            placement.selector_kind = 'identity'
            AND placement.selector_runner_id IS DISTINCT FROM
                current_registration_runner
       )
       OR (
            placement.selector_kind = 'capability_class'
            AND NOT EXISTS (
                SELECT 1
                  FROM runner_registration_class
                 WHERE enrollment_id =
                    NEW.registration_enrollment_id
                   AND registration_revision =
                    current_registration_revision
                   AND capability_class =
                    placement.selector_capability_class
            )
       )
       OR EXISTS (
            SELECT 1
              FROM runner_session_placement_tool AS required
             WHERE required.session_id = placement.session_id
               AND required.event_ordinal = placement.event_ordinal
               AND required.runner_required
               AND NOT EXISTS (
                    SELECT 1
                      FROM runner_registration_tool AS available
                     WHERE available.enrollment_id =
                        NEW.registration_enrollment_id
                       AND available.registration_revision =
                        current_registration_revision
                       AND available.tool_name = required.tool_name
               )
       )
       OR (
            placement.pinned_credential_profile_name IS NOT NULL
            AND NOT EXISTS (
                SELECT 1
                  FROM runner_registration_profile
                 WHERE enrollment_id =
                    NEW.registration_enrollment_id
                   AND registration_revision =
                    current_registration_revision
                   AND credential_profile_name =
                    placement.pinned_credential_profile_name
            )
       )
       OR (
            placement.workspace_requirement_kind =
                'repository_worktree'
            AND NOT EXISTS (
                SELECT 1
                  FROM runner_registration_workspace
                 WHERE enrollment_id =
                    NEW.registration_enrollment_id
                   AND registration_revision =
                    current_registration_revision
                   AND workspace_kind = 'worktree_per_session'
            )
       )
       OR enrollment_state IS DISTINCT FROM 'active'
       OR attempted_tool IS DISTINCT FROM NEW.tool_name
       OR attempted_state IS DISTINCT FROM 'in_flight'
       OR registered_effect IS DISTINCT FROM NEW.effect_class
       OR (
            NEW.effect_class = 'pure'
            AND attempted_effect <> 'effect_free'
       )
       OR (
            NEW.effect_class IN ('idempotent', 'side_effecting')
            AND attempted_effect <> 'external_effect'
       )
    THEN
        RAISE EXCEPTION 'runner lease offer is not canonically authorized'
            USING ERRCODE = '23514';
    END IF;
    IF EXISTS (
        SELECT 1
          FROM runner_lease_generation AS previous
          JOIN runner_current_lease_event AS current_event
            ON current_event.lease_id = previous.lease_id
           AND current_event.generation = previous.generation
          JOIN runner_lease_event AS event
            ON event.lease_id = current_event.lease_id
           AND event.generation = current_event.generation
           AND event.event_ordinal = current_event.event_ordinal
         WHERE previous.lease_id = NEW.lease_id
           AND previous.generation < NEW.generation
           AND previous.attempt_id = NEW.attempt_id
           AND event.state_kind IN ('lost_claimed', 'completed')
    ) THEN
        RAISE EXCEPTION 'claimed physical attempt cannot be reused'
            USING ERRCODE = '23514';
    END IF;
    IF EXISTS (
        SELECT 1
          FROM runner_lease_generation AS existing
         WHERE existing.attempt_id = NEW.attempt_id
           AND existing.lease_id <> NEW.lease_id
    ) THEN
        RAISE EXCEPTION 'physical attempt is already bound to another lease'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.credential_grant_revision IS NOT NULL
       AND EXISTS (
            SELECT 1
              FROM runner_credential_grant_audit AS audit
             WHERE audit.session_id = NEW.session_id
               AND audit.runner_id = NEW.runner_id
               AND audit.grant_revision =
                    NEW.credential_grant_revision
               AND audit.event_kind = 'revoked'
       )
    THEN
        RAISE EXCEPTION 'revoked credential grant cannot authorize a lease'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.generation > 1 THEN
        SELECT * INTO prior
          FROM runner_lease_generation
         WHERE lease_id = NEW.lease_id
           AND generation = NEW.predecessor_generation;
        SELECT event.state_kind INTO prior_state
          FROM runner_current_lease_event AS current_event
          JOIN runner_lease_event AS event
            ON event.lease_id = current_event.lease_id
           AND event.generation = current_event.generation
           AND event.event_ordinal = current_event.event_ordinal
         WHERE current_event.lease_id = NEW.lease_id
           AND current_event.generation = NEW.predecessor_generation;
        SELECT attempt.request_id INTO prior_request
          FROM tool_attempt AS attempt
         WHERE attempt.attempt_id = prior.attempt_id;
        IF NOT FOUND
           OR prior_state IS NULL
           OR prior_state NOT IN ('lost_unclaimed', 'lost_claimed')
           OR ROW(
                prior.session_id,
                prior.runner_id,
                prior.tool_name,
                prior.effect_class,
                prior.credential_profile_name,
                prior.credential_grant_revision,
                prior.credential_approval_kind
           ) IS DISTINCT FROM ROW(
                NEW.session_id,
                NEW.runner_id,
                NEW.tool_name,
                NEW.effect_class,
                NEW.credential_profile_name,
                NEW.credential_grant_revision,
                NEW.credential_approval_kind
           )
           OR (
                prior_state = 'lost_unclaimed'
                AND prior.attempt_id <> NEW.attempt_id
           )
           OR (
                prior_state = 'lost_claimed'
                AND (
                    prior.effect_class = 'side_effecting'
                    OR prior.attempt_id = NEW.attempt_id
                    OR prior_request IS DISTINCT FROM attempted_request
                )
           )
        THEN
            RAISE EXCEPTION 'runner lease retry violates durable effect law'
                USING ERRCODE = '23514';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_lease_generations_are_guarded
BEFORE INSERT ON runner_lease_generation
FOR EACH ROW
EXECUTE FUNCTION guard_runner_lease_generation();

CREATE FUNCTION guard_runner_lease_event()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    prior_state text;
BEGIN
    IF NEW.event_ordinal = 1 THEN
        RETURN NEW;
    END IF;
    SELECT state_kind INTO prior_state
      FROM runner_lease_event
     WHERE lease_id = NEW.lease_id
       AND generation = NEW.generation
       AND event_ordinal = NEW.event_ordinal - 1;
    IF NOT FOUND
       OR (
            NEW.event_ordinal = 2
            AND prior_state <> 'offered'
       )
       OR (
            NEW.event_ordinal = 3
            AND prior_state <> 'claimed'
       )
    THEN
        RAISE EXCEPTION 'runner lease event transition is not monotonic'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_lease_events_are_guarded
BEFORE INSERT ON runner_lease_event
FOR EACH ROW
EXECUTE FUNCTION guard_runner_lease_event();
