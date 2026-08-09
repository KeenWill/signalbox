-- Complete the append-only session placement loss lifecycle without changing
-- any previously applied migration.

ALTER TABLE runner_session_placement_record
    ADD COLUMN lost_runner_id uuid,
    ADD COLUMN loss_source_kind text;

-- Legacy loss rows did not retain enough evidence to distinguish connection
-- loss from registration reconciliation. Fail closed instead of guessing a
-- source that could later authorize or deny same-runner recovery.
DO $migration$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM runner_session_placement_record
         WHERE state_kind = 'runner_lost'
    ) THEN
        RAISE EXCEPTION
            'runner placement loss source cannot be inferred from legacy rows'
            USING ERRCODE = '23514';
    END IF;
END;
$migration$;

-- Supersedes the closed event vocabulary from
-- 202607280401_runner_protocol.sql.
ALTER TABLE runner_session_placement_record
    DROP CONSTRAINT runner_session_placement_event_closed,
    ADD CONSTRAINT runner_session_placement_event_closed
        CHECK (
            event_kind IN (
                'created',
                'pinned',
                'runner_lost_before_pin',
                'pre_pin_replaced',
                'runner_lost',
                'runner_replaced',
                'abandoned',
                'profile_replaced'
            )
        ),
    ADD CONSTRAINT runner_session_placement_loss_source_closed
        CHECK (
            loss_source_kind IS NULL
            OR loss_source_kind IN ('connection', 'registration')
        );

-- Supersedes the state-shape constraint from
-- 202607280402_runner_grant_tombstones.sql.
ALTER TABLE runner_session_placement_record
    DROP CONSTRAINT runner_session_placement_state_shape,
    ADD CONSTRAINT runner_session_placement_state_shape
        CHECK (
            (
                state_kind = 'unpinned'
                AND event_kind IN ('created', 'pre_pin_replaced')
                AND (
                    (event_kind = 'created' AND event_ordinal = 1
                        AND placement_revision = 1)
                    OR (event_kind = 'pre_pin_replaced' AND event_ordinal > 1
                        AND placement_revision > 1)
                )
                AND lost_runner_id IS NULL
                AND loss_source_kind IS NULL
                AND pinned_runner_id IS NULL
                AND pinned_working_directory IS NULL
                AND pinned_credential_profile_name IS NULL
                AND registration_enrollment_id IS NULL
                AND registration_revision IS NULL
                AND pinned_tool_count = 0
                AND workspace_repository_key IS NULL
                AND workspace_working_directory IS NULL
                AND credential_grant_runner_id IS NULL
                AND credential_grant_lineage_origin_ordinal IS NULL
                AND credential_grant_revision IS NULL
            )
            OR (
                state_kind = 'runner_lost_before_pin'
                AND event_kind = 'runner_lost_before_pin'
                AND lost_runner_id IS NOT NULL
                AND loss_source_kind IS NULL
                AND selector_kind = 'identity'
                AND selector_runner_id = lost_runner_id
                AND pinned_runner_id IS NULL
                AND pinned_working_directory IS NULL
                AND pinned_credential_profile_name IS NULL
                AND registration_enrollment_id IS NULL
                AND registration_revision IS NULL
                AND pinned_tool_count = 0
                AND workspace_repository_key IS NULL
                AND workspace_working_directory IS NULL
                AND credential_grant_runner_id IS NULL
                AND credential_grant_lineage_origin_ordinal IS NULL
                AND credential_grant_revision IS NULL
            )
            OR (
                state_kind = 'pinned'
                AND event_kind IN ('pinned', 'runner_replaced', 'profile_replaced')
                AND lost_runner_id IS NULL
                AND loss_source_kind IS NULL
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
                                AND credential_grant_lineage_origin_ordinal IS NULL
                                AND credential_grant_revision IS NULL
                            )
                            OR (
                                credential_grant_runner_id IS NOT NULL
                                AND credential_grant_lineage_origin_ordinal IS NOT NULL
                                AND credential_grant_revision IS NOT NULL
                            )
                        )
                    )
                    OR (
                        pinned_credential_profile_name IS NOT NULL
                        AND credential_grant_runner_id = pinned_runner_id
                        AND credential_grant_lineage_origin_ordinal IS NOT NULL
                        AND credential_grant_revision IS NOT NULL
                    )
                )
            )
            OR (
                state_kind = 'runner_lost'
                AND event_kind = 'runner_lost'
                AND lost_runner_id = pinned_runner_id
                AND loss_source_kind IS NOT NULL
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
                                AND credential_grant_lineage_origin_ordinal IS NULL
                                AND credential_grant_revision IS NULL
                            )
                            OR (
                                credential_grant_runner_id IS NOT NULL
                                AND credential_grant_lineage_origin_ordinal IS NOT NULL
                                AND credential_grant_revision IS NOT NULL
                            )
                        )
                    )
                    OR (
                        pinned_credential_profile_name IS NOT NULL
                        AND credential_grant_runner_id = pinned_runner_id
                        AND credential_grant_lineage_origin_ordinal IS NOT NULL
                        AND credential_grant_revision IS NOT NULL
                    )
                )
            )
            OR (
                state_kind = 'runner_abandoned'
                AND event_kind = 'abandoned'
                AND lost_runner_id IS NOT NULL
                AND (
                    (
                        loss_source_kind IS NULL
                        AND selector_kind = 'identity'
                        AND selector_runner_id = lost_runner_id
                        AND pinned_runner_id IS NULL
                        AND pinned_working_directory IS NULL
                        AND pinned_credential_profile_name IS NULL
                        AND registration_enrollment_id IS NULL
                        AND registration_revision IS NULL
                        AND pinned_tool_count = 0
                        AND workspace_repository_key IS NULL
                        AND workspace_working_directory IS NULL
                        AND credential_grant_runner_id IS NULL
                        AND credential_grant_lineage_origin_ordinal IS NULL
                        AND credential_grant_revision IS NULL
                    )
                    OR (
                        loss_source_kind IS NOT NULL
                        AND lost_runner_id = pinned_runner_id
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
                                        AND credential_grant_lineage_origin_ordinal IS NULL
                                        AND credential_grant_revision IS NULL
                                    )
                                    OR (
                                        credential_grant_runner_id IS NOT NULL
                                        AND credential_grant_lineage_origin_ordinal IS NOT NULL
                                        AND credential_grant_revision IS NOT NULL
                                    )
                                )
                            )
                            OR (
                                pinned_credential_profile_name IS NOT NULL
                                AND credential_grant_runner_id = pinned_runner_id
                                AND credential_grant_lineage_origin_ordinal IS NOT NULL
                                AND credential_grant_revision IS NOT NULL
                            )
                        )
                    )
                )
            )
        );

