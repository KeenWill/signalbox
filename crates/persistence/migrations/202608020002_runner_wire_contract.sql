-- Durable sandbox, repository, permission, and workspace-recovery facts.

ALTER TABLE runner_registration
    ADD COLUMN repository_count numeric(20, 0) NOT NULL DEFAULT 0,
    ADD COLUMN sandbox_count numeric(20, 0) NOT NULL DEFAULT 0,
    ADD CONSTRAINT runner_registration_wire_counts_bounded
        CHECK (
            repository_count BETWEEN 0 AND 64
            AND sandbox_count BETWEEN 0 AND 2
        );

ALTER TABLE runner_registration
    ALTER COLUMN repository_count DROP DEFAULT,
    ALTER COLUMN sandbox_count DROP DEFAULT;

CREATE TABLE runner_registration_sandbox (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20, 0) NOT NULL,
    sandbox_profile text NOT NULL,

    CONSTRAINT runner_registration_sandbox_pk
        PRIMARY KEY (enrollment_id, registration_revision, sandbox_profile),
    CONSTRAINT runner_registration_sandbox_closed
        CHECK (sandbox_profile IN ('ambient', 'workspace_restricted')),
    CONSTRAINT runner_registration_sandbox_registration_fk
        FOREIGN KEY (enrollment_id, registration_revision)
        REFERENCES runner_registration (enrollment_id, registration_revision)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE runner_registration_repository (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20, 0) NOT NULL,
    repository_key runner_catalog_name NOT NULL,
    credential_profile_name runner_catalog_name,

    CONSTRAINT runner_registration_repository_pk
        PRIMARY KEY (enrollment_id, registration_revision, repository_key),
    CONSTRAINT runner_registration_repository_registration_fk
        FOREIGN KEY (enrollment_id, registration_revision)
        REFERENCES runner_registration (enrollment_id, registration_revision)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT runner_registration_repository_profile_fk
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

CREATE TRIGGER runner_registration_sandbox_is_append_only
BEFORE UPDATE OR DELETE ON runner_registration_sandbox
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_registration_sandbox_rejects_truncate
BEFORE TRUNCATE ON runner_registration_sandbox
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_registration_repository_is_append_only
BEFORE UPDATE OR DELETE ON runner_registration_repository
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_registration_repository_rejects_truncate
BEFORE TRUNCATE ON runner_registration_repository
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION assert_runner_wire_registration_complete(
    checked_enrollment uuid,
    checked_revision numeric
)
RETURNS void
LANGUAGE plpgsql
AS $function$
DECLARE
    declared_repositories numeric;
    declared_sandboxes numeric;
    actual_repositories bigint;
    actual_sandboxes bigint;
BEGIN
    SELECT repository_count, sandbox_count
      INTO declared_repositories, declared_sandboxes
      FROM runner_registration
     WHERE enrollment_id = checked_enrollment
       AND registration_revision = checked_revision;
    IF NOT FOUND THEN
        RETURN;
    END IF;
    SELECT count(*) INTO actual_repositories
      FROM runner_registration_repository
     WHERE enrollment_id = checked_enrollment
       AND registration_revision = checked_revision;
    SELECT count(*) INTO actual_sandboxes
      FROM runner_registration_sandbox
     WHERE enrollment_id = checked_enrollment
       AND registration_revision = checked_revision;
    IF ROW(declared_repositories, declared_sandboxes)
        IS DISTINCT FROM ROW(actual_repositories, actual_sandboxes)
    THEN
        RAISE EXCEPTION 'runner wire registration inventory is incomplete'
            USING ERRCODE = '23514';
    END IF;
END;
$function$;

CREATE FUNCTION require_runner_wire_registration_complete()
RETURNS trigger
LANGUAGE plpgsql
AS $function$
BEGIN
    PERFORM assert_runner_wire_registration_complete(
        COALESCE(NEW.enrollment_id, OLD.enrollment_id),
        COALESCE(NEW.registration_revision, OLD.registration_revision)
    );
    RETURN NULL;
END;
$function$;

CREATE CONSTRAINT TRIGGER runner_registration_requires_wire_inventory
AFTER INSERT OR UPDATE OR DELETE ON runner_registration
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_wire_registration_complete();

