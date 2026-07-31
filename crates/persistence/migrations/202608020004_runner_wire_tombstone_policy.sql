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
    policy_event numeric;
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
    WITH RECURSIVE grant_line AS (
        SELECT current_grant.*
          FROM runner_credential_grant AS current_grant
         WHERE current_grant.session_id = grant_row.session_id
           AND current_grant.lineage_origin_event_ordinal =
                grant_row.lineage_origin_event_ordinal
           AND current_grant.runner_id = grant_row.runner_id
           AND current_grant.grant_revision = grant_row.grant_revision
        UNION ALL
        SELECT predecessor.*
          FROM grant_line AS successor
          JOIN runner_credential_grant AS predecessor
            ON predecessor.session_id = successor.session_id
           AND predecessor.lineage_origin_event_ordinal =
                successor.lineage_origin_event_ordinal
           AND predecessor.runner_id = successor.prior_runner_id
           AND predecessor.grant_revision = successor.prior_grant_revision
    )
    SELECT grant_line.placement_event_ordinal INTO policy_event
      FROM grant_line
      JOIN runner_session_placement_record AS placement
        ON placement.session_id = grant_line.session_id
       AND placement.event_ordinal = grant_line.placement_event_ordinal
     WHERE placement.pinned_credential_profile_name IS NOT NULL
     ORDER BY grant_line.grant_revision DESC
     LIMIT 1;
    IF policy_event IS NULL THEN
        RAISE EXCEPTION 'runner credential grant has no active policy origin'
            USING ERRCODE = '23514';
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
      LEFT JOIN runner_session_placement_record AS policy_placement
        ON policy_placement.session_id = grant_row.session_id
       AND policy_placement.event_ordinal = policy_event
      LEFT JOIN runner_session_placement_permission_override AS override_record
        ON override_record.session_id = policy_placement.session_id
       AND override_record.event_ordinal = policy_placement.event_ordinal
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
                    WHEN policy_placement.requested_sandbox_profile =
                        'workspace_restricted'
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