-- Supersedes the workspace-state shape from
-- 202608020003_runner_wire_contract.sql.
ALTER TABLE runner_session_placement_record
    DROP CONSTRAINT runner_session_placement_workspace_shape,
    ADD CONSTRAINT runner_session_placement_workspace_shape
        CHECK (
            (
                pinned_runner_id IS NULL
                AND workspace_repository_key IS NULL
                AND workspace_working_directory IS NULL
                AND workspace_manifest_id IS NULL
                AND workspace_placement_revision IS NULL
                AND workspace_clone_url_digest IS NULL
                AND workspace_credential_profile_name IS NULL
                AND workspace_sandbox_profile IS NULL
                AND workspace_relative_path IS NULL
                AND workspace_recovery_kind IS NULL
                AND workspace_branch_name IS NULL
                AND workspace_revision IS NULL
            )
            OR (
                pinned_runner_id IS NOT NULL
                AND workspace_requirement_kind = 'none'
                AND requested_repository_key IS NULL
                AND (
                    (
                        workspace_repository_key IS NULL
                        AND workspace_working_directory IS NULL
                        AND workspace_manifest_id IS NULL
                        AND workspace_placement_revision IS NULL
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
                        AND workspace_placement_revision IS NOT NULL
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
                pinned_runner_id IS NOT NULL
                AND workspace_requirement_kind = 'repository_worktree'
                AND requested_repository_key IS NOT NULL
                AND workspace_repository_key = requested_repository_key
                AND workspace_working_directory = pinned_working_directory
                AND workspace_manifest_id IS NOT NULL
                AND workspace_placement_revision IS NOT NULL
                AND workspace_clone_url_digest IS NOT NULL
                AND workspace_credential_profile_name IS NOT DISTINCT FROM
                    requested_credential_profile_name
                AND workspace_sandbox_profile = requested_sandbox_profile
                AND workspace_relative_path IS NOT NULL
                AND workspace_recovery_kind IN ('commit', 'branch')
                AND workspace_revision IS NOT NULL
                AND (
                    (workspace_recovery_kind = 'commit'
                        AND workspace_branch_name IS NULL)
                    OR (workspace_recovery_kind = 'branch'
                        AND workspace_branch_name IS NOT NULL)
                )
            )
        );

CREATE OR REPLACE FUNCTION guard_runner_placement_record()
RETURNS trigger
LANGUAGE plpgsql
AS $function$
DECLARE
    prior runner_session_placement_record%ROWTYPE;
    prior_grant_state text;
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
    SELECT * INTO prior
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
                NEW.selector_kind, NEW.selector_runner_id,
                NEW.selector_capability_class, NEW.directory_selection_kind,
                NEW.requested_working_directory,
                NEW.requested_credential_profile_name,
                NEW.workspace_requirement_kind, NEW.requested_repository_key,
                NEW.requested_sandbox_profile, NEW.permission_override_count
           ) IS DISTINCT FROM ROW(
                prior.selector_kind, prior.selector_runner_id,
                prior.selector_capability_class, prior.directory_selection_kind,
                prior.requested_working_directory,
                prior.requested_credential_profile_name,
                prior.workspace_requirement_kind, prior.requested_repository_key,
                prior.requested_sandbox_profile, prior.permission_override_count
           )
        THEN
            RAISE EXCEPTION 'runner placement pin is not canonical'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.event_kind = 'runner_lost_before_pin' THEN
        IF prior.state_kind <> 'unpinned'
           OR NEW.state_kind <> 'runner_lost_before_pin'
           OR NEW.placement_revision <> prior.placement_revision
           OR NEW.lost_runner_id IS DISTINCT FROM prior.selector_runner_id
           OR ROW(
                NEW.selector_kind, NEW.selector_runner_id,
                NEW.selector_capability_class, NEW.directory_selection_kind,
                NEW.requested_working_directory,
                NEW.requested_credential_profile_name,
                NEW.workspace_requirement_kind, NEW.requested_repository_key,
                NEW.requested_sandbox_profile, NEW.permission_override_count
           ) IS DISTINCT FROM ROW(
                prior.selector_kind, prior.selector_runner_id,
                prior.selector_capability_class, prior.directory_selection_kind,
                prior.requested_working_directory,
                prior.requested_credential_profile_name,
                prior.workspace_requirement_kind, prior.requested_repository_key,
                prior.requested_sandbox_profile, prior.permission_override_count
           )
        THEN
            RAISE EXCEPTION 'pre-pin runner loss changed placement intent'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.event_kind = 'pre_pin_replaced' THEN
        IF prior.state_kind <> 'runner_lost_before_pin'
           OR NEW.state_kind <> 'unpinned'
           OR NEW.placement_revision <> prior.placement_revision + 1
           OR NEW.selector_kind <> 'identity'
           OR NEW.selector_runner_id IS NULL
           OR NEW.selector_runner_id = prior.lost_runner_id
        THEN
            RAISE EXCEPTION 'pre-pin replacement is not a checked successor'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.event_kind = 'runner_lost' THEN
        IF prior.state_kind <> 'pinned'
           OR NEW.state_kind <> 'runner_lost'
           OR NEW.placement_revision <> prior.placement_revision
           OR NEW.lost_runner_id IS DISTINCT FROM prior.pinned_runner_id
           OR NEW.loss_source_kind IS NULL
           OR ROW(
                NEW.pinned_runner_id, NEW.pinned_working_directory,
                NEW.pinned_credential_profile_name,
                NEW.registration_enrollment_id, NEW.registration_revision,
                NEW.pinned_tool_count, NEW.workspace_repository_key,
                NEW.workspace_working_directory, NEW.workspace_manifest_id,
                NEW.workspace_placement_revision,
                NEW.workspace_clone_url_digest,
                NEW.workspace_credential_profile_name,
                NEW.workspace_sandbox_profile, NEW.workspace_relative_path,
                NEW.workspace_recovery_kind, NEW.workspace_branch_name,
                NEW.workspace_revision, NEW.credential_grant_runner_id,
                NEW.credential_grant_lineage_origin_ordinal,
                NEW.credential_grant_revision,
                NEW.selector_kind, NEW.selector_runner_id,
                NEW.selector_capability_class, NEW.directory_selection_kind,
                NEW.requested_working_directory,
                NEW.requested_credential_profile_name,
                NEW.workspace_requirement_kind, NEW.requested_repository_key,
                NEW.requested_sandbox_profile, NEW.permission_override_count
           ) IS DISTINCT FROM ROW(
                prior.pinned_runner_id, prior.pinned_working_directory,
                prior.pinned_credential_profile_name,
                prior.registration_enrollment_id, prior.registration_revision,
                prior.pinned_tool_count, prior.workspace_repository_key,
                prior.workspace_working_directory, prior.workspace_manifest_id,
                prior.workspace_placement_revision,
                prior.workspace_clone_url_digest,
                prior.workspace_credential_profile_name,
                prior.workspace_sandbox_profile, prior.workspace_relative_path,
                prior.workspace_recovery_kind, prior.workspace_branch_name,
                prior.workspace_revision, prior.credential_grant_runner_id,
                prior.credential_grant_lineage_origin_ordinal,
                prior.credential_grant_revision,
                prior.selector_kind, prior.selector_runner_id,
                prior.selector_capability_class, prior.directory_selection_kind,
                prior.requested_working_directory,
                prior.requested_credential_profile_name,
                prior.workspace_requirement_kind, prior.requested_repository_key,
                prior.requested_sandbox_profile, prior.permission_override_count
           )
        THEN
            RAISE EXCEPTION 'runner loss changed affinity facts'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.event_kind = 'runner_replaced' THEN
        IF prior.state_kind <> 'runner_lost'
           OR NEW.state_kind <> 'pinned'
           OR NEW.placement_revision <> prior.placement_revision + 1
           OR NEW.pinned_runner_id = prior.lost_runner_id
           OR (
                prior.credential_grant_revision IS NULL
                AND NEW.credential_grant_revision IS NOT NULL
                AND (
                    NEW.credential_grant_revision <> 1
                    OR NEW.credential_grant_lineage_origin_ordinal <>
                        NEW.event_ordinal
                )
           )
           OR (
                prior.credential_grant_revision IS NOT NULL
                AND (
                    NEW.credential_grant_revision IS DISTINCT FROM
                        prior.credential_grant_revision + 1
                    OR NEW.credential_grant_lineage_origin_ordinal IS DISTINCT FROM
                        prior.credential_grant_lineage_origin_ordinal
                )
           )
        THEN
            RAISE EXCEPTION 'runner replacement is not a checked successor'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.event_kind = 'abandoned' THEN
        IF NEW.state_kind <> 'runner_abandoned'
           OR NEW.placement_revision <> prior.placement_revision
           OR prior.state_kind NOT IN ('runner_lost_before_pin', 'runner_lost')
           OR ROW(
                NEW.lost_runner_id, NEW.loss_source_kind,
                NEW.pinned_runner_id, NEW.pinned_working_directory,
                NEW.pinned_credential_profile_name,
                NEW.registration_enrollment_id, NEW.registration_revision,
                NEW.pinned_tool_count, NEW.workspace_repository_key,
                NEW.workspace_working_directory, NEW.workspace_manifest_id,
                NEW.workspace_placement_revision,
                NEW.workspace_clone_url_digest,
                NEW.workspace_credential_profile_name,
                NEW.workspace_sandbox_profile, NEW.workspace_relative_path,
                NEW.workspace_recovery_kind, NEW.workspace_branch_name,
                NEW.workspace_revision, NEW.credential_grant_runner_id,
                NEW.credential_grant_lineage_origin_ordinal,
                NEW.credential_grant_revision,
                NEW.selector_kind, NEW.selector_runner_id,
                NEW.selector_capability_class, NEW.directory_selection_kind,
                NEW.requested_working_directory,
                NEW.requested_credential_profile_name,
                NEW.workspace_requirement_kind, NEW.requested_repository_key,
                NEW.requested_sandbox_profile, NEW.permission_override_count
           ) IS DISTINCT FROM ROW(
                prior.lost_runner_id, prior.loss_source_kind,
                prior.pinned_runner_id, prior.pinned_working_directory,
                prior.pinned_credential_profile_name,
                prior.registration_enrollment_id, prior.registration_revision,
                prior.pinned_tool_count, prior.workspace_repository_key,
                prior.workspace_working_directory, prior.workspace_manifest_id,
                prior.workspace_placement_revision,
                prior.workspace_clone_url_digest,
                prior.workspace_credential_profile_name,
                prior.workspace_sandbox_profile, prior.workspace_relative_path,
                prior.workspace_recovery_kind, prior.workspace_branch_name,
                prior.workspace_revision, prior.credential_grant_runner_id,
                prior.credential_grant_lineage_origin_ordinal,
                prior.credential_grant_revision,
                prior.selector_kind, prior.selector_runner_id,
                prior.selector_capability_class, prior.directory_selection_kind,
                prior.requested_working_directory,
                prior.requested_credential_profile_name,
                prior.workspace_requirement_kind, prior.requested_repository_key,
                prior.requested_sandbox_profile, prior.permission_override_count
           )
        THEN
            RAISE EXCEPTION 'runner abandonment changed retained facts'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.event_kind = 'profile_replaced' THEN
        SELECT event_kind INTO prior_grant_state
          FROM runner_current_credential_grant_audit
         WHERE session_id = prior.session_id
           AND lineage_origin_event_ordinal =
                prior.credential_grant_lineage_origin_ordinal
           AND runner_id = prior.credential_grant_runner_id
           AND grant_revision = prior.credential_grant_revision
         FOR SHARE;
        IF prior.state_kind <> 'pinned'
           OR NEW.state_kind <> 'pinned'
           OR NEW.placement_revision <> prior.placement_revision + 1
           OR NEW.pinned_runner_id <> prior.pinned_runner_id
           OR NEW.pinned_working_directory <> prior.pinned_working_directory
           OR NEW.registration_enrollment_id <> prior.registration_enrollment_id
           OR NEW.registration_revision <> prior.registration_revision
           OR NEW.credential_grant_lineage_origin_ordinal IS DISTINCT FROM
                prior.credential_grant_lineage_origin_ordinal
           OR NEW.credential_grant_revision IS DISTINCT FROM
                prior.credential_grant_revision + 1
           OR prior_grant_state IS NULL
           OR prior_grant_state NOT IN ('issued', 'replaced')
           OR NEW.workspace_repository_key IS DISTINCT FROM
                prior.workspace_repository_key
           OR NEW.workspace_working_directory IS DISTINCT FROM
                prior.workspace_working_directory
           OR ROW(
                NEW.selector_kind, NEW.selector_runner_id,
                NEW.selector_capability_class, NEW.directory_selection_kind,
                NEW.requested_working_directory,
                NEW.workspace_requirement_kind, NEW.requested_repository_key,
                NEW.requested_sandbox_profile, NEW.permission_override_count
           ) IS DISTINCT FROM ROW(
                prior.selector_kind, prior.selector_runner_id,
                prior.selector_capability_class, prior.directory_selection_kind,
                prior.requested_working_directory,
                prior.workspace_requirement_kind, prior.requested_repository_key,
                prior.requested_sandbox_profile, prior.permission_override_count
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
$function$;

CREATE OR REPLACE FUNCTION guard_runner_wire_placement_record()
RETURNS trigger
LANGUAGE plpgsql
AS $function$
DECLARE
    prior runner_session_placement_record%ROWTYPE;
BEGIN
    IF NEW.event_ordinal = 1
       OR NEW.event_kind IN ('runner_replaced', 'pre_pin_replaced')
    THEN
        RETURN NEW;
    END IF;
    SELECT * INTO prior
      FROM runner_session_placement_record
     WHERE session_id = NEW.session_id
       AND event_ordinal = NEW.event_ordinal - 1;
    IF NOT FOUND THEN
        RETURN NEW;
    END IF;
    IF NEW.event_kind IN (
        'pinned', 'runner_lost_before_pin', 'runner_lost',
        'abandoned', 'profile_replaced'
    )
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
    IF NEW.event_kind IN ('runner_lost', 'abandoned', 'profile_replaced')
       AND ROW(
            NEW.workspace_manifest_id, NEW.workspace_placement_revision,
            NEW.workspace_clone_url_digest,
            NEW.workspace_credential_profile_name,
            NEW.workspace_sandbox_profile, NEW.workspace_relative_path,
            NEW.workspace_recovery_kind, NEW.workspace_branch_name,
            NEW.workspace_revision
       ) IS DISTINCT FROM ROW(
            prior.workspace_manifest_id, prior.workspace_placement_revision,
            prior.workspace_clone_url_digest,
            prior.workspace_credential_profile_name,
            prior.workspace_sandbox_profile, prior.workspace_relative_path,
            prior.workspace_recovery_kind, prior.workspace_branch_name,
            prior.workspace_revision
       )
    THEN
        RAISE EXCEPTION 'runner placement changed workspace recovery facts'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$function$;

-- The canonical inventory checks apply only when the state retains a pin.
CREATE OR REPLACE FUNCTION assert_runner_placement_complete(
    checked_session uuid,
    checked_event numeric
)
RETURNS void
LANGUAGE plpgsql
AS $function$
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
        ON registered.enrollment_id = placement.registration_enrollment_id
       AND registered.registration_revision = placement.registration_revision
       AND registered.tool_name = pinned.tool_name
     WHERE pinned.session_id = checked_session
       AND pinned.event_ordinal = checked_event
       AND (
            registered.tool_name IS NULL
            OR pinned.runner_required IS DISTINCT FROM
                (registered.loci_kind = 'runner_only')
       );
    IF placement.pinned_tool_count <> actual_tools
       OR foreign_tools <> 0
       OR (
            placement.pinned_runner_id IS NOT NULL
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
                         WHERE enrollment_id = placement.registration_enrollment_id
                           AND registration_revision = placement.registration_revision
                           AND capability_class = placement.selector_capability_class
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
                             WHERE enrollment_id = placement.registration_enrollment_id
                               AND registration_revision = placement.registration_revision
                               AND credential_profile_name =
                                    placement.pinned_credential_profile_name
                        )
                        OR NOT EXISTS (
                            SELECT 1
                              FROM runner_credential_grant AS grant_record
                             WHERE grant_record.session_id = placement.session_id
                               AND grant_record.lineage_origin_event_ordinal =
                                    placement.credential_grant_lineage_origin_ordinal
                               AND grant_record.runner_id = placement.pinned_runner_id
                               AND grant_record.grant_revision =
                                    placement.credential_grant_revision
                               AND grant_record.credential_profile_name =
                                    placement.pinned_credential_profile_name
                               AND grant_record.registration_enrollment_id =
                                    placement.registration_enrollment_id
                        )
                    )
                )
                OR (
                    placement.workspace_requirement_kind = 'repository_worktree'
                    AND NOT EXISTS (
                        SELECT 1
                          FROM runner_registration_workspace
                         WHERE enrollment_id = placement.registration_enrollment_id
                           AND registration_revision = placement.registration_revision
                           AND workspace_kind = 'worktree_per_session'
                    )
                )
            )
       )
       OR (
            placement.pinned_runner_id IS NOT NULL
            AND actual_tools <> (
                SELECT count(*)
                  FROM runner_registration_tool
                 WHERE enrollment_id = placement.registration_enrollment_id
                   AND registration_revision = placement.registration_revision
            )
       )
       OR NOT EXISTS (
            SELECT 1
              FROM runner_current_session_placement AS current_placement
             WHERE current_placement.session_id = checked_session
               AND current_placement.event_ordinal = checked_event
               AND checked_event = (
                    SELECT max(latest.event_ordinal)
                      FROM runner_session_placement_record AS latest
                     WHERE latest.session_id = checked_session
               )
       )
    THEN
        RAISE EXCEPTION 'runner placement tool inventory is not canonical'
            USING ERRCODE = '23514';
    END IF;
END;
$function$;

CREATE OR REPLACE FUNCTION assert_runner_wire_placement_complete(
    checked_session uuid,
    checked_event numeric
)
RETURNS void
LANGUAGE plpgsql
AS $function$
DECLARE
    placement runner_session_placement_record%ROWTYPE;
    actual_overrides bigint;
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
    changed_overrides := 0;
    IF placement.event_ordinal > 1
       AND placement.event_kind IN (
            'pinned', 'runner_lost_before_pin', 'runner_lost',
            'abandoned', 'profile_replaced'
       )
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
       OR changed_overrides <> 0
    THEN
        RAISE EXCEPTION 'runner placement permission inventory is incomplete'
            USING ERRCODE = '23514';
    END IF;
END;
$function$;