CREATE CONSTRAINT TRIGGER runner_registration_sandbox_rechecks_inventory
AFTER INSERT OR UPDATE OR DELETE ON runner_registration_sandbox
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_wire_registration_complete();

CREATE CONSTRAINT TRIGGER runner_registration_repository_rechecks_inventory
AFTER INSERT OR UPDATE OR DELETE ON runner_registration_repository
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_wire_registration_complete();

ALTER TABLE runner_session_placement_record
    ADD COLUMN requested_sandbox_profile text NOT NULL
        DEFAULT 'workspace_restricted',
    ADD COLUMN permission_override_count numeric(20, 0) NOT NULL DEFAULT 0,
    ADD COLUMN workspace_manifest_id uuid,
    ADD COLUMN workspace_clone_url_digest text,
    ADD COLUMN workspace_credential_profile_name runner_catalog_name,
    ADD COLUMN workspace_sandbox_profile text,
    ADD COLUMN workspace_relative_path runner_exact_text,
    ADD COLUMN workspace_recovery_kind text,
    ADD COLUMN workspace_branch_name text,
    ADD COLUMN workspace_revision text;

ALTER TABLE runner_session_placement_record
    ALTER COLUMN requested_sandbox_profile DROP DEFAULT,
    ALTER COLUMN permission_override_count DROP DEFAULT,
    DROP CONSTRAINT runner_session_placement_workspace_shape,
    ADD CONSTRAINT runner_session_placement_wire_u64
        CHECK (
            permission_override_count BETWEEN 0 AND 64
        ),
    ADD CONSTRAINT runner_session_placement_sandbox_closed
        CHECK (
            requested_sandbox_profile IN ('ambient', 'workspace_restricted')
            AND (
                workspace_sandbox_profile IS NULL
                OR workspace_sandbox_profile IN (
                    'ambient',
                    'workspace_restricted'
                )
            )
        ),
    ADD CONSTRAINT runner_session_placement_repository_key_shape
        CHECK (
            (
                requested_repository_key IS NULL
                OR (
                    octet_length(requested_repository_key) BETWEEN 1 AND 64
                    AND requested_repository_key ~ '^[A-Za-z0-9][A-Za-z0-9._-]*$'
                )
            )
            AND (
                workspace_repository_key IS NULL
                OR (
                    octet_length(workspace_repository_key) BETWEEN 1 AND 64
                    AND workspace_repository_key ~ '^[A-Za-z0-9][A-Za-z0-9._-]*$'
                )
            )
        ),
    ADD CONSTRAINT runner_session_placement_relative_path_shape
        CHECK (
            workspace_relative_path IS NULL
            OR workspace_relative_path !~ '(^/|//|(^|/)\.{1,2}(/|$))'
        ),
    ADD CONSTRAINT runner_session_placement_clone_url_digest_hex
        CHECK (
            workspace_clone_url_digest IS NULL
            OR workspace_clone_url_digest ~ '^[0-9a-f]{64}$'
        ),
    ADD CONSTRAINT runner_session_placement_workspace_revision_hex
        CHECK (
            workspace_revision IS NULL
            OR workspace_revision ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'
        ),
    ADD CONSTRAINT runner_session_placement_workspace_branch_shape
        CHECK (
            workspace_branch_name IS NULL
            OR (
                octet_length(workspace_branch_name) BETWEEN 1 AND 255
                AND workspace_branch_name !~ '[[:cntrl:] ~^:?*\\[\\]\\\\]'
                AND workspace_branch_name !~ '(^-|^/|/$|//|\\.\\.|@\\{|\\.$)'
                AND workspace_branch_name !~ '(^|/)\\.'
                AND workspace_branch_name !~ '\\.lock(?:/|$)'
                AND workspace_branch_name <> '@'
            )
        ),
    ADD CONSTRAINT runner_session_placement_workspace_shape
        CHECK (
            (
                state_kind = 'unpinned'
                AND workspace_repository_key IS NULL
                AND workspace_working_directory IS NULL
                AND workspace_manifest_id IS NULL
                AND workspace_clone_url_digest IS NULL
                AND workspace_credential_profile_name IS NULL
                AND workspace_sandbox_profile IS NULL
                AND workspace_relative_path IS NULL
                AND workspace_recovery_kind IS NULL
                AND workspace_branch_name IS NULL
                AND workspace_revision IS NULL
            )
            OR (
                state_kind IN ('pinned', 'runner_lost')
                AND workspace_requirement_kind = 'none'
                AND requested_repository_key IS NULL
                AND (
                    (
                        workspace_repository_key IS NULL
                        AND workspace_working_directory IS NULL
                        AND workspace_manifest_id IS NULL
                        AND workspace_clone_url_digest IS NULL
                        AND workspace_credential_profile_name IS NULL
                        AND workspace_sandbox_profile IS NULL
                        AND workspace_relative_path IS NULL
                        AND workspace_recovery_kind IS NULL
                        AND workspace_branch_name IS NULL
                        AND workspace_revision IS NULL
                        AND (
                            requested_sandbox_profile = 'ambient'
                            OR directory_selection_kind = 'exact'
                        )
                    )
                    OR (
                        requested_sandbox_profile = 'workspace_restricted'
                        AND directory_selection_kind = 'runner_default'
                        AND workspace_repository_key IS NULL
                        AND workspace_working_directory = pinned_working_directory
                        AND workspace_manifest_id IS NOT NULL
                        AND workspace_clone_url_digest IS NULL
                        AND workspace_credential_profile_name IS NULL
                        AND workspace_sandbox_profile = requested_sandbox_profile
                        AND workspace_relative_path IS NOT NULL
                        AND workspace_recovery_kind IS NULL
                        AND workspace_branch_name IS NULL
                        AND workspace_revision IS NULL
                    )
                )
            )
            OR (
                state_kind IN ('pinned', 'runner_lost')
                AND workspace_requirement_kind = 'repository_worktree'
                AND requested_repository_key IS NOT NULL
                AND workspace_repository_key = requested_repository_key
                AND workspace_working_directory = pinned_working_directory
                AND workspace_manifest_id IS NOT NULL
                AND workspace_clone_url_digest IS NOT NULL
                AND workspace_credential_profile_name IS NOT DISTINCT FROM
                    requested_credential_profile_name
                AND workspace_sandbox_profile = requested_sandbox_profile
                AND workspace_relative_path IS NOT NULL
                AND workspace_recovery_kind IN ('commit', 'branch')
                AND workspace_revision IS NOT NULL
                AND (
                    (
                        workspace_recovery_kind = 'commit'
                        AND workspace_branch_name IS NULL
                    )
                    OR (
                        workspace_recovery_kind = 'branch'
                        AND workspace_branch_name IS NOT NULL
                    )
                )
            )
        );

CREATE TABLE runner_session_placement_permission_override (
    session_id uuid NOT NULL,
    event_ordinal numeric(20, 0) NOT NULL,
    tool_name text NOT NULL,
    permission_kind text NOT NULL,

    CONSTRAINT runner_session_placement_permission_override_pk
        PRIMARY KEY (session_id, event_ordinal, tool_name),
    CONSTRAINT runner_session_placement_permission_override_tool_shape
        CHECK (
            octet_length(tool_name) BETWEEN 1 AND 64
            AND tool_name ~ '^[A-Za-z0-9_-]+$'
        ),
    CONSTRAINT runner_session_placement_permission_override_closed
        CHECK (permission_kind IN ('auto', 'confirm')),
    CONSTRAINT runner_session_placement_permission_override_record_fk
        FOREIGN KEY (session_id, event_ordinal)
        REFERENCES runner_session_placement_record (session_id, event_ordinal)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER runner_session_placement_permission_override_is_append_only
BEFORE UPDATE OR DELETE ON runner_session_placement_permission_override
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_session_placement_permission_override_rejects_truncate
BEFORE TRUNCATE ON runner_session_placement_permission_override
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION guard_runner_wire_placement_record()
RETURNS trigger
LANGUAGE plpgsql
AS $function$
DECLARE
    prior runner_session_placement_record%ROWTYPE;
BEGIN
    IF NEW.event_ordinal = 1 OR NEW.event_kind = 'runner_replaced' THEN
        RETURN NEW;
    END IF;
    SELECT * INTO prior
      FROM runner_session_placement_record
     WHERE session_id = NEW.session_id
       AND event_ordinal = NEW.event_ordinal - 1;
    IF NOT FOUND THEN
        RETURN NEW;
    END IF;
    IF NEW.event_kind IN ('pinned', 'runner_lost', 'profile_replaced')
       AND ROW(
            NEW.requested_sandbox_profile,
            NEW.permission_override_count
       ) IS DISTINCT FROM ROW(
            prior.requested_sandbox_profile,
            prior.permission_override_count
       )
    THEN
        RAISE EXCEPTION 'runner placement changed sandbox or permission overrides'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.event_kind IN ('runner_lost', 'profile_replaced')
       AND ROW(
            NEW.workspace_manifest_id,
            NEW.workspace_clone_url_digest,
            NEW.workspace_credential_profile_name,
            NEW.workspace_sandbox_profile,
            NEW.workspace_relative_path,
            NEW.workspace_recovery_kind,
            NEW.workspace_branch_name,
            NEW.workspace_revision
       ) IS DISTINCT FROM ROW(
            prior.workspace_manifest_id,
            prior.workspace_clone_url_digest,
            prior.workspace_credential_profile_name,
            prior.workspace_sandbox_profile,
            prior.workspace_relative_path,
            prior.workspace_recovery_kind,
            prior.workspace_branch_name,
            prior.workspace_revision
       )
    THEN
        RAISE EXCEPTION 'runner placement changed workspace recovery facts'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$function$;

CREATE TRIGGER runner_wire_placement_records_are_guarded
BEFORE INSERT ON runner_session_placement_record
FOR EACH ROW
EXECUTE FUNCTION guard_runner_wire_placement_record();

CREATE FUNCTION assert_runner_wire_placement_complete(
    checked_session uuid,
    checked_event numeric
)
RETURNS void
LANGUAGE plpgsql
AS $function$
DECLARE
    placement runner_session_placement_record%ROWTYPE;
    actual_overrides bigint;
    invalid_overrides bigint;
    changed_overrides bigint;
BEGIN
    SELECT * INTO placement
      FROM runner_session_placement_record
     WHERE session_id = checked_session
       AND event_ordinal = checked_event;
    IF NOT FOUND THEN
        RETURN;
    END IF;
    SELECT count(*) INTO actual_overrides
      FROM runner_session_placement_permission_override
     WHERE session_id = checked_session
       AND event_ordinal = checked_event;
    SELECT count(*) INTO invalid_overrides
      FROM runner_session_placement_permission_override AS override_record
     WHERE override_record.session_id = checked_session
       AND override_record.event_ordinal = checked_event
       AND placement.state_kind <> 'unpinned'
       AND NOT EXISTS (
            SELECT 1
              FROM runner_registration_tool AS available
             WHERE available.enrollment_id = placement.registration_enrollment_id
               AND available.registration_revision = placement.registration_revision
               AND available.tool_name = override_record.tool_name
       );
    changed_overrides := 0;
    IF placement.event_ordinal > 1
       AND placement.event_kind IN ('pinned', 'runner_lost', 'profile_replaced')
    THEN
        SELECT count(*) INTO changed_overrides
          FROM (
                (
                    SELECT tool_name, permission_kind
                      FROM runner_session_placement_permission_override
                     WHERE session_id = checked_session
                       AND event_ordinal = checked_event
                    EXCEPT
                    SELECT tool_name, permission_kind
                      FROM runner_session_placement_permission_override
                     WHERE session_id = checked_session
                       AND event_ordinal = checked_event - 1
                )
                UNION ALL
                (
                    SELECT tool_name, permission_kind
                      FROM runner_session_placement_permission_override
                     WHERE session_id = checked_session
                       AND event_ordinal = checked_event - 1
                    EXCEPT
                    SELECT tool_name, permission_kind
                      FROM runner_session_placement_permission_override
                     WHERE session_id = checked_session
                       AND event_ordinal = checked_event
                )
          ) AS changed;
    END IF;
    IF placement.permission_override_count <> actual_overrides
       OR invalid_overrides <> 0
       OR changed_overrides <> 0
    THEN
        RAISE EXCEPTION 'runner placement permission inventory is incomplete'
            USING ERRCODE = '23514';
    END IF;
END;
$function$;

CREATE FUNCTION require_runner_wire_placement_complete()
RETURNS trigger
LANGUAGE plpgsql
AS $function$
BEGIN
    PERFORM assert_runner_wire_placement_complete(
        COALESCE(NEW.session_id, OLD.session_id),
        COALESCE(NEW.event_ordinal, OLD.event_ordinal)
    );
    RETURN NULL;
END;
$function$;

CREATE CONSTRAINT TRIGGER runner_session_placement_requires_permission_overrides
AFTER INSERT OR UPDATE OR DELETE ON runner_session_placement_record
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_wire_placement_complete();

CREATE CONSTRAINT TRIGGER runner_session_placement_permission_rechecks_inventory
AFTER INSERT OR UPDATE OR DELETE ON runner_session_placement_permission_override
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_wire_placement_complete();

-- Approval is placement policy: exact override, then sandbox default.
CREATE OR REPLACE FUNCTION assert_runner_grant_complete(
    checked_session uuid,
    checked_origin numeric,
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
       AND lineage_origin_event_ordinal = checked_origin
       AND runner_id = checked_runner
       AND grant_revision = checked_revision;
    IF NOT FOUND THEN
        RETURN;
    END IF;
    SELECT count(*) INTO actual_tools
      FROM runner_credential_grant_tool
     WHERE session_id = checked_session
       AND lineage_origin_event_ordinal = checked_origin
       AND runner_id = checked_runner
       AND grant_revision = checked_revision;
    SELECT count(*) INTO invalid_tools
      FROM runner_credential_grant_tool AS granted
      LEFT JOIN runner_registration_tool AS available
        ON available.enrollment_id = grant_row.registration_enrollment_id
       AND available.registration_revision = grant_row.registration_revision
       AND available.tool_name = granted.tool_name
      LEFT JOIN runner_session_placement_record AS placement
        ON placement.session_id = grant_row.session_id
       AND placement.event_ordinal = grant_row.placement_event_ordinal
      LEFT JOIN runner_session_placement_record AS prior_placement
        ON prior_placement.session_id = grant_row.session_id
       AND prior_placement.event_ordinal = grant_row.placement_event_ordinal - 1
       AND placement.pinned_credential_profile_name IS NULL
       AND grant_row.credential_profile_name IS NOT NULL
      LEFT JOIN runner_session_placement_permission_override AS override_record
        ON override_record.session_id = placement.session_id
       AND override_record.event_ordinal = CASE
            WHEN prior_placement.event_ordinal IS NOT NULL
                THEN prior_placement.event_ordinal
            ELSE placement.event_ordinal
       END
       AND override_record.tool_name = granted.tool_name
     WHERE granted.session_id = checked_session
       AND granted.lineage_origin_event_ordinal = checked_origin
       AND granted.runner_id = checked_runner
       AND granted.grant_revision = checked_revision
       AND (
            available.tool_name IS NULL
            OR granted.approval_kind <>
                CASE
                    WHEN override_record.permission_kind = 'auto'
                        THEN 'automatic'
                    WHEN override_record.permission_kind = 'confirm'
                        THEN 'session_policy'
                    WHEN COALESCE(
                        prior_placement.requested_sandbox_profile,
                        placement.requested_sandbox_profile
                    ) = 'workspace_restricted'
                        THEN 'automatic'
                    WHEN available.effect_class = 'pure'
                        THEN 'automatic'
                    ELSE 'session_policy'
                END
       );
    SELECT count(*) INTO initial_audit
      FROM runner_credential_grant_audit
     WHERE session_id = checked_session
       AND lineage_origin_event_ordinal = checked_origin
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
                   AND prior_placement.credential_grant_lineage_origin_ordinal =
                        grant_row.lineage_origin_event_ordinal
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
               AND placement.event_kind IN ('pinned', 'runner_replaced')
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
                               AND granted.lineage_origin_event_ordinal =
                                    grant_row.lineage_origin_event_ordinal
                               AND granted.runner_id = grant_row.runner_id
                               AND granted.grant_revision = grant_row.grant_revision
                               AND granted.tool_name = available.tool_name
                       )
               )
       )
       OR NOT EXISTS (
            SELECT 1
              FROM runner_session_placement_record AS placement
             WHERE placement.session_id = grant_row.session_id
               AND placement.event_ordinal = grant_row.placement_event_ordinal
               AND placement.state_kind = 'pinned'
               AND placement.credential_grant_runner_id = grant_row.runner_id
               AND placement.credential_grant_lineage_origin_ordinal =
                    grant_row.lineage_origin_event_ordinal
               AND placement.credential_grant_revision = grant_row.grant_revision
               AND (
                    (
                        placement.pinned_runner_id = grant_row.runner_id
                        AND placement.registration_enrollment_id =
                            grant_row.registration_enrollment_id
                        AND placement.pinned_credential_profile_name =
                            grant_row.credential_profile_name
                    )
                    OR (
                        placement.pinned_credential_profile_name IS NULL
                        AND EXISTS (
                            SELECT 1
                              FROM runner_credential_grant_audit AS revoked
                             WHERE revoked.session_id = grant_row.session_id
                               AND revoked.lineage_origin_event_ordinal =
                                    grant_row.lineage_origin_event_ordinal
                               AND revoked.runner_id = grant_row.runner_id
                               AND revoked.grant_revision = grant_row.grant_revision
                               AND revoked.event_kind = 'revoked'
                        )
                    )
               )
       )
    THEN
        RAISE EXCEPTION 'runner credential grant evidence is incomplete'
            USING ERRCODE = '23514';
    END IF;
END;
$function$;

-- Placement policy, not profile policy or declaration permission, authorizes runner work.
CREATE FUNCTION guard_runner_wire_lease_approval()
RETURNS trigger
LANGUAGE plpgsql
AS $function$
DECLARE
    effective_approval text;
    decision_source text;
BEGIN
    SELECT
        CASE
            WHEN override_record.permission_kind = 'auto'
                THEN 'automatic'
            WHEN override_record.permission_kind = 'confirm'
                THEN 'session_policy'
            WHEN placement.requested_sandbox_profile = 'workspace_restricted'
                THEN 'automatic'
            WHEN registered.effect_class = 'pure'
                THEN 'automatic'
            ELSE 'session_policy'
        END,
        approval.decision_source
      INTO effective_approval, decision_source
      FROM runner_session_placement_record AS placement
      JOIN runner_registration_tool AS registered
        ON registered.enrollment_id = placement.registration_enrollment_id
       AND registered.registration_revision = placement.registration_revision
       AND registered.tool_name = NEW.tool_name
      JOIN tool_attempt AS attempt
        ON attempt.attempt_id = NEW.attempt_id
      LEFT JOIN runner_session_placement_permission_override AS override_record
        ON override_record.session_id = placement.session_id
       AND override_record.event_ordinal = placement.event_ordinal
       AND override_record.tool_name = NEW.tool_name
      LEFT JOIN tool_approval_decision AS approval
        ON approval.request_id = attempt.request_id
       AND approval.decision_kind = 'approve'
     WHERE placement.session_id = NEW.session_id
       AND placement.event_ordinal = NEW.placement_event_ordinal;
    IF NOT FOUND
       OR decision_source = 'session_blanket'
       OR (
            effective_approval = 'session_policy'
            AND decision_source IS DISTINCT FROM 'owner_command'
       )
       OR (
            NEW.credential_profile_name IS NOT NULL
            AND NEW.credential_approval_kind IS DISTINCT FROM effective_approval
       )
    THEN
        RAISE EXCEPTION 'runner lease approval is not placement-authorized'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$function$;

CREATE TRIGGER runner_lease_generation_wire_approval_is_guarded
BEFORE INSERT ON runner_lease_generation
FOR EACH ROW
EXECUTE FUNCTION guard_runner_wire_lease_approval();
