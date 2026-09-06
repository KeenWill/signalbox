-- Runners: enrollment, registration and its satellite catalogs, connection
-- authority and loss propagation, leases and their generations, credential
-- grants and audits, session placement, tool-request and attempt lease
-- bindings, and the runner-scoped outbox events.

-- Function bodies may read tables a later section or file creates;
-- 202609010000_core.sql explains why validation is deferred.
SET check_function_bodies = false;

--
-- Domains.
--

--
-- Name: runner_catalog_name; Type: DOMAIN; Schema: public
--

CREATE DOMAIN runner_catalog_name AS text
	CONSTRAINT runner_catalog_name_check CHECK (((octet_length(VALUE) BETWEEN 1 AND 64) AND (VALUE ~ '^[A-Za-z0-9][A-Za-z0-9._-]*$'::text)));


--
-- Name: runner_exact_text; Type: DOMAIN; Schema: public
--

CREATE DOMAIN runner_exact_text AS text
	CONSTRAINT runner_exact_text_check CHECK (((octet_length(VALUE) >= 1) AND (octet_length(VALUE) <= 4096)));


--
-- Name: runner_tool_schema; Type: DOMAIN; Schema: public
--

CREATE DOMAIN runner_tool_schema AS text
	CONSTRAINT runner_tool_schema_check CHECK (((octet_length(VALUE) >= 1) AND (octet_length(VALUE) <= 1048576)));


--
-- Functions.
--

--
-- Name: advance_runner_current_grant_audit(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION advance_runner_current_grant_audit() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.audit_ordinal = 1 THEN
        INSERT INTO runner_current_credential_grant_audit
            (session_id, lineage_origin_event_ordinal, runner_id,
             grant_revision, audit_ordinal, event_kind)
        VALUES (
            NEW.session_id,
            NEW.lineage_origin_event_ordinal,
            NEW.runner_id,
            NEW.grant_revision,
            NEW.audit_ordinal,
            NEW.event_kind
        );
    ELSE
        UPDATE runner_current_credential_grant_audit
           SET audit_ordinal = NEW.audit_ordinal,
               event_kind = NEW.event_kind
         WHERE session_id = NEW.session_id
           AND lineage_origin_event_ordinal =
                NEW.lineage_origin_event_ordinal
           AND runner_id = NEW.runner_id
           AND grant_revision = NEW.grant_revision
           AND audit_ordinal = NEW.audit_ordinal - 1;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'runner grant audit does not advance its current head'
                USING ERRCODE = '23514';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: assert_runner_connection_authority_head_complete(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_runner_connection_authority_head_complete(checked_enrollment uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    expected_connection_epoch numeric;
    expected_connection_event_ordinal numeric;
    expected_loss_epoch numeric;
    current_loss_epoch numeric;
    head runner_connection_authority_head%ROWTYPE;
BEGIN
    SELECT connection_epoch, event_ordinal
      INTO expected_connection_epoch, expected_connection_event_ordinal
      FROM runner_connection_event
     WHERE enrollment_id = checked_enrollment
     ORDER BY connection_epoch DESC, event_ordinal DESC
     LIMIT 1;
    SELECT max(loss_epoch)
      INTO expected_loss_epoch
      FROM runner_connection_loss_epoch
     WHERE enrollment_id = checked_enrollment;
    SELECT loss_epoch
      INTO current_loss_epoch
      FROM runner_current_connection_loss
     WHERE enrollment_id = checked_enrollment;
    SELECT *
      INTO head
      FROM runner_connection_authority_head
     WHERE enrollment_id = checked_enrollment;
    IF expected_loss_epoch IS DISTINCT FROM current_loss_epoch
       OR (
            expected_connection_epoch IS NOT NULL
            AND ROW(
                head.connection_epoch,
                head.connection_event_ordinal,
                head.latest_loss_epoch
            ) IS DISTINCT FROM ROW(
                expected_connection_epoch,
                expected_connection_event_ordinal,
                expected_loss_epoch
            )
       )
    THEN
        RAISE EXCEPTION 'runner connection authority head is not complete'
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: assert_runner_connection_loss_complete(uuid, numeric); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_runner_connection_loss_complete(checked_enrollment uuid, checked_connection_epoch numeric) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    connection_state text;
    loss_count bigint;
BEGIN
    SELECT state_kind
      INTO connection_state
      FROM runner_connection_event
     WHERE enrollment_id = checked_enrollment
       AND connection_epoch = checked_connection_epoch
     ORDER BY event_ordinal DESC
     LIMIT 1;
    SELECT count(*)
      INTO loss_count
      FROM runner_connection_loss_epoch
     WHERE enrollment_id = checked_enrollment
       AND connection_epoch = checked_connection_epoch;
    IF (connection_state = 'lost' AND loss_count <> 1)
       OR (connection_state IS DISTINCT FROM 'lost' AND loss_count <> 0)
    THEN
        RAISE EXCEPTION 'terminal runner connection lacks its exact loss epoch'
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: assert_runner_connection_loss_has_propagation(uuid, numeric); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_runner_connection_loss_has_propagation(checked_enrollment uuid, checked_loss_epoch numeric) RETURNS void
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM runner_connection_loss_epoch AS loss
          JOIN runner_connection_loss_propagation AS propagation
            ON propagation.enrollment_id = loss.enrollment_id
           AND propagation.loss_epoch = loss.loss_epoch
         WHERE loss.enrollment_id = checked_enrollment
           AND loss.loss_epoch = checked_loss_epoch
    )
    THEN
        RAISE EXCEPTION 'runner connection loss lacks propagation cursor'
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: assert_runner_grant_complete(uuid, numeric, uuid, numeric); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_runner_grant_complete(checked_session uuid, checked_origin numeric, checked_runner uuid, checked_revision numeric) RETURNS void
    LANGUAGE plpgsql
    AS $$
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
$$;


--
-- Name: assert_runner_lease_generation_complete(uuid, numeric); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_runner_lease_generation_complete(checked_lease uuid, checked_generation numeric) RETURNS void
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
                   AND current_event.event_ordinal = (
                        SELECT max(latest.event_ordinal)
                          FROM runner_lease_event AS latest
                         WHERE latest.lease_id = checked_lease
                           AND latest.generation =
                                checked_generation
                   )
            )
       )
    THEN
        RAISE EXCEPTION 'runner lease generation lacks canonical event evidence'
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: assert_runner_no_execution_proof_complete(uuid, numeric); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_runner_no_execution_proof_complete(checked_lease uuid, checked_generation numeric) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    current_state text;
    proof_exists boolean;
BEGIN
    SELECT event.state_kind INTO current_state
      FROM runner_current_lease_event AS current_event
      JOIN runner_lease_event AS event
        ON event.lease_id = current_event.lease_id
       AND event.generation = current_event.generation
       AND event.event_ordinal = current_event.event_ordinal
     WHERE current_event.lease_id = checked_lease
       AND current_event.generation = checked_generation;
    SELECT EXISTS (
        SELECT 1
          FROM runner_lease_no_execution_proof
         WHERE lease_id = checked_lease
           AND generation = checked_generation
    ) INTO proof_exists;
    IF proof_exists IS DISTINCT FROM (current_state = 'lost_unclaimed') THEN
        RAISE EXCEPTION 'runner lost-unclaimed lease lacks exact no-execution proof'
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: assert_runner_placement_complete(uuid, numeric); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_runner_placement_complete(checked_session uuid, checked_event numeric) RETURNS void
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
$$;


--
-- Name: assert_runner_placement_interrupted_attempt_complete(uuid, numeric); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_runner_placement_interrupted_attempt_complete(checked_session_id uuid, checked_event_ordinal numeric) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    placement runner_session_placement_record%ROWTYPE;
BEGIN
    SELECT * INTO placement
      FROM runner_session_placement_record
     WHERE session_id = checked_session_id
       AND event_ordinal = checked_event_ordinal;
    IF NOT FOUND OR placement.interrupted_tool_attempt_id IS NULL THEN
        RETURN;
    END IF;
    IF placement.event_kind <> 'runner_lost'
       OR NOT EXISTS (
            SELECT 1
              FROM tool_attempt AS attempt
              JOIN runner_current_tool_attempt AS current_attempt
                ON current_attempt.attempt_id = attempt.attempt_id
              JOIN tool_request AS request
                ON request.request_id = attempt.request_id
               AND request.turn_id = attempt.turn_id
               AND request.session_id = attempt.session_id
              JOIN runner_physical_attempt_lease_binding AS binding
                ON binding.attempt_id = attempt.attempt_id
              JOIN runner_lease_generation AS lease
                ON lease.lease_id = binding.lease_id
               AND lease.attempt_id = attempt.attempt_id
               AND lease.session_id = attempt.session_id
              JOIN runner_current_lease_event AS current_lease
                ON current_lease.lease_id = lease.lease_id
               AND current_lease.generation = lease.generation
              JOIN runner_lease_event AS lease_event
                ON lease_event.lease_id = current_lease.lease_id
               AND lease_event.generation = current_lease.generation
               AND lease_event.event_ordinal = current_lease.event_ordinal
              JOIN runner_session_placement_record AS leased_placement
                ON leased_placement.session_id = lease.session_id
               AND leased_placement.event_ordinal =
                    lease.placement_event_ordinal
             WHERE attempt.attempt_id =
                    placement.interrupted_tool_attempt_id
               AND attempt.session_id = placement.session_id
               AND (
                    (
                        attempt.state_kind = 'in_flight'
                        AND (
                            lease_event.state_kind = 'lost_unclaimed'
                            OR (
                                lease_event.state_kind IN (
                                    'lost_execution_possible',
                                    'lost_claimed'
                                )
                                AND lease.effect_class IN (
                                    'pure', 'idempotent'
                                )
                            )
                        )
                    )
                    OR (
                        attempt.state_kind = 'terminal'
                        AND attempt.terminal_disposition_kind = 'ambiguous'
                        AND lease_event.state_kind IN (
                            'lost_execution_possible',
                            'lost_claimed'
                        )
                        AND (
                            lease.effect_class = 'side_effecting'
                            OR (
                                lease.effect_class = 'idempotent'
                                AND EXISTS (
                                    SELECT 1
                                      FROM turn_runner_recovery_interrupt_effect
                                        AS stopped_effect
                                      JOIN turn_lifecycle AS stopped_turn
                                        ON stopped_turn.session_id =
                                            stopped_effect.session_id
                                       AND stopped_turn.turn_id =
                                            stopped_effect.turn_id
                                     WHERE stopped_effect.session_id =
                                            placement.session_id
                                       AND stopped_effect.interrupted_tool_attempt_id =
                                            attempt.attempt_id
                                       AND stopped_turn.state_kind = 'terminal'
                                       AND stopped_turn.terminal_disposition_kind =
                                            'reconciliation_required'
                                       AND stopped_turn.terminal_tool_attempt_id =
                                            attempt.attempt_id
                                )
                            )
                        )
                    )
                    OR (
                        attempt.state_kind = 'terminal'
                        AND attempt.terminal_disposition_kind = 'known_failed'
                        AND attempt.error_kind = 'crash_lost'
                        AND attempt.error_detail IS NULL
                        AND (
                            lease_event.state_kind = 'lost_unclaimed'
                            OR (
                                lease_event.state_kind IN (
                                    'lost_execution_possible',
                                    'lost_claimed'
                                )
                                AND lease.effect_class = 'pure'
                            )
                        )
                    )
               )
               AND lease.runner_id = placement.lost_runner_id
               AND leased_placement.event_ordinal < placement.event_ordinal
               AND runner_lease_placement_reaches_loss_revision(
                    lease.session_id,
                    lease.placement_event_ordinal,
                    placement.placement_revision,
                    placement.lost_runner_id
               )
               AND leased_placement.state_kind = 'pinned'
               AND leased_placement.pinned_runner_id =
                    placement.lost_runner_id
               AND EXISTS (
                    SELECT 1
                      FROM turn_lifecycle AS lifecycle
                     WHERE lifecycle.turn_id = attempt.turn_id
                       AND lifecycle.session_id = attempt.session_id
                       AND lifecycle.state_kind = 'active'
                       AND lifecycle.active_phase_kind =
                            'awaiting_runner_recovery'
                       AND lifecycle.current_attempt_id IS NULL
                       AND lifecycle.active_tool_round_call_id =
                            request.producing_model_call_id
                       AND lifecycle.runner_recovery_runner_id =
                            placement.lost_runner_id
                       AND lifecycle.runner_recovery_placement_revision =
                            placement.placement_revision
                       AND lifecycle.runner_recovery_tool_attempt_id =
                            attempt.attempt_id
               )
       )
    THEN
        RAISE EXCEPTION
            'runner placement loss lacks exact interrupted lease lineage'
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: assert_runner_registration_complete(uuid, numeric); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_runner_registration_complete(checked_enrollment uuid, checked_revision numeric) RETURNS void
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
       OR NOT EXISTS (
            SELECT 1
              FROM runner_current_registration AS current_registration
             WHERE current_registration.enrollment_id =
                    checked_enrollment
               AND current_registration.registration_revision =
                    checked_revision
               AND checked_revision = (
                    SELECT max(latest.registration_revision)
                      FROM runner_registration AS latest
                     WHERE latest.enrollment_id = checked_enrollment
               )
       )
    THEN
        RAISE EXCEPTION 'runner registration inventory is incomplete'
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: assert_runner_wire_placement_complete(uuid, numeric); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_runner_wire_placement_complete(checked_session uuid, checked_event numeric) RETURNS void
    LANGUAGE plpgsql
    AS $$
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
$$;


--
-- Name: assert_runner_wire_registration_complete(uuid, numeric); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_runner_wire_registration_complete(checked_enrollment uuid, checked_revision numeric) RETURNS void
    LANGUAGE plpgsql
    AS $$
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
$$;


--
-- Name: guard_runner_claimed_retry_attempt_authority(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_runner_claimed_retry_attempt_authority() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    source_attempt uuid;
    source_session uuid;
    source_effect text;
    source_state text;
    source_request uuid;
    source_turn uuid;
    source_issuing_attempt uuid;
    source_dispatch_generation numeric;
BEGIN
    SELECT generation.attempt_id, generation.session_id,
           generation.effect_class, event.state_kind,
           attempt.request_id, attempt.turn_id,
           attempt.issuing_turn_attempt_id,
           attempt.dispatch_generation
      INTO source_attempt, source_session, source_effect, source_state,
           source_request, source_turn, source_issuing_attempt,
           source_dispatch_generation
      FROM runner_lease_generation AS generation
      JOIN runner_current_lease_event AS current_event
        ON current_event.lease_id = generation.lease_id
       AND current_event.generation = generation.generation
      JOIN runner_lease_event AS event
        ON event.lease_id = current_event.lease_id
       AND event.generation = current_event.generation
       AND event.event_ordinal = current_event.event_ordinal
      JOIN tool_attempt AS attempt
        ON attempt.attempt_id = generation.attempt_id
     WHERE generation.lease_id = NEW.source_lease_id
       AND generation.generation = NEW.source_generation
     FOR UPDATE OF current_event;
    IF NOT FOUND
       OR source_state NOT IN ('lost_execution_possible', 'lost_claimed')
       OR source_effect = 'side_effecting'
       OR NEW.replacement_attempt_id = source_attempt
       OR ROW(
            NEW.replacement_session_id,
            NEW.replacement_turn_id,
            NEW.replacement_issuing_turn_attempt_id,
            NEW.replacement_request_id,
            NEW.replacement_dispatch_generation
       ) IS DISTINCT FROM ROW(
            source_session,
            source_turn,
            source_issuing_attempt,
            source_request,
            source_dispatch_generation
       )
    THEN
        RAISE EXCEPTION 'claimed retry attempt lacks durable loss authority'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: guard_runner_connection_authority_head(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_runner_connection_authority_head() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'DELETE'
       OR (
            TG_OP = 'UPDATE'
            AND (
                OLD.enrollment_id <> NEW.enrollment_id
                OR NEW.connection_epoch < OLD.connection_epoch
                OR (
                    NEW.connection_epoch = OLD.connection_epoch
                    AND NEW.connection_event_ordinal <=
                        OLD.connection_event_ordinal
                )
                OR (
                    NEW.connection_epoch > OLD.connection_epoch
                    AND NEW.connection_event_ordinal <> 1
                )
                OR (
                    NEW.latest_loss_epoch IS DISTINCT FROM
                        OLD.latest_loss_epoch
                    AND NEW.latest_loss_epoch IS NULL
                )
            )
       )
    THEN
        RAISE EXCEPTION 'runner connection authority head must advance monotonically'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: guard_runner_connection_event_insert(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_runner_connection_event_insert() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    prior runner_connection_event%ROWTYPE;
BEGIN
    PERFORM 1
      FROM runner_enrollment
     WHERE enrollment_id = NEW.enrollment_id
       AND state_kind = 'active'
       FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner connection requires active enrollment'
            USING ERRCODE = '23514';
    END IF;

    SELECT *
      INTO prior
      FROM runner_connection_event
     WHERE enrollment_id = NEW.enrollment_id
     ORDER BY connection_epoch DESC, event_ordinal DESC
     LIMIT 1;

    IF NOT FOUND THEN
        IF NEW.connection_epoch <> 1
            OR NEW.event_ordinal <> 1
            OR NEW.state_kind <> 'connected'
            OR NEW.cause_kind <> 'established'
        THEN
            RAISE EXCEPTION 'invalid initial runner connection event'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF NEW.connection_epoch = prior.connection_epoch + 1 THEN
        IF NEW.event_ordinal <> 1
            OR NEW.state_kind <> 'connected'
            OR NEW.cause_kind <> 'established'
        THEN
            RAISE EXCEPTION 'invalid successor runner connection event'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF NEW.connection_epoch <> prior.connection_epoch
        OR NEW.event_ordinal <> prior.event_ordinal + 1
        OR prior.state_kind IN ('shutdown', 'lost')
        OR (prior.state_kind = 'connected' AND NEW.state_kind NOT IN ('suspect', 'shutdown', 'lost'))
        OR (prior.state_kind = 'suspect' AND NEW.state_kind NOT IN ('connected', 'shutdown', 'lost'))
    THEN
        RAISE EXCEPTION 'invalid runner connection transition'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: guard_runner_connection_loss_epoch(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_runner_connection_loss_epoch() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    prior runner_connection_loss_epoch%ROWTYPE;
    source_state text;
BEGIN
    SELECT state_kind
      INTO source_state
      FROM runner_connection_event
     WHERE enrollment_id = NEW.enrollment_id
       AND connection_epoch = NEW.connection_epoch
       AND event_ordinal = NEW.connection_event_ordinal;
    SELECT *
      INTO prior
      FROM runner_connection_loss_epoch
     WHERE enrollment_id = NEW.enrollment_id
     ORDER BY loss_epoch DESC
     LIMIT 1;
    IF source_state IS DISTINCT FROM 'lost'
       OR (
            NOT FOUND
            AND NEW.loss_epoch <> 1
       )
       OR (
            prior.enrollment_id IS NOT NULL
            AND (
                NEW.loss_epoch <> prior.loss_epoch + 1
                OR NEW.connection_epoch <= prior.connection_epoch
            )
       )
    THEN
        RAISE EXCEPTION 'runner loss epoch lacks its next terminal connection'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: guard_runner_connection_loss_propagation(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_runner_connection_loss_propagation() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    lost_runner uuid;
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'runner connection loss propagation is durable'
            USING ERRCODE = '23514';
    END IF;
    IF TG_OP = 'INSERT' THEN
        IF NEW.state_kind <> 'pending'
           OR NEW.propagated_through_session_id IS NOT NULL
        THEN
            RAISE EXCEPTION 'new runner loss propagation must start pending'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;
    IF NEW.enrollment_id IS DISTINCT FROM OLD.enrollment_id
       OR NEW.loss_epoch IS DISTINCT FROM OLD.loss_epoch
       OR OLD.state_kind = 'completed'
       OR (
            NEW.state_kind = 'pending'
            AND (
                NEW.propagated_through_session_id IS NULL
                OR (
                    OLD.propagated_through_session_id IS NOT NULL
                    AND NEW.propagated_through_session_id <=
                        OLD.propagated_through_session_id
                )
            )
       )
       OR (
            NEW.state_kind = 'completed'
            AND NEW.propagated_through_session_id IS DISTINCT FROM
                OLD.propagated_through_session_id
       )
    THEN
        RAISE EXCEPTION 'runner connection loss propagation must advance monotonically'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.state_kind = 'completed' THEN
        SELECT runner_id
          INTO STRICT lost_runner
          FROM runner_enrollment
         WHERE enrollment_id = NEW.enrollment_id;
        PERFORM lock_runner_loss_identity(lost_runner);
    END IF;
    IF NEW.state_kind = 'pending'
       AND EXISTS (
            SELECT 1
              FROM runner_current_session_placement AS current_placement
              JOIN runner_session_placement_record AS placement
                ON placement.session_id = current_placement.session_id
               AND placement.event_ordinal = current_placement.event_ordinal
              JOIN runner_enrollment AS lost_enrollment
                ON lost_enrollment.enrollment_id = NEW.enrollment_id
             WHERE (
                    placement.loss_fence_enrollment_id = NEW.enrollment_id
                    OR (
                        placement.loss_fence_enrollment_id IS NULL
                        AND placement.state_kind = 'unpinned'
                        AND placement.selector_kind = 'identity'
                        AND placement.selector_runner_id =
                            lost_enrollment.runner_id
                    )
               )
               AND (
                    placement.observed_runner_loss_epoch IS NULL
                    OR placement.observed_runner_loss_epoch < NEW.loss_epoch
               )
               AND (
                    placement.state_kind = 'pinned'
                    OR (
                        placement.state_kind = 'unpinned'
                        AND placement.selector_kind = 'identity'
                    )
               )
               AND placement.session_id <=
                    NEW.propagated_through_session_id
       )
    THEN
        RAISE EXCEPTION 'runner connection loss cursor skipped an affected session'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.state_kind = 'completed'
       AND EXISTS (
            SELECT 1
              FROM runner_current_session_placement AS current_placement
              JOIN runner_session_placement_record AS placement
                ON placement.session_id = current_placement.session_id
               AND placement.event_ordinal = current_placement.event_ordinal
              JOIN runner_enrollment AS lost_enrollment
                ON lost_enrollment.enrollment_id = NEW.enrollment_id
             WHERE (
                    placement.loss_fence_enrollment_id = NEW.enrollment_id
                    OR (
                        placement.loss_fence_enrollment_id IS NULL
                        AND placement.state_kind = 'unpinned'
                        AND placement.selector_kind = 'identity'
                        AND placement.selector_runner_id =
                            lost_enrollment.runner_id
                    )
               )
               AND (
                    placement.observed_runner_loss_epoch IS NULL
                    OR placement.observed_runner_loss_epoch < NEW.loss_epoch
               )
               AND (
                    placement.state_kind = 'pinned'
                    OR (
                        placement.state_kind = 'unpinned'
                        AND placement.selector_kind = 'identity'
                    )
               )
       )
    THEN
        RAISE EXCEPTION 'runner connection loss cursor completed before propagation'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: guard_runner_current_connection_loss(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_runner_current_connection_loss() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    latest_epoch numeric;
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'runner loss head cannot be deleted'
            USING ERRCODE = '23514';
    END IF;
    SELECT max(loss_epoch)
      INTO latest_epoch
      FROM runner_connection_loss_epoch
     WHERE enrollment_id = NEW.enrollment_id;
    IF NEW.loss_epoch IS DISTINCT FROM latest_epoch
       OR (
            TG_OP = 'INSERT'
            AND NEW.loss_epoch <> 1
       )
       OR (
            TG_OP = 'UPDATE'
            AND (
                OLD.enrollment_id <> NEW.enrollment_id
                OR NEW.loss_epoch <> OLD.loss_epoch + 1
            )
       )
    THEN
        RAISE EXCEPTION 'runner loss head must advance to the latest epoch'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: guard_runner_current_grant_audit(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_runner_current_grant_audit() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'DELETE'
       OR (
            TG_OP = 'INSERT'
            AND (
                NEW.audit_ordinal <> 1
                OR NEW.event_kind NOT IN ('issued', 'replaced')
            )
       )
       OR (
            TG_OP = 'UPDATE'
            AND (
                ROW(
                    NEW.session_id,
                    NEW.lineage_origin_event_ordinal,
                    NEW.runner_id,
                    NEW.grant_revision
                )
                    IS DISTINCT FROM
                    ROW(
                    OLD.session_id,
                    OLD.lineage_origin_event_ordinal,
                    OLD.runner_id,
                    OLD.grant_revision
                )
                OR OLD.audit_ordinal <> 1
                OR OLD.event_kind NOT IN ('issued', 'replaced')
                OR NEW.audit_ordinal <> 2
                OR NEW.event_kind <> 'revoked'
            )
       )
    THEN
        RAISE EXCEPTION 'runner grant audit head is not a canonical advance'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: guard_runner_current_lease_event(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_runner_current_lease_event() RETURNS trigger
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


--
-- Name: guard_runner_current_placement(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_runner_current_placement() RETURNS trigger
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


--
-- Name: guard_runner_current_registration(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_runner_current_registration() RETURNS trigger
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


--
-- Name: guard_runner_enrollment_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_runner_enrollment_change() RETURNS trigger
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


--
-- Name: guard_runner_lease_event(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_runner_lease_event() RETURNS trigger
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


--
-- Name: guard_runner_lease_generation(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_runner_lease_generation() RETURNS trigger
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
    registered_permission text;
    bound_lease uuid;
    bound_request_lease uuid;
    prior runner_lease_generation%ROWTYPE;
    prior_state text;
    prior_request uuid;
    grant_state text;
BEGIN
    SELECT record.* INTO placement
      FROM runner_current_session_placement AS current_placement
      JOIN runner_session_placement_record AS record
        ON record.session_id = current_placement.session_id
       AND record.event_ordinal = current_placement.event_ordinal
     WHERE current_placement.session_id = NEW.session_id
       FOR SHARE OF current_placement;
    SELECT state_kind INTO enrollment_state
      FROM runner_enrollment
     WHERE enrollment_id = NEW.registration_enrollment_id
       FOR SHARE;
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
           registered.effect_class,
           registered.permission_kind
      INTO current_registration_revision,
           current_registration_runner,
           registered_effect,
           registered_permission
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
    IF NEW.credential_grant_revision IS NOT NULL THEN
        SELECT event_kind INTO grant_state
          FROM runner_current_credential_grant_audit
         WHERE session_id = NEW.session_id
           AND lineage_origin_event_ordinal =
                NEW.credential_grant_lineage_origin_ordinal
           AND runner_id = NEW.runner_id
           AND grant_revision = NEW.credential_grant_revision
         FOR SHARE;
    END IF;
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
       OR (
            NEW.credential_profile_name IS NOT NULL
            AND (
                placement.credential_grant_lineage_origin_ordinal IS DISTINCT FROM
                    NEW.credential_grant_lineage_origin_ordinal
                OR placement.credential_grant_revision IS DISTINCT FROM
                    NEW.credential_grant_revision
            )
       )
       OR (
            NEW.credential_profile_name IS NULL
            AND NEW.credential_grant_revision IS NOT NULL
       )
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
    -- A session-policy tool/profile pair requires confirmation: only a
    -- user-command decision, a consumed one-shot user override, or the frozen
    -- session blanket may approve the request this lease dispatches. The
    -- override is the user confirming that exact command in advance, so it
    -- confirms the pair exactly as a user command does. Policy-auto provenance
    -- would bypass the confirmation the pair posture records.
    IF NEW.credential_approval_kind = 'session_policy'
       AND NOT EXISTS (
            SELECT 1
              FROM tool_approval_decision AS approval
             WHERE approval.request_id = attempted_request
               AND approval.decision_kind = 'approve'
               AND approval.decision_source
                    IN ('user_command', 'session_blanket', 'user_override')
       )
    THEN
        RAISE EXCEPTION
            'session-policy lease admission requires confirmed approval provenance'
            USING ERRCODE = '23514';
    END IF;
    -- A profileless Confirm declaration accepts only a user-command
    -- decision, a consumed one-shot user override, or the frozen session
    -- blanket. The override is the user confirming that exact command in
    -- advance. Policy-auto provenance would bypass the confirmation the
    -- daemon-authoritative declaration records.
    IF NEW.credential_profile_name IS NULL
       AND registered_permission = 'confirm'
       AND NOT EXISTS (
            SELECT 1
              FROM tool_approval_decision AS approval
             WHERE approval.request_id = attempted_request
               AND approval.decision_kind = 'approve'
               AND approval.decision_source
                    IN ('user_command', 'session_blanket', 'user_override')
       )
    THEN
        RAISE EXCEPTION
            'profileless confirm lease admission requires confirmed approval provenance'
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
           AND event.state_kind IN ('lost_execution_possible', 'lost_claimed', 'completed')
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
       AND grant_state NOT IN ('issued', 'replaced')
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
           OR prior_state NOT IN ('lost_unclaimed', 'lost_execution_possible', 'lost_claimed')
           OR ROW(
                prior.session_id,
                prior.runner_id,
                prior.tool_name,
                prior.effect_class,
                prior.credential_profile_name,
                prior.credential_grant_lineage_origin_ordinal,
                prior.credential_grant_revision,
                prior.credential_approval_kind
           ) IS DISTINCT FROM ROW(
                NEW.session_id,
                NEW.runner_id,
                NEW.tool_name,
                NEW.effect_class,
                NEW.credential_profile_name,
                NEW.credential_grant_lineage_origin_ordinal,
                NEW.credential_grant_revision,
                NEW.credential_approval_kind
           )
           OR (
                prior_state = 'lost_unclaimed'
                AND prior.attempt_id <> NEW.attempt_id
           )
           OR (
                prior_state IN ('lost_execution_possible', 'lost_claimed')
                AND (
                    prior.effect_class = 'side_effecting'
                    OR prior.attempt_id = NEW.attempt_id
                    OR prior_request IS DISTINCT FROM attempted_request
                    OR NOT EXISTS (
                        SELECT 1
                          FROM runner_claimed_retry_attempt_authority AS authority
                         WHERE authority.source_lease_id = prior.lease_id
                           AND authority.source_generation = prior.generation
                           AND authority.replacement_attempt_id = NEW.attempt_id
                    )
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


--
-- Name: guard_runner_placement_record(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_runner_placement_record() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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
           OR (
                NEW.pinned_runner_id = prior.lost_runner_id
                AND prior.loss_source_kind <> 'registration'
           )
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
$$;


--
-- Name: guard_runner_registration_insert(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_runner_registration_insert() RETURNS trigger
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


--
-- Name: guard_runner_state_transition_outbox_event(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_runner_state_transition_outbox_event() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    placement runner_session_placement_record%ROWTYPE;
    prior runner_session_placement_record%ROWTYPE;
    connection runner_connection_event%ROWTYPE;
    predecessor runner_connection_event%ROWTYPE;
    expected_runner uuid;
BEGIN
    SELECT *
      INTO STRICT placement
      FROM runner_session_placement_record
     WHERE session_id = NEW.session_id
       AND event_ordinal = NEW.placement_event_ordinal
       AND placement_revision = NEW.placement_revision;

    IF NEW.sandbox_profile <> placement.requested_sandbox_profile
       OR NEW.working_directory IS DISTINCT FROM
            placement.requested_working_directory
    THEN
        RAISE EXCEPTION 'runner outbox placement projection does not match source'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.state_kind IN ('suspect', 'connected') THEN
        PERFORM 1
          FROM runner_current_session_placement AS current_placement
         WHERE current_placement.session_id = placement.session_id
           AND current_placement.event_ordinal = placement.event_ordinal;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'runner connection outbox source is not the current placement'
                USING ERRCODE = '23514';
        END IF;

        SELECT *
          INTO STRICT connection
          FROM runner_connection_event
         WHERE enrollment_id = NEW.connection_enrollment_id
           AND connection_epoch = NEW.connection_epoch
           AND event_ordinal = NEW.connection_event_ordinal;
        PERFORM 1
          FROM runner_connection_event AS later
         WHERE later.enrollment_id = connection.enrollment_id
           AND (
                later.connection_epoch > connection.connection_epoch
                OR (
                    later.connection_epoch = connection.connection_epoch
                    AND later.event_ordinal > connection.event_ordinal
                )
           );
        IF FOUND THEN
            RAISE EXCEPTION 'runner connection outbox source is not the latest event'
                USING ERRCODE = '23514';
        END IF;
        IF NEW.state_kind = 'connected'
           AND connection.cause_kind = 'established'
        THEN
            SELECT *
              INTO predecessor
              FROM runner_connection_event AS earlier
             WHERE earlier.enrollment_id = connection.enrollment_id
               AND (
                    earlier.connection_epoch < connection.connection_epoch
                    OR (
                        earlier.connection_epoch = connection.connection_epoch
                        AND earlier.event_ordinal < connection.event_ordinal
                    )
               )
             ORDER BY earlier.connection_epoch DESC, earlier.event_ordinal DESC
             LIMIT 1;
            IF NOT FOUND
               OR connection.event_ordinal <> 1
               OR predecessor.connection_epoch + 1 <>
                    connection.connection_epoch
               OR predecessor.state_kind <> 'suspect'
            THEN
                RAISE EXCEPTION 'established runner recovery lacks suspect predecessor'
                    USING ERRCODE = '23514';
            END IF;
        END IF;
        IF placement.state_kind <> 'pinned'
           OR placement.pinned_runner_id <> NEW.runner_id
           OR placement.registration_enrollment_id <>
                NEW.connection_enrollment_id
           OR (NEW.state_kind = 'suspect'
                AND ROW(connection.state_kind, connection.cause_kind) <>
                    ROW('suspect', 'heartbeat_missed'))
           OR (NEW.state_kind = 'connected'
                AND connection.state_kind <> 'connected')
           OR (NEW.state_kind = 'connected'
                AND connection.cause_kind NOT IN (
                    'established',
                    'heartbeat_recovered'
                ))
        THEN
            RAISE EXCEPTION 'runner connection outbox source does not match placement'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF NEW.state_kind = 'pinned' THEN
        expected_runner := placement.pinned_runner_id;
        IF placement.event_kind <> 'pinned'
           OR placement.state_kind <> 'pinned'
        THEN
            RAISE EXCEPTION 'runner pinned outbox source is not a pin'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.state_kind = 'runner_lost_before_pin' THEN
        expected_runner := placement.lost_runner_id;
        IF placement.event_kind <> 'runner_lost_before_pin'
           OR placement.state_kind <> 'runner_lost_before_pin'
        THEN
            RAISE EXCEPTION 'pre-pin loss outbox source is not runner loss'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.state_kind = 'runner_lost' THEN
        expected_runner := placement.lost_runner_id;
        IF placement.event_kind <> 'runner_lost'
           OR placement.state_kind <> 'runner_lost'
        THEN
            RAISE EXCEPTION 'runner loss outbox source is not runner loss'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.state_kind = 'replaced' THEN
        IF placement.event_kind = 'pre_pin_replaced'
           AND placement.state_kind = 'unpinned'
        THEN
            expected_runner := placement.selector_runner_id;
        ELSIF placement.event_kind = 'runner_replaced'
              AND placement.state_kind = 'pinned'
        THEN
            SELECT *
              INTO prior
              FROM runner_session_placement_record
             WHERE session_id = placement.session_id
               AND event_ordinal = placement.event_ordinal - 1;
            IF NOT FOUND THEN
                RAISE EXCEPTION 'runner replacement outbox source lacks its predecessor'
                    USING ERRCODE = '23514';
            END IF;
            IF prior.lost_runner_id IS NOT DISTINCT FROM placement.pinned_runner_id
               AND prior.requested_working_directory IS DISTINCT FROM
                    placement.requested_working_directory
            THEN
                RAISE EXCEPTION 'same-runner directory relocation requires its exact outbox state'
                    USING ERRCODE = '23514';
            END IF;
            expected_runner := placement.pinned_runner_id;
        ELSE
            RAISE EXCEPTION 'runner replacement outbox source is not replacement'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.state_kind = 'working_directory_changed' THEN
        SELECT *
          INTO prior
          FROM runner_session_placement_record
         WHERE session_id = placement.session_id
           AND event_ordinal = placement.event_ordinal - 1;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'working-directory outbox source lacks its predecessor'
                USING ERRCODE = '23514';
        END IF;
        expected_runner := placement.pinned_runner_id;
        IF placement.event_kind <> 'runner_replaced'
           OR placement.state_kind <> 'pinned'
           OR prior.lost_runner_id IS DISTINCT FROM placement.pinned_runner_id
           OR prior.requested_working_directory IS NOT DISTINCT FROM
                placement.requested_working_directory
        THEN
            RAISE EXCEPTION 'working-directory outbox source is not relocation'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.state_kind = 'abandoned' THEN
        expected_runner := placement.lost_runner_id;
        IF placement.event_kind <> 'abandoned'
           OR placement.state_kind <> 'runner_abandoned'
        THEN
            RAISE EXCEPTION 'runner abandonment outbox source is not abandonment'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        RAISE EXCEPTION 'unsupported runner outbox state %', NEW.state_kind
            USING ERRCODE = '23514';
    END IF;

    IF expected_runner IS NULL OR expected_runner <> NEW.runner_id THEN
        RAISE EXCEPTION 'runner outbox identity does not match source'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: guard_runner_wire_lease_approval(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_runner_wire_lease_approval() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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
            AND decision_source IS DISTINCT FROM 'user_command'
            AND decision_source IS DISTINCT FROM 'user_override'
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
$$;


--
-- Name: guard_runner_wire_placement_record(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_runner_wire_placement_record() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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
$$;


--
-- Name: guard_session_placement_head(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_session_placement_head() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'DELETE'
       OR (TG_OP = 'INSERT' AND NEW.current_version <> 1)
       OR (TG_OP = 'UPDATE' AND (
            NEW.session_id <> OLD.session_id
            OR NEW.current_version <> OLD.current_version + 1
       )) THEN
        RAISE EXCEPTION 'session placement head must advance by one version';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: lock_runner_loss_identity(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION lock_runner_loss_identity(checked_runner uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM pg_advisory_xact_lock(
        hashtextextended(
            'signalbox.runner-loss-identity.' || checked_runner::text,
            0
        )
    );
END;
$$;


--
-- Name: lock_scheduler_before_runner_recovery_dependency_insert(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION lock_scheduler_before_runner_recovery_dependency_insert() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM 1
      FROM session_scheduler
     WHERE session_id = NEW.session_id
     FOR UPDATE;
    IF NOT FOUND AND TG_TABLE_NAME = 'turn_attempt' THEN
        RAISE EXCEPTION 'turn attempt lacks its session scheduler'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: materialize_legacy_creation_placement(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION materialize_legacy_creation_placement() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    matching_event boolean;
    matching_head boolean;
BEGIN
    IF TG_TABLE_NAME = 'create_session_command' AND NEW.storage_version >= 6 THEN
        RETURN NEW;
    END IF;

    SELECT EXISTS (
        SELECT 1
          FROM session_placement_event
         WHERE session_id = NEW.created_session_id
           AND version = 1
           AND prior_version IS NULL
           AND event_kind = 'created'
           AND placement_path IS NULL
           AND NOT root_global_read_intent
           AND provenance_command_id = NEW.command_id
    ) INTO matching_event;
    SELECT EXISTS (
        SELECT 1
          FROM session_current_placement
         WHERE session_id = NEW.created_session_id
           AND current_version = 1
    ) INTO matching_head;

    IF matching_event AND matching_head THEN
        RETURN NEW;
    END IF;
    IF EXISTS (
        SELECT 1 FROM session_placement_event
         WHERE session_id = NEW.created_session_id
    ) OR EXISTS (
        SELECT 1 FROM session_current_placement
         WHERE session_id = NEW.created_session_id
    ) THEN
        RAISE EXCEPTION
            'legacy session % has a partial or inconsistent placement',
            NEW.created_session_id
            USING ERRCODE = '23514',
                CONSTRAINT = 'legacy_creation_placement_is_consistent';
    END IF;

    INSERT INTO session_placement_event
        (session_id, version, prior_version, event_kind, placement_path,
         root_global_read_intent, provenance_command_id, recorded_at)
    VALUES
        (NEW.created_session_id, 1, NULL, 'created', NULL, FALSE,
         NEW.command_id, transaction_timestamp());
    INSERT INTO session_current_placement (session_id, current_version)
    VALUES (NEW.created_session_id, 1);
    RETURN NEW;
END;
$$;


--
-- Name: reject_runner_lease_claim_after_connection_loss(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_runner_lease_claim_after_connection_loss() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    offered runner_lease_generation%ROWTYPE;
    authority runner_connection_authority_head%ROWTYPE;
    connection_state text;
BEGIN
    IF NEW.state_kind <> 'claimed' THEN
        RETURN NEW;
    END IF;
    SELECT *
      INTO offered
      FROM runner_lease_generation AS lease_generation
     WHERE lease_generation.lease_id = NEW.lease_id
       AND lease_generation.generation = NEW.generation;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner lease claim lacks its generation'
            USING ERRCODE = '23514';
    END IF;
    PERFORM 1
      FROM runner_enrollment
     WHERE enrollment_id = offered.registration_enrollment_id
     FOR SHARE;
    SELECT *
      INTO authority
      FROM runner_connection_authority_head
     WHERE enrollment_id = offered.registration_enrollment_id
     FOR SHARE;
    IF authority.enrollment_id IS NULL THEN
        RAISE EXCEPTION 'runner lease claim lacks connection authority'
            USING ERRCODE = '23514';
    END IF;
    IF offered.offer_connection_epoch IS NULL THEN
        RAISE EXCEPTION 'runner lease claim lacks offer connection authority'
            USING ERRCODE = '23514';
    END IF;
    IF authority.connection_epoch IS DISTINCT FROM
            offered.offer_connection_epoch
       OR authority.latest_loss_epoch IS DISTINCT FROM
            offered.offer_loss_epoch
    THEN
        RAISE EXCEPTION 'runner lease claim crossed a connection loss fence'
            USING ERRCODE = '23514';
    END IF;
    SELECT state_kind
      INTO connection_state
      FROM runner_connection_event
     WHERE enrollment_id = authority.enrollment_id
       AND connection_epoch = authority.connection_epoch
       AND event_ordinal = authority.connection_event_ordinal;
    IF connection_state IS DISTINCT FROM 'connected' THEN
        RAISE EXCEPTION 'runner lease claim lacks a live connection'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: reject_runner_lease_generation_after_connection_loss(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_runner_lease_generation_after_connection_loss() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    authority runner_connection_authority_head%ROWTYPE;
    connection runner_connection_event%ROWTYPE;
    loss runner_connection_loss_epoch%ROWTYPE;
    placement runner_session_placement_record%ROWTYPE;
BEGIN
    IF NEW.offer_connection_epoch IS NOT NULL
       OR NEW.offer_connection_event_ordinal IS NOT NULL
       OR NEW.offer_loss_epoch IS NOT NULL
    THEN
        RAISE EXCEPTION 'runner lease offer authority is adapter-owned'
            USING ERRCODE = '23514';
    END IF;
    PERFORM 1
      FROM runner_enrollment
     WHERE enrollment_id = NEW.registration_enrollment_id
     FOR SHARE;
    SELECT *
      INTO authority
      FROM runner_connection_authority_head
     WHERE enrollment_id = NEW.registration_enrollment_id
     FOR SHARE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner lease offer lacks connection authority'
            USING ERRCODE = '23514';
    END IF;
    SELECT *
      INTO placement
      FROM runner_session_placement_record
     WHERE session_id = NEW.session_id
       AND event_ordinal = NEW.placement_event_ordinal;
    IF NOT FOUND
       OR placement.loss_fence_enrollment_id IS DISTINCT FROM
            NEW.registration_enrollment_id
    THEN
        RAISE EXCEPTION 'runner lease lacks its placement loss baseline'
            USING ERRCODE = '23514';
    END IF;
    IF authority.latest_loss_epoch IS NOT NULL
       AND (
            placement.observed_runner_loss_epoch IS NULL
            OR placement.observed_runner_loss_epoch <
                authority.latest_loss_epoch
       )
    THEN
        RAISE EXCEPTION 'runner placement is fenced by connection loss'
            USING ERRCODE = '23514';
    END IF;
    SELECT *
      INTO connection
      FROM runner_connection_event
     WHERE enrollment_id = authority.enrollment_id
       AND connection_epoch = authority.connection_epoch
       AND event_ordinal = authority.connection_event_ordinal;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner connection authority head lacks its event'
            USING ERRCODE = '23514';
    END IF;
    IF connection.state_kind = 'shutdown' THEN
        RAISE EXCEPTION 'shutdown runner connection cannot authorize a lease offer'
            USING ERRCODE = '23514';
    END IF;
    IF connection.state_kind IS DISTINCT FROM 'lost' THEN
        NEW.offer_connection_epoch := authority.connection_epoch;
        NEW.offer_connection_event_ordinal :=
            authority.connection_event_ordinal;
        NEW.offer_loss_epoch := authority.latest_loss_epoch;
        RETURN NEW;
    END IF;
    SELECT epoch.*
      INTO loss
      FROM runner_current_connection_loss AS current_loss
      JOIN runner_connection_loss_epoch AS epoch
        ON epoch.enrollment_id = current_loss.enrollment_id
       AND epoch.loss_epoch = current_loss.loss_epoch
     WHERE current_loss.enrollment_id = authority.enrollment_id
     FOR SHARE OF current_loss;
    IF authority.latest_loss_epoch IS DISTINCT FROM loss.loss_epoch
       OR loss.connection_epoch IS DISTINCT FROM connection.connection_epoch
       OR loss.connection_event_ordinal IS DISTINCT FROM
            connection.event_ordinal
    THEN
        RAISE EXCEPTION 'lost runner connection lacks its current epoch fence'
            USING ERRCODE = '23514';
    END IF;
    RAISE EXCEPTION 'lost runner connection cannot authorize a lease offer'
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: reject_runner_recovery_reopen(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_runner_recovery_reopen() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.state_kind = 'active'
       AND NEW.active_phase_kind = 'awaiting_runner_recovery'
       AND OLD.active_phase_kind IS DISTINCT FROM 'awaiting_runner_recovery'
       AND NOT (
            OLD.state_kind = 'active'
            AND OLD.active_phase_kind = 'running'
            AND EXISTS (
                SELECT 1
                  FROM turn_attempt AS yielded_attempt
                 WHERE yielded_attempt.turn_attempt_id = OLD.current_attempt_id
                   AND yielded_attempt.turn_id = OLD.turn_id
                   AND yielded_attempt.session_id = OLD.session_id
                   AND yielded_attempt.state_kind = 'ended'
                   AND yielded_attempt.end_variant = 'without_stop'
                   AND yielded_attempt.end_disposition =
                        'yielded_to_durable_wait'
                   AND yielded_attempt.interrupt_command_id IS NULL
                   AND yielded_attempt.interrupt_predecessor_turn_id IS NULL
                   AND NOT EXISTS (
                        SELECT 1
                          FROM turn_attempt AS continuation
                         WHERE continuation.continued_from_attempt_id =
                                yielded_attempt.turn_attempt_id
                   )
            )
       )
    THEN
        RAISE EXCEPTION
            'runner recovery wait requires an active runner boundary'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'active'
       AND OLD.active_phase_kind = 'awaiting_runner_recovery'
       AND NEW.state_kind = 'active'
       AND NOT (
            NOT OLD.delegation_runtime_terminal
            AND NEW.delegation_runtime_terminal
            AND (to_jsonb(OLD) - 'delegation_runtime_terminal') =
                (to_jsonb(NEW) - 'delegation_runtime_terminal')
       )
    THEN
        RAISE EXCEPTION
            'runner recovery wait cannot reopen without a checked replacement'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: require_runner_connection_authority_head_complete(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_connection_authority_head_complete() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM assert_runner_connection_authority_head_complete(NEW.enrollment_id);
    RETURN NULL;
END;
$$;


--
-- Name: require_runner_connection_loss_complete(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_connection_loss_complete() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM assert_runner_connection_loss_complete(
        NEW.enrollment_id,
        NEW.connection_epoch
    );
    RETURN NULL;
END;
$$;


--
-- Name: require_runner_connection_loss_has_propagation(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_connection_loss_has_propagation() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM assert_runner_connection_loss_has_propagation(
        NEW.enrollment_id,
        NEW.loss_epoch
    );
    RETURN NULL;
END;
$$;


--
-- Name: require_runner_enrollment_audit_complete(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_enrollment_audit_complete() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_enrollment uuid :=
        COALESCE(NEW.enrollment_id, OLD.enrollment_id);
    checked_revision numeric :=
        COALESCE(NEW.revision, OLD.revision);
    declared_count numeric;
    actual_count bigint;
    mismatched_classes bigint;
BEGIN
    SELECT allowed_class_count INTO declared_count
      FROM runner_enrollment_audit
     WHERE enrollment_id = checked_enrollment
       AND revision = checked_revision;
    IF NOT FOUND THEN
        RETURN NULL;
    END IF;
    SELECT count(*) INTO actual_count
      FROM runner_enrollment_audit_allowed_class
     WHERE enrollment_id = checked_enrollment
       AND revision = checked_revision;
    SELECT count(*) INTO mismatched_classes
      FROM (
            (
                SELECT capability_class
                  FROM runner_enrollment_allowed_class
                 WHERE enrollment_id = checked_enrollment
                EXCEPT
                SELECT capability_class
                  FROM runner_enrollment_audit_allowed_class
                 WHERE enrollment_id = checked_enrollment
                   AND revision = checked_revision
            )
            UNION ALL
            (
                SELECT capability_class
                  FROM runner_enrollment_audit_allowed_class
                 WHERE enrollment_id = checked_enrollment
                   AND revision = checked_revision
                EXCEPT
                SELECT capability_class
                  FROM runner_enrollment_allowed_class
                 WHERE enrollment_id = checked_enrollment
            )
      ) AS mismatch;
    IF declared_count <> actual_count OR mismatched_classes <> 0 THEN
        RAISE EXCEPTION 'runner enrollment audit class inventory is incomplete'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_runner_enrollment_audit_installed(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_enrollment_audit_installed() RETURNS trigger
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


--
-- Name: require_runner_enrollment_complete(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_enrollment_complete() RETURNS trigger
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


--
-- Name: require_runner_grant_complete(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_grant_complete() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM assert_runner_grant_complete(
        COALESCE(NEW.session_id, OLD.session_id),
        COALESCE(
            NEW.lineage_origin_event_ordinal,
            OLD.lineage_origin_event_ordinal
        ),
        COALESCE(NEW.runner_id, OLD.runner_id),
        COALESCE(NEW.grant_revision, OLD.grant_revision)
    );
    RETURN NULL;
END;
$$;


--
-- Name: require_runner_initial_pin_has_lease(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_initial_pin_has_lease() RETURNS trigger
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


--
-- Name: require_runner_lease_generation_complete(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_lease_generation_complete() RETURNS trigger
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


--
-- Name: require_runner_no_execution_proof_complete(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_no_execution_proof_complete() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM assert_runner_no_execution_proof_complete(
        COALESCE(NEW.lease_id, OLD.lease_id),
        COALESCE(NEW.generation, OLD.generation)
    );
    RETURN NULL;
END;
$$;


--
-- Name: require_runner_physical_attempt_lease_binding_complete(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_physical_attempt_lease_binding_complete() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    binding runner_physical_attempt_lease_binding%ROWTYPE :=
        COALESCE(NEW, OLD);
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM runner_lease_generation AS generation
         WHERE generation.lease_id = binding.lease_id
           AND generation.attempt_id = binding.attempt_id
    )
    THEN
        RAISE EXCEPTION 'runner physical attempt binding lacks its lease lineage'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_runner_placement_complete(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_placement_complete() RETURNS trigger
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


--
-- Name: require_runner_placement_interrupted_attempt_complete(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_placement_interrupted_attempt_complete() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM assert_runner_placement_interrupted_attempt_complete(
        NEW.session_id,
        NEW.event_ordinal
    );
    RETURN NULL;
END;
$$;


--
-- Name: require_runner_profileless_grant_tombstone(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_profileless_grant_tombstone() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    prior_grant_origin numeric;
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
               AND audit.lineage_origin_event_ordinal =
                    grant_record.lineage_origin_event_ordinal
               AND audit.runner_id = grant_record.runner_id
               AND audit.grant_revision = grant_record.grant_revision
               AND audit.event_kind = 'revoked'
             WHERE grant_record.session_id = NEW.session_id
               AND grant_record.lineage_origin_event_ordinal =
                    NEW.credential_grant_lineage_origin_ordinal
               AND grant_record.runner_id = NEW.credential_grant_runner_id
               AND grant_record.grant_revision = NEW.credential_grant_revision
        ) THEN
            RAISE EXCEPTION 'profileless grant authority must be a revoked tombstone'
                USING ERRCODE = '23514';
        END IF;
        IF NEW.event_kind = 'runner_replaced' THEN
            SELECT credential_grant_lineage_origin_ordinal,
                   credential_grant_runner_id, credential_grant_revision
              INTO prior_grant_origin, prior_grant_runner, prior_grant_revision
              FROM runner_session_placement_record
             WHERE session_id = NEW.session_id
               AND event_ordinal = NEW.event_ordinal - 1;
            IF NOT FOUND
               OR NEW.credential_grant_lineage_origin_ordinal IS DISTINCT FROM
                    prior_grant_origin
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
        SELECT credential_grant_lineage_origin_ordinal,
               credential_grant_runner_id, credential_grant_revision
          INTO prior_grant_origin, prior_grant_runner, prior_grant_revision
          FROM runner_session_placement_record
         WHERE session_id = NEW.session_id
           AND event_ordinal = NEW.event_ordinal - 1;
        IF NEW.credential_grant_lineage_origin_ordinal IS DISTINCT FROM
                prior_grant_origin
           OR NEW.credential_grant_runner_id IS DISTINCT FROM prior_grant_runner
           OR NEW.credential_grant_revision IS DISTINCT FROM
                prior_grant_revision
        THEN
            RAISE EXCEPTION 'runner loss changed grant authority'
                USING ERRCODE = '23514';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: require_runner_registration_complete(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_registration_complete() RETURNS trigger
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


--
-- Name: require_runner_retry_attempt_authority(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_retry_attempt_authority() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM tool_attempt AS prior
         WHERE prior.request_id = NEW.request_id
    )
       AND NOT EXISTS (
            SELECT 1
              FROM runner_claimed_retry_attempt_authority AS authority
             WHERE authority.replacement_attempt_id = NEW.attempt_id
               AND authority.replacement_session_id = NEW.session_id
               AND authority.replacement_turn_id = NEW.turn_id
               AND authority.replacement_issuing_turn_attempt_id =
                    NEW.issuing_turn_attempt_id
               AND authority.replacement_request_id = NEW.request_id
               AND authority.replacement_dispatch_generation =
                    NEW.dispatch_generation
       )
    THEN
        RAISE EXCEPTION 'extra tool attempt lacks durable runner retry authority'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: require_runner_retry_replacement_successor_lease(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_retry_replacement_successor_lease() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    authority runner_claimed_retry_attempt_authority%ROWTYPE;
BEGIN
    SELECT * INTO authority
      FROM runner_claimed_retry_attempt_authority
     WHERE replacement_attempt_id = NEW.attempt_id;
    IF NOT FOUND THEN
        RETURN NULL;
    END IF;
    IF NOT EXISTS (
        SELECT 1
          FROM runner_lease_generation AS generation
         WHERE generation.lease_id = authority.source_lease_id
           AND generation.predecessor_generation = authority.source_generation
           AND generation.attempt_id = NEW.attempt_id
    )
    THEN
        RAISE EXCEPTION
            'replacement attempt lacks its atomic successor lease generation'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_runner_retryable_loss_live_attempt(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_retryable_loss_live_attempt() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    lease runner_lease_generation%ROWTYPE;
    attempt_state text;
    attempt_effect text;
BEGIN
    IF NEW.state_kind NOT IN
        ('lost_unclaimed', 'lost_execution_possible', 'lost_claimed')
    THEN
        RETURN NULL;
    END IF;
    SELECT * INTO lease
     FROM runner_lease_generation
     WHERE lease_id = NEW.lease_id
       AND generation = NEW.generation;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner claimed loss lacks its lease generation'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.state_kind <> 'lost_unclaimed'
       AND lease.effect_class = 'side_effecting'
    THEN
        RETURN NULL;
    END IF;
    SELECT state_kind, effect_class
      INTO attempt_state, attempt_effect
      FROM tool_attempt
     WHERE attempt_id = lease.attempt_id
       AND session_id = lease.session_id
       FOR SHARE;
    IF attempt_state IS DISTINCT FROM 'in_flight'
       OR attempt_effect IS DISTINCT FROM (
            CASE lease.effect_class
                WHEN 'pure' THEN 'effect_free'
                ELSE 'external_effect'
            END
       )
    THEN
        RAISE EXCEPTION 'runner retryable loss lacks its live physical attempt'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_runner_tool_request_lease_binding_complete(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_tool_request_lease_binding_complete() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    binding runner_tool_request_lease_binding%ROWTYPE :=
        COALESCE(NEW, OLD);
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM runner_lease_generation AS generation
          JOIN tool_attempt AS attempt
            ON attempt.attempt_id = generation.attempt_id
         WHERE generation.lease_id = binding.lease_id
           AND attempt.request_id = binding.request_id
    )
    THEN
        RAISE EXCEPTION 'runner request binding lacks its lease lineage'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_runner_wire_placement_complete(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_wire_placement_complete() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM assert_runner_wire_placement_complete(
        COALESCE(NEW.session_id, OLD.session_id),
        COALESCE(NEW.event_ordinal, OLD.event_ordinal)
    );
    RETURN NULL;
END;
$$;


--
-- Name: require_runner_wire_registration_complete(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_runner_wire_registration_complete() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM assert_runner_wire_registration_complete(
        COALESCE(NEW.enrollment_id, OLD.enrollment_id),
        COALESCE(NEW.registration_revision, OLD.registration_revision)
    );
    RETURN NULL;
END;
$$;


--
-- Name: require_session_placement(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_session_placement() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM session_current_placement AS placement_head
          JOIN session_placement_event AS placement_event
            ON placement_event.session_id = placement_head.session_id
           AND placement_event.version = placement_head.current_version
         WHERE placement_head.session_id = NEW.session_id
    ) THEN
        RAISE EXCEPTION 'session % requires a complete current placement', NEW.session_id
            USING ERRCODE = '23503',
                CONSTRAINT = 'session_requires_placement';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: runner_lease_placement_reaches_loss_revision(uuid, numeric, numeric, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION runner_lease_placement_reaches_loss_revision(checked_session_id uuid, leased_event_ordinal numeric, checked_loss_revision numeric, checked_runner_id uuid) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT EXISTS (
        SELECT 1
          FROM runner_session_placement_record AS leased_placement
          JOIN runner_session_placement_record AS loss_placement
            ON loss_placement.session_id = leased_placement.session_id
           AND loss_placement.event_kind = 'runner_lost'
           AND loss_placement.state_kind = 'runner_lost'
           AND loss_placement.lost_runner_id = checked_runner_id
           AND loss_placement.placement_revision = checked_loss_revision
           AND loss_placement.event_ordinal > leased_placement.event_ordinal
         WHERE leased_placement.session_id = checked_session_id
           AND leased_placement.event_ordinal = leased_event_ordinal
           AND leased_placement.state_kind = 'pinned'
           AND leased_placement.pinned_runner_id = checked_runner_id
           AND (
                leased_placement.placement_revision = checked_loss_revision
                OR (
                    leased_placement.placement_revision < checked_loss_revision
                    AND EXISTS (
                        SELECT 1
                          FROM runner_session_placement_record AS successor
                         WHERE successor.session_id = checked_session_id
                           AND successor.event_ordinal > leased_event_ordinal
                           AND successor.event_ordinal < loss_placement.event_ordinal
                    )
                    AND NOT EXISTS (
                        SELECT 1
                          FROM runner_session_placement_record AS successor
                         WHERE successor.session_id = checked_session_id
                           AND successor.event_ordinal > leased_event_ordinal
                           AND successor.event_ordinal < loss_placement.event_ordinal
                           AND successor.event_kind <> 'profile_replaced'
                    )
                )
           )
    );
$$;


--
-- Name: serialize_runner_enrollment_loss_identity(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION serialize_runner_enrollment_loss_identity() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM lock_runner_loss_identity(NEW.runner_id);
    RETURN NEW;
END;
$$;


--
-- Name: serialize_runner_placement_loss_identity(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION serialize_runner_placement_loss_identity() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    selected_runner uuid;
BEGIN
    -- This trigger runs immediately before the baseline trigger. It extends
    -- that trigger's existing total order with the absence fence without
    -- duplicating its baseline predicate.
    PERFORM 1
      FROM session_scheduler
     WHERE session_id = NEW.session_id
     FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner placement lacks its session scheduler'
            USING ERRCODE = '23514';
    END IF;

    selected_runner := COALESCE(
        NEW.pinned_runner_id,
        NEW.selector_runner_id,
        NEW.lost_runner_id
    );
    IF selected_runner IS NULL THEN
        RETURN NEW;
    END IF;

    PERFORM lock_runner_loss_identity(selected_runner);
    RETURN NEW;
END;
$$;


--
-- Name: set_runner_placement_loss_baseline(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION set_runner_placement_loss_baseline() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    selected_runner uuid;
    selected_enrollment uuid;
    current_loss_epoch numeric;
    prior runner_session_placement_record%ROWTYPE;
BEGIN
    IF NEW.loss_fence_enrollment_id IS NOT NULL
       OR NEW.observed_runner_loss_epoch IS NOT NULL
    THEN
        RAISE EXCEPTION 'runner placement loss baseline is adapter-derived'
            USING ERRCODE = '23514';
    END IF;

    -- Placement mutation shares the runner total order with loss propagation:
    -- scheduler, enrollment, connection/loss, then the placement insert.
    PERFORM 1
      FROM session_scheduler
     WHERE session_id = NEW.session_id
     FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner placement lacks its session scheduler'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.event_ordinal > 1 THEN
        SELECT * INTO prior
          FROM runner_session_placement_record
         WHERE session_id = NEW.session_id
           AND event_ordinal = NEW.event_ordinal - 1;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'runner placement loss baseline lacks its predecessor'
                USING ERRCODE = '23514';
        END IF;
    END IF;

    selected_runner := COALESCE(
        NEW.pinned_runner_id,
        NEW.selector_runner_id,
        NEW.lost_runner_id
    );
    IF selected_runner IS NULL THEN
        RETURN NEW;
    END IF;

    SELECT enrollment_id
      INTO selected_enrollment
      FROM runner_enrollment
     WHERE runner_id = selected_runner
     FOR SHARE;
    IF NOT FOUND THEN
        RETURN NEW;
    END IF;

    PERFORM 1
      FROM runner_connection_authority_head
     WHERE enrollment_id = selected_enrollment
     FOR SHARE;
    SELECT loss_epoch
      INTO current_loss_epoch
      FROM runner_current_connection_loss
     WHERE enrollment_id = selected_enrollment
     FOR SHARE;

    IF NEW.event_kind IN (
        'runner_lost_before_pin', 'runner_lost', 'abandoned'
    ) THEN
        IF prior.loss_fence_enrollment_id IS NOT NULL
           AND prior.loss_fence_enrollment_id IS DISTINCT FROM
                selected_enrollment
        THEN
            RAISE EXCEPTION
                'runner placement loss changed its enrollment baseline'
                USING ERRCODE = '23514';
        END IF;
        NEW.loss_fence_enrollment_id := prior.loss_fence_enrollment_id;
        NEW.observed_runner_loss_epoch := prior.observed_runner_loss_epoch;
    ELSIF NEW.event_kind = 'pinned'
          AND prior.selector_kind = 'identity'
          AND prior.loss_fence_enrollment_id IS NULL
    THEN
        IF current_loss_epoch IS NOT NULL THEN
            RAISE EXCEPTION
                'runner placement predecessor is fenced by connection loss'
                USING ERRCODE = '23514';
        END IF;
        NEW.loss_fence_enrollment_id := selected_enrollment;
        NEW.observed_runner_loss_epoch := NULL;
    ELSIF NEW.event_kind = 'profile_replaced' OR (
        NEW.event_kind = 'pinned'
        AND prior.selector_kind = 'identity'
    ) THEN
        IF prior.loss_fence_enrollment_id IS DISTINCT FROM selected_enrollment
           OR prior.observed_runner_loss_epoch IS DISTINCT FROM
                current_loss_epoch
        THEN
            RAISE EXCEPTION
                'runner placement predecessor is fenced by connection loss'
                USING ERRCODE = '23514';
        END IF;
        NEW.loss_fence_enrollment_id := prior.loss_fence_enrollment_id;
        NEW.observed_runner_loss_epoch := prior.observed_runner_loss_epoch;
    ELSE
        NEW.loss_fence_enrollment_id := selected_enrollment;
        NEW.observed_runner_loss_epoch := current_loss_epoch;
    END IF;
    RETURN NEW;
END;
$$;


--
-- Tables.
--

--
-- Name: runner_claimed_retry_attempt_authority; Type: TABLE; Schema: public
--

CREATE TABLE runner_claimed_retry_attempt_authority (
    source_lease_id uuid NOT NULL,
    source_generation numeric(20,0) CONSTRAINT runner_claimed_retry_attempt_authori_source_generation_not_null NOT NULL,
    replacement_attempt_id uuid CONSTRAINT runner_claimed_retry_attempt_au_replacement_attempt_id_not_null NOT NULL,
    replacement_session_id uuid CONSTRAINT runner_claimed_retry_attempt_au_replacement_session_id_not_null NOT NULL,
    replacement_turn_id uuid CONSTRAINT runner_claimed_retry_attempt_autho_replacement_turn_id_not_null NOT NULL,
    replacement_issuing_turn_attempt_id uuid CONSTRAINT runner_claimed_retry_attemp_replacement_issuing_turn_a_not_null NOT NULL,
    replacement_request_id uuid CONSTRAINT runner_claimed_retry_attempt_au_replacement_request_id_not_null NOT NULL,
    replacement_dispatch_generation numeric(20,0) CONSTRAINT runner_claimed_retry_attemp_replacement_dispatch_gener_not_null NOT NULL
);


--
-- Name: runner_connection_authority_head; Type: TABLE; Schema: public
--

CREATE TABLE runner_connection_authority_head (
    enrollment_id uuid NOT NULL,
    connection_epoch numeric(20,0) NOT NULL,
    connection_event_ordinal numeric(20,0) CONSTRAINT runner_connection_authority_h_connection_event_ordinal_not_null NOT NULL,
    latest_loss_epoch numeric(20,0),
    CONSTRAINT runner_connection_authority_head_positive_u64 CHECK (((connection_epoch BETWEEN (1)::numeric AND '18446744073709551615'::numeric) AND (connection_event_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric) AND ((latest_loss_epoch IS NULL) OR (latest_loss_epoch BETWEEN (1)::numeric AND '18446744073709551615'::numeric))))
);


--
-- Name: runner_connection_event; Type: TABLE; Schema: public
--

CREATE TABLE runner_connection_event (
    enrollment_id uuid NOT NULL,
    connection_epoch numeric(20,0) NOT NULL,
    event_ordinal numeric(20,0) NOT NULL,
    state_kind text NOT NULL,
    cause_kind text NOT NULL,
    recorded_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT runner_connection_event_cause_shape CHECK ((((state_kind = 'connected'::text) AND (cause_kind = ANY (ARRAY['established'::text, 'heartbeat_recovered'::text]))) OR ((state_kind = 'suspect'::text) AND (cause_kind = 'heartbeat_missed'::text)) OR ((state_kind = 'shutdown'::text) AND (cause_kind = ANY (ARRAY['daemon_shutdown'::text, 'runner_shutdown'::text]))) OR ((state_kind = 'lost'::text) AND (cause_kind = ANY (ARRAY['heartbeat_timeout'::text, 'transport_closed'::text, 'protocol_failure'::text, 'enrollment_revoked'::text]))))),
    CONSTRAINT runner_connection_event_positive_u64 CHECK (((connection_epoch BETWEEN (1)::numeric AND '18446744073709551615'::numeric) AND (event_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric))),
    CONSTRAINT runner_connection_event_state_closed CHECK ((state_kind = ANY (ARRAY['connected'::text, 'suspect'::text, 'shutdown'::text, 'lost'::text])))
);


--
-- Name: runner_connection_loss_epoch; Type: TABLE; Schema: public
--

CREATE TABLE runner_connection_loss_epoch (
    enrollment_id uuid NOT NULL,
    loss_epoch numeric(20,0) NOT NULL,
    connection_epoch numeric(20,0) NOT NULL,
    connection_event_ordinal numeric(20,0) NOT NULL,
    CONSTRAINT runner_connection_loss_epoch_positive_u64 CHECK (((loss_epoch BETWEEN (1)::numeric AND '18446744073709551615'::numeric) AND (connection_epoch BETWEEN (1)::numeric AND '18446744073709551615'::numeric) AND (connection_event_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric)))
);


--
-- Name: runner_connection_loss_propagation; Type: TABLE; Schema: public
--

CREATE TABLE runner_connection_loss_propagation (
    enrollment_id uuid NOT NULL,
    loss_epoch numeric(20,0) NOT NULL,
    propagated_through_session_id uuid,
    state_kind text NOT NULL,
    CONSTRAINT runner_connection_loss_propagation_state_closed CHECK ((state_kind = ANY (ARRAY['pending'::text, 'completed'::text])))
);


--
-- Name: runner_credential_grant; Type: TABLE; Schema: public
--

CREATE TABLE runner_credential_grant (
    session_id uuid NOT NULL,
    lineage_origin_event_ordinal numeric(20,0) NOT NULL,
    runner_id uuid NOT NULL,
    grant_revision numeric(20,0) NOT NULL,
    credential_profile_name runner_catalog_name NOT NULL,
    registration_enrollment_id uuid NOT NULL,
    registration_revision numeric(20,0) NOT NULL,
    placement_event_ordinal numeric(20,0) NOT NULL,
    prior_runner_id uuid,
    prior_grant_revision numeric(20,0),
    tool_count numeric(20,0) NOT NULL,
    CONSTRAINT runner_credential_grant_revision_shape CHECK (((lineage_origin_event_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric) AND (grant_revision BETWEEN (1)::numeric AND '18446744073709551615'::numeric) AND (tool_count BETWEEN (0)::numeric AND '18446744073709551615'::numeric) AND (((grant_revision = (1)::numeric) AND (lineage_origin_event_ordinal = placement_event_ordinal) AND (prior_runner_id IS NULL) AND (prior_grant_revision IS NULL)) OR ((prior_runner_id IS NOT NULL) AND (prior_grant_revision = (grant_revision - (1)::numeric))))))
);


--
-- Name: runner_credential_grant_audit; Type: TABLE; Schema: public
--

CREATE TABLE runner_credential_grant_audit (
    session_id uuid NOT NULL,
    lineage_origin_event_ordinal numeric(20,0) CONSTRAINT runner_credential_grant_aud_lineage_origin_event_ordin_not_null NOT NULL,
    runner_id uuid NOT NULL,
    grant_revision numeric(20,0) NOT NULL,
    audit_ordinal numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    credential_profile_name runner_catalog_name NOT NULL,
    CONSTRAINT runner_credential_grant_audit_shape CHECK ((((audit_ordinal = (1)::numeric) AND (((grant_revision = (1)::numeric) AND (event_kind = 'issued'::text)) OR ((grant_revision > (1)::numeric) AND (event_kind = 'replaced'::text)))) OR ((audit_ordinal = (2)::numeric) AND (event_kind = 'revoked'::text))))
);


--
-- Name: runner_credential_grant_tool; Type: TABLE; Schema: public
--

CREATE TABLE runner_credential_grant_tool (
    session_id uuid NOT NULL,
    lineage_origin_event_ordinal numeric(20,0) CONSTRAINT runner_credential_grant_too_lineage_origin_event_ordin_not_null NOT NULL,
    runner_id uuid NOT NULL,
    grant_revision numeric(20,0) NOT NULL,
    credential_profile_name runner_catalog_name NOT NULL,
    tool_name text NOT NULL,
    approval_kind text NOT NULL,
    CONSTRAINT runner_credential_grant_tool_approval_closed CHECK ((approval_kind = ANY (ARRAY['automatic'::text, 'session_policy'::text])))
);


--
-- Name: runner_current_connection_loss; Type: TABLE; Schema: public
--

CREATE TABLE runner_current_connection_loss (
    enrollment_id uuid NOT NULL,
    loss_epoch numeric(20,0) NOT NULL
);


--
-- Name: runner_current_credential_grant_audit; Type: TABLE; Schema: public
--

CREATE TABLE runner_current_credential_grant_audit (
    session_id uuid NOT NULL,
    lineage_origin_event_ordinal numeric(20,0) CONSTRAINT runner_current_credential_g_lineage_origin_event_ordin_not_null NOT NULL,
    runner_id uuid NOT NULL,
    grant_revision numeric(20,0) NOT NULL,
    audit_ordinal numeric(20,0) NOT NULL,
    event_kind text NOT NULL
);


--
-- Name: runner_current_lease_event; Type: TABLE; Schema: public
--

CREATE TABLE runner_current_lease_event (
    lease_id uuid NOT NULL,
    generation numeric(20,0) NOT NULL,
    event_ordinal numeric(20,0) NOT NULL
);


--
-- Name: runner_current_registration; Type: TABLE; Schema: public
--

CREATE TABLE runner_current_registration (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20,0) NOT NULL
);


--
-- Name: runner_current_session_placement; Type: TABLE; Schema: public
--

CREATE TABLE runner_current_session_placement (
    session_id uuid NOT NULL,
    event_ordinal numeric(20,0) NOT NULL
);


--
-- Name: runner_lease_event; Type: TABLE; Schema: public
--

CREATE TABLE runner_lease_event (
    lease_id uuid NOT NULL,
    generation numeric(20,0) NOT NULL,
    event_ordinal numeric(20,0) NOT NULL,
    state_kind text NOT NULL,
    CONSTRAINT runner_lease_event_state_shape CHECK ((((event_ordinal = (1)::numeric) AND (state_kind = 'offered'::text)) OR ((event_ordinal = (2)::numeric) AND (state_kind = ANY (ARRAY['claimed'::text, 'lost_unclaimed'::text, 'lost_execution_possible'::text]))) OR ((event_ordinal = (3)::numeric) AND (state_kind = ANY (ARRAY['completed'::text, 'lost_claimed'::text])))))
);


--
-- Name: runner_lease_generation; Type: TABLE; Schema: public
--

CREATE TABLE runner_lease_generation (
    lease_id uuid NOT NULL,
    generation numeric(20,0) NOT NULL,
    attempt_id uuid NOT NULL,
    session_id uuid NOT NULL,
    runner_id uuid NOT NULL,
    tool_name text NOT NULL,
    effect_class text NOT NULL,
    placement_event_ordinal numeric(20,0) NOT NULL,
    registration_enrollment_id uuid NOT NULL,
    registration_revision numeric(20,0) NOT NULL,
    credential_profile_name runner_catalog_name,
    credential_grant_lineage_origin_ordinal numeric(20,0),
    credential_grant_revision numeric(20,0),
    credential_approval_kind text,
    predecessor_generation numeric(20,0),
    offer_connection_epoch numeric(20,0),
    offer_connection_event_ordinal numeric(20,0),
    offer_loss_epoch numeric(20,0),
    CONSTRAINT runner_lease_credential_shape CHECK ((((credential_profile_name IS NULL) AND (credential_grant_lineage_origin_ordinal IS NULL) AND (credential_grant_revision IS NULL) AND (credential_approval_kind IS NULL)) OR ((credential_profile_name IS NOT NULL) AND (credential_grant_lineage_origin_ordinal IS NOT NULL) AND (credential_grant_revision IS NOT NULL) AND (credential_approval_kind = ANY (ARRAY['automatic'::text, 'session_policy'::text]))))),
    CONSTRAINT runner_lease_effect_closed CHECK ((effect_class = ANY (ARRAY['pure'::text, 'idempotent'::text, 'side_effecting'::text]))),
    CONSTRAINT runner_lease_generation_positive_u64 CHECK (((generation >= (1)::numeric) AND (generation <= '18446744073709551615'::numeric))),
    CONSTRAINT runner_lease_offer_connection_shape CHECK ((((offer_connection_epoch IS NULL) AND (offer_connection_event_ordinal IS NULL) AND (offer_loss_epoch IS NULL)) OR ((offer_connection_epoch BETWEEN (1)::numeric AND '18446744073709551615'::numeric) AND (offer_connection_event_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric) AND ((offer_loss_epoch IS NULL) OR (offer_loss_epoch BETWEEN (1)::numeric AND '18446744073709551615'::numeric))))),
    CONSTRAINT runner_lease_predecessor_shape CHECK ((((generation = (1)::numeric) AND (predecessor_generation IS NULL)) OR ((generation > (1)::numeric) AND (predecessor_generation = (generation - (1)::numeric)))))
);


--
-- Name: runner_current_tool_attempt; Type: VIEW; Schema: public
--

CREATE VIEW runner_current_tool_attempt AS
 SELECT attempt_id,
    request_id,
    session_id,
    turn_id,
    issuing_turn_attempt_id,
    effect_class,
    dispatch_generation,
    state_kind,
    terminal_disposition_kind,
    result_content_kind,
    result_text,
    error_kind,
    error_detail,
    wait_spawning_request_id,
    wait_child_session_id
   FROM tool_attempt attempt
  WHERE (NOT ((state_kind = 'terminal'::text) AND (EXISTS ( SELECT 1
           FROM ((runner_lease_generation generation
             JOIN runner_current_lease_event current_event ON (((current_event.lease_id = generation.lease_id) AND (current_event.generation = generation.generation))))
             JOIN runner_lease_event event ON (((event.lease_id = current_event.lease_id) AND (event.generation = current_event.generation) AND (event.event_ordinal = current_event.event_ordinal))))
          WHERE ((generation.attempt_id = attempt.attempt_id) AND (generation.effect_class = ANY (ARRAY['pure'::text, 'idempotent'::text])) AND (event.state_kind = ANY (ARRAY['lost_execution_possible'::text, 'lost_claimed'::text])))))));


--
-- Name: runner_enrollment; Type: TABLE; Schema: public
--

CREATE TABLE runner_enrollment (
    enrollment_id uuid NOT NULL,
    runner_id uuid NOT NULL,
    authentication_reference_id uuid NOT NULL,
    allowed_class_count numeric(20,0) NOT NULL,
    revision numeric(20,0) NOT NULL,
    state_kind text NOT NULL,
    CONSTRAINT runner_enrollment_class_count_u64 CHECK (((allowed_class_count >= (0)::numeric) AND (allowed_class_count <= '18446744073709551615'::numeric))),
    CONSTRAINT runner_enrollment_state_shape CHECK ((((revision = (1)::numeric) AND (state_kind = 'active'::text)) OR ((revision = (2)::numeric) AND (state_kind = 'revoked'::text))))
);


--
-- Name: runner_enrollment_allowed_class; Type: TABLE; Schema: public
--

CREATE TABLE runner_enrollment_allowed_class (
    enrollment_id uuid NOT NULL,
    capability_class runner_catalog_name NOT NULL
);


--
-- Name: runner_enrollment_audit; Type: TABLE; Schema: public
--

CREATE TABLE runner_enrollment_audit (
    enrollment_id uuid NOT NULL,
    revision numeric(20,0) NOT NULL,
    runner_id uuid NOT NULL,
    authentication_reference_id uuid NOT NULL,
    allowed_class_count numeric(20,0) NOT NULL,
    state_kind text NOT NULL,
    CONSTRAINT runner_enrollment_audit_revision_positive_u64 CHECK (((revision BETWEEN (1)::numeric AND '18446744073709551615'::numeric) AND (allowed_class_count BETWEEN (0)::numeric AND '18446744073709551615'::numeric))),
    CONSTRAINT runner_enrollment_audit_state_closed CHECK ((state_kind = ANY (ARRAY['active'::text, 'revoked'::text]))),
    CONSTRAINT runner_enrollment_audit_state_shape CHECK ((((revision = (1)::numeric) AND (state_kind = 'active'::text)) OR ((revision = (2)::numeric) AND (state_kind = 'revoked'::text))))
);


--
-- Name: runner_enrollment_audit_allowed_class; Type: TABLE; Schema: public
--

CREATE TABLE runner_enrollment_audit_allowed_class (
    enrollment_id uuid NOT NULL,
    revision numeric(20,0) NOT NULL,
    capability_class runner_catalog_name NOT NULL
);


--
-- Name: runner_enrollment_request_receipt; Type: TABLE; Schema: public
--

CREATE TABLE runner_enrollment_request_receipt (
    request_id uuid NOT NULL,
    enrollment_id uuid NOT NULL,
    runner_id uuid NOT NULL,
    authentication_reference_id uuid CONSTRAINT runner_enrollment_request_r_authentication_reference_i_not_null NOT NULL,
    registration_revision numeric(20,0) CONSTRAINT runner_enrollment_request_receip_registration_revision_not_null NOT NULL,
    CONSTRAINT runner_enrollment_request_receipt_initial_revision CHECK ((registration_revision = (1)::numeric))
);


--
-- Name: runner_lease_no_execution_proof; Type: TABLE; Schema: public
--

CREATE TABLE runner_lease_no_execution_proof (
    lease_id uuid NOT NULL,
    generation numeric(20,0) NOT NULL,
    attempt_id uuid NOT NULL,
    session_id uuid NOT NULL,
    runner_id uuid NOT NULL,
    tool_name text NOT NULL,
    turn_id uuid NOT NULL,
    issuing_turn_attempt_id uuid CONSTRAINT runner_lease_no_execution_proo_issuing_turn_attempt_id_not_null NOT NULL,
    request_id uuid NOT NULL,
    dispatch_generation numeric(20,0) NOT NULL
);


--
-- Name: runner_physical_attempt_lease_binding; Type: TABLE; Schema: public
--

CREATE TABLE runner_physical_attempt_lease_binding (
    attempt_id uuid NOT NULL,
    lease_id uuid NOT NULL
);


--
-- Name: runner_registration; Type: TABLE; Schema: public
--

CREATE TABLE runner_registration (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20,0) NOT NULL,
    runner_id uuid NOT NULL,
    authentication_reference_id uuid NOT NULL,
    class_count numeric(20,0) NOT NULL,
    tool_count numeric(20,0) NOT NULL,
    profile_count numeric(20,0) NOT NULL,
    workspace_count numeric(20,0) NOT NULL,
    repository_count numeric(20,0) NOT NULL,
    sandbox_count numeric(20,0) NOT NULL,
    CONSTRAINT runner_registration_counts_u64 CHECK (((class_count BETWEEN (0)::numeric AND '18446744073709551615'::numeric) AND (tool_count BETWEEN (0)::numeric AND '18446744073709551615'::numeric) AND (profile_count BETWEEN (0)::numeric AND '18446744073709551615'::numeric) AND (workspace_count BETWEEN (0)::numeric AND '18446744073709551615'::numeric))),
    CONSTRAINT runner_registration_revision_positive_u64 CHECK (((registration_revision >= (1)::numeric) AND (registration_revision <= '18446744073709551615'::numeric))),
    CONSTRAINT runner_registration_wire_counts_bounded CHECK (((repository_count BETWEEN (0)::numeric AND (64)::numeric) AND (sandbox_count BETWEEN (0)::numeric AND (2)::numeric)))
);


--
-- Name: runner_registration_class; Type: TABLE; Schema: public
--

CREATE TABLE runner_registration_class (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20,0) NOT NULL,
    capability_class runner_catalog_name NOT NULL
);


--
-- Name: runner_registration_profile; Type: TABLE; Schema: public
--

CREATE TABLE runner_registration_profile (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20,0) NOT NULL,
    credential_profile_name runner_catalog_name NOT NULL,
    approval_count numeric(20,0) NOT NULL,
    CONSTRAINT runner_registration_profile_approval_count_u64 CHECK (((approval_count >= (0)::numeric) AND (approval_count <= '18446744073709551615'::numeric)))
);


--
-- Name: runner_registration_profile_approval; Type: TABLE; Schema: public
--

CREATE TABLE runner_registration_profile_approval (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20,0) CONSTRAINT runner_registration_profile_appr_registration_revision_not_null NOT NULL,
    credential_profile_name runner_catalog_name CONSTRAINT runner_registration_profile_ap_credential_profile_name_not_null NOT NULL,
    tool_name text NOT NULL,
    approval_kind text NOT NULL,
    CONSTRAINT runner_registration_profile_approval_closed CHECK ((approval_kind = ANY (ARRAY['automatic'::text, 'session_policy'::text]))),
    CONSTRAINT runner_registration_profile_approval_tool_name_shape CHECK (((octet_length(tool_name) BETWEEN 1 AND 64) AND (tool_name ~ '^[A-Za-z0-9_-]+$'::text)))
);


--
-- Name: runner_registration_repository; Type: TABLE; Schema: public
--

CREATE TABLE runner_registration_repository (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20,0) NOT NULL,
    repository_key runner_catalog_name NOT NULL,
    credential_profile_name runner_catalog_name
);


--
-- Name: runner_registration_sandbox; Type: TABLE; Schema: public
--

CREATE TABLE runner_registration_sandbox (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20,0) NOT NULL,
    sandbox_profile text NOT NULL,
    CONSTRAINT runner_registration_sandbox_closed CHECK ((sandbox_profile = ANY (ARRAY['ambient'::text, 'workspace_restricted'::text])))
);


--
-- Name: runner_registration_tool; Type: TABLE; Schema: public
--

CREATE TABLE runner_registration_tool (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20,0) NOT NULL,
    tool_name text NOT NULL,
    model_description runner_exact_text NOT NULL,
    model_input_schema runner_tool_schema NOT NULL,
    permission_kind text NOT NULL,
    effect_class text NOT NULL,
    loci_kind text NOT NULL,
    selector_kind text,
    selector_runner_id uuid,
    selector_capability_class runner_catalog_name,
    CONSTRAINT runner_registration_tool_effect_closed CHECK ((effect_class = ANY (ARRAY['pure'::text, 'idempotent'::text, 'side_effecting'::text]))),
    CONSTRAINT runner_registration_tool_idempotent_runner_only CHECK (((effect_class <> 'idempotent'::text) OR (loci_kind = 'runner_only'::text))),
    CONSTRAINT runner_registration_tool_loci_closed CHECK ((loci_kind = ANY (ARRAY['runner_only'::text, 'daemon_or_runner'::text]))),
    CONSTRAINT runner_registration_tool_model_schema CHECK (((canonical_tool_json((model_input_schema)::text) IS NOT NULL) AND ((model_input_schema)::text = canonical_tool_json((model_input_schema)::text)) AND ("left"((model_input_schema)::text, 1) = '{'::text))),
    CONSTRAINT runner_registration_tool_name_shape CHECK (((octet_length(tool_name) BETWEEN 1 AND 64) AND (tool_name ~ '^[A-Za-z0-9_-]+$'::text))),
    CONSTRAINT runner_registration_tool_permission_closed CHECK ((permission_kind = ANY (ARRAY['auto'::text, 'confirm'::text, 'always_confirm'::text]))),
    CONSTRAINT runner_registration_tool_selector_shape CHECK (((selector_kind IS NOT NULL) AND (((selector_kind = 'identity'::text) AND (selector_runner_id IS NOT NULL) AND (selector_capability_class IS NULL)) OR ((selector_kind = 'capability_class'::text) AND (selector_runner_id IS NULL) AND (selector_capability_class IS NOT NULL)))))
);


--
-- Name: runner_registration_workspace; Type: TABLE; Schema: public
--

CREATE TABLE runner_registration_workspace (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20,0) NOT NULL,
    workspace_kind text NOT NULL,
    CONSTRAINT runner_registration_workspace_closed CHECK ((workspace_kind = 'worktree_per_session'::text))
);


--
-- Name: runner_session_placement_permission_override; Type: TABLE; Schema: public
--

CREATE TABLE runner_session_placement_permission_override (
    session_id uuid CONSTRAINT runner_session_placement_permission_overrid_session_id_not_null NOT NULL,
    event_ordinal numeric(20,0) CONSTRAINT runner_session_placement_permission_over_event_ordinal_not_null NOT NULL,
    tool_name text NOT NULL,
    permission_kind text CONSTRAINT runner_session_placement_permission_ov_permission_kind_not_null NOT NULL,
    CONSTRAINT runner_session_placement_permission_override_closed CHECK ((permission_kind = ANY (ARRAY['auto'::text, 'confirm'::text]))),
    CONSTRAINT runner_session_placement_permission_override_tool_shape CHECK (((octet_length(tool_name) BETWEEN 1 AND 64) AND (tool_name ~ '^[A-Za-z0-9_-]+$'::text)))
);


--
-- Name: runner_session_placement_record; Type: TABLE; Schema: public
--

CREATE TABLE runner_session_placement_record (
    session_id uuid NOT NULL,
    event_ordinal numeric(20,0) NOT NULL,
    placement_revision numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    selector_kind text NOT NULL,
    selector_runner_id uuid,
    selector_capability_class runner_catalog_name,
    directory_selection_kind text CONSTRAINT runner_session_placement_reco_directory_selection_kind_not_null NOT NULL,
    requested_working_directory runner_exact_text,
    requested_credential_profile_name runner_catalog_name,
    workspace_requirement_kind text CONSTRAINT runner_session_placement_re_workspace_requirement_kind_not_null NOT NULL,
    requested_repository_key runner_exact_text,
    state_kind text NOT NULL,
    pinned_runner_id uuid,
    pinned_working_directory runner_exact_text,
    pinned_credential_profile_name runner_catalog_name,
    registration_enrollment_id uuid,
    registration_revision numeric(20,0),
    pinned_tool_count numeric(20,0) NOT NULL,
    workspace_repository_key runner_exact_text,
    workspace_working_directory runner_exact_text,
    credential_grant_lineage_origin_ordinal numeric(20,0),
    credential_grant_revision numeric(20,0),
    credential_grant_runner_id uuid,
    requested_sandbox_profile text CONSTRAINT runner_session_placement_rec_requested_sandbox_profile_not_null NOT NULL,
    permission_override_count numeric(20,0) CONSTRAINT runner_session_placement_rec_permission_override_count_not_null NOT NULL,
    workspace_manifest_id uuid,
    workspace_placement_revision numeric(20,0),
    workspace_clone_url_digest text,
    workspace_credential_profile_name runner_catalog_name,
    workspace_sandbox_profile text,
    workspace_relative_path runner_exact_text,
    workspace_recovery_kind text,
    workspace_branch_name text,
    workspace_revision text,
    lost_runner_id uuid,
    loss_source_kind text,
    interrupted_tool_attempt_id uuid,
    loss_fence_enrollment_id uuid,
    observed_runner_loss_epoch numeric(20,0),
    CONSTRAINT runner_session_placement_clone_url_digest_hex CHECK (((workspace_clone_url_digest IS NULL) OR (workspace_clone_url_digest ~ '^[0-9a-f]{64}$'::text))),
    CONSTRAINT runner_session_placement_directory_shape CHECK ((((directory_selection_kind = 'runner_default'::text) AND (requested_working_directory IS NULL)) OR ((directory_selection_kind = 'exact'::text) AND (requested_working_directory IS NOT NULL)))),
    CONSTRAINT runner_session_placement_event_closed CHECK ((event_kind = ANY (ARRAY['created'::text, 'pinned'::text, 'runner_lost_before_pin'::text, 'pre_pin_replaced'::text, 'runner_lost'::text, 'runner_replaced'::text, 'abandoned'::text, 'profile_replaced'::text]))),
    CONSTRAINT runner_session_placement_grant_pointer_shape CHECK ((((credential_grant_runner_id IS NULL) = (credential_grant_lineage_origin_ordinal IS NULL)) AND ((credential_grant_lineage_origin_ordinal IS NULL) = (credential_grant_revision IS NULL)))),
    CONSTRAINT runner_session_placement_interrupted_attempt_shape CHECK (((interrupted_tool_attempt_id IS NULL) OR (event_kind = 'runner_lost'::text))),
    CONSTRAINT runner_session_placement_loss_fence_shape CHECK ((((loss_fence_enrollment_id IS NULL) AND (observed_runner_loss_epoch IS NULL)) OR (loss_fence_enrollment_id IS NOT NULL))),
    CONSTRAINT runner_session_placement_loss_source_closed CHECK (((loss_source_kind IS NULL) OR (loss_source_kind = ANY (ARRAY['connection'::text, 'registration'::text])))),
    CONSTRAINT runner_session_placement_observed_loss_positive CHECK (((observed_runner_loss_epoch IS NULL) OR ((observed_runner_loss_epoch >= (1)::numeric) AND (observed_runner_loss_epoch <= '18446744073709551615'::numeric)))),
    CONSTRAINT runner_session_placement_record_positive_u64 CHECK (((event_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric) AND (placement_revision BETWEEN (1)::numeric AND '18446744073709551615'::numeric) AND (pinned_tool_count BETWEEN (0)::numeric AND '18446744073709551615'::numeric) AND ((credential_grant_lineage_origin_ordinal IS NULL) OR (credential_grant_lineage_origin_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric)) AND ((credential_grant_revision IS NULL) OR (credential_grant_revision BETWEEN (1)::numeric AND '18446744073709551615'::numeric)))),
    CONSTRAINT runner_session_placement_relative_path_shape CHECK (((workspace_relative_path IS NULL) OR ((workspace_relative_path)::text !~ '(^/|//|(^|/)\.{1,2}(/|$))'::text))),
    CONSTRAINT runner_session_placement_repository_key_shape CHECK ((((requested_repository_key IS NULL) OR ((octet_length((requested_repository_key)::text) BETWEEN 1 AND 64) AND ((requested_repository_key)::text ~ '^[A-Za-z0-9][A-Za-z0-9._-]*$'::text))) AND ((workspace_repository_key IS NULL) OR ((octet_length((workspace_repository_key)::text) BETWEEN 1 AND 64) AND ((workspace_repository_key)::text ~ '^[A-Za-z0-9][A-Za-z0-9._-]*$'::text))))),
    CONSTRAINT runner_session_placement_sandbox_closed CHECK (((requested_sandbox_profile = ANY (ARRAY['ambient'::text, 'workspace_restricted'::text])) AND ((workspace_sandbox_profile IS NULL) OR (workspace_sandbox_profile = ANY (ARRAY['ambient'::text, 'workspace_restricted'::text]))))),
    CONSTRAINT runner_session_placement_selector_shape CHECK ((((selector_kind = 'identity'::text) AND (selector_runner_id IS NOT NULL) AND (selector_capability_class IS NULL)) OR ((selector_kind = 'capability_class'::text) AND (selector_runner_id IS NULL) AND (selector_capability_class IS NOT NULL)))),
    CONSTRAINT runner_session_placement_state_shape CHECK ((((state_kind = 'unpinned'::text) AND (event_kind = ANY (ARRAY['created'::text, 'pre_pin_replaced'::text])) AND (((event_kind = 'created'::text) AND (event_ordinal = (1)::numeric) AND (placement_revision = (1)::numeric)) OR ((event_kind = 'pre_pin_replaced'::text) AND (event_ordinal > (1)::numeric) AND (placement_revision > (1)::numeric))) AND (lost_runner_id IS NULL) AND (loss_source_kind IS NULL) AND (pinned_runner_id IS NULL) AND (pinned_working_directory IS NULL) AND (pinned_credential_profile_name IS NULL) AND (registration_enrollment_id IS NULL) AND (registration_revision IS NULL) AND (pinned_tool_count = (0)::numeric) AND (workspace_repository_key IS NULL) AND (workspace_working_directory IS NULL) AND (credential_grant_runner_id IS NULL) AND (credential_grant_lineage_origin_ordinal IS NULL) AND (credential_grant_revision IS NULL)) OR ((state_kind = 'runner_lost_before_pin'::text) AND (event_kind = 'runner_lost_before_pin'::text) AND (lost_runner_id IS NOT NULL) AND (loss_source_kind IS NULL) AND (selector_kind = 'identity'::text) AND (selector_runner_id = lost_runner_id) AND (pinned_runner_id IS NULL) AND (pinned_working_directory IS NULL) AND (pinned_credential_profile_name IS NULL) AND (registration_enrollment_id IS NULL) AND (registration_revision IS NULL) AND (pinned_tool_count = (0)::numeric) AND (workspace_repository_key IS NULL) AND (workspace_working_directory IS NULL) AND (credential_grant_runner_id IS NULL) AND (credential_grant_lineage_origin_ordinal IS NULL) AND (credential_grant_revision IS NULL)) OR ((state_kind = 'pinned'::text) AND (event_kind = ANY (ARRAY['pinned'::text, 'runner_replaced'::text, 'profile_replaced'::text])) AND (lost_runner_id IS NULL) AND (loss_source_kind IS NULL) AND (pinned_runner_id IS NOT NULL) AND (pinned_working_directory IS NOT NULL) AND (NOT ((pinned_credential_profile_name)::text IS DISTINCT FROM (requested_credential_profile_name)::text)) AND (registration_enrollment_id IS NOT NULL) AND (registration_revision IS NOT NULL) AND (((pinned_credential_profile_name IS NULL) AND (((credential_grant_runner_id IS NULL) AND (credential_grant_lineage_origin_ordinal IS NULL) AND (credential_grant_revision IS NULL)) OR ((credential_grant_runner_id IS NOT NULL) AND (credential_grant_lineage_origin_ordinal IS NOT NULL) AND (credential_grant_revision IS NOT NULL)))) OR ((pinned_credential_profile_name IS NOT NULL) AND (credential_grant_runner_id = pinned_runner_id) AND (credential_grant_lineage_origin_ordinal IS NOT NULL) AND (credential_grant_revision IS NOT NULL)))) OR ((state_kind = 'runner_lost'::text) AND (event_kind = 'runner_lost'::text) AND (lost_runner_id = pinned_runner_id) AND (loss_source_kind IS NOT NULL) AND (pinned_runner_id IS NOT NULL) AND (pinned_working_directory IS NOT NULL) AND (NOT ((pinned_credential_profile_name)::text IS DISTINCT FROM (requested_credential_profile_name)::text)) AND (registration_enrollment_id IS NOT NULL) AND (registration_revision IS NOT NULL) AND (((pinned_credential_profile_name IS NULL) AND (((credential_grant_runner_id IS NULL) AND (credential_grant_lineage_origin_ordinal IS NULL) AND (credential_grant_revision IS NULL)) OR ((credential_grant_runner_id IS NOT NULL) AND (credential_grant_lineage_origin_ordinal IS NOT NULL) AND (credential_grant_revision IS NOT NULL)))) OR ((pinned_credential_profile_name IS NOT NULL) AND (credential_grant_runner_id = pinned_runner_id) AND (credential_grant_lineage_origin_ordinal IS NOT NULL) AND (credential_grant_revision IS NOT NULL)))) OR ((state_kind = 'runner_abandoned'::text) AND (event_kind = 'abandoned'::text) AND (lost_runner_id IS NOT NULL) AND (((loss_source_kind IS NULL) AND (selector_kind = 'identity'::text) AND (selector_runner_id = lost_runner_id) AND (pinned_runner_id IS NULL) AND (pinned_working_directory IS NULL) AND (pinned_credential_profile_name IS NULL) AND (registration_enrollment_id IS NULL) AND (registration_revision IS NULL) AND (pinned_tool_count = (0)::numeric) AND (workspace_repository_key IS NULL) AND (workspace_working_directory IS NULL) AND (credential_grant_runner_id IS NULL) AND (credential_grant_lineage_origin_ordinal IS NULL) AND (credential_grant_revision IS NULL)) OR ((loss_source_kind IS NOT NULL) AND (lost_runner_id = pinned_runner_id) AND (pinned_runner_id IS NOT NULL) AND (pinned_working_directory IS NOT NULL) AND (NOT ((pinned_credential_profile_name)::text IS DISTINCT FROM (requested_credential_profile_name)::text)) AND (registration_enrollment_id IS NOT NULL) AND (registration_revision IS NOT NULL) AND (((pinned_credential_profile_name IS NULL) AND (((credential_grant_runner_id IS NULL) AND (credential_grant_lineage_origin_ordinal IS NULL) AND (credential_grant_revision IS NULL)) OR ((credential_grant_runner_id IS NOT NULL) AND (credential_grant_lineage_origin_ordinal IS NOT NULL) AND (credential_grant_revision IS NOT NULL)))) OR ((pinned_credential_profile_name IS NOT NULL) AND (credential_grant_runner_id = pinned_runner_id) AND (credential_grant_lineage_origin_ordinal IS NOT NULL) AND (credential_grant_revision IS NOT NULL)))))))),
    CONSTRAINT runner_session_placement_wire_u64 CHECK (((permission_override_count BETWEEN (0)::numeric AND (64)::numeric) AND ((workspace_placement_revision IS NULL) OR (workspace_placement_revision BETWEEN (1)::numeric AND '18446744073709551615'::numeric)))),
    CONSTRAINT runner_session_placement_workspace_branch_shape CHECK (((workspace_branch_name IS NULL) OR ((octet_length(workspace_branch_name) BETWEEN 1 AND 255) AND (workspace_branch_name !~ '[[:cntrl:] ~^:?*]'::text) AND (POSITION(('['::text) IN (workspace_branch_name)) = 0) AND (POSITION((chr(92)) IN (workspace_branch_name)) = 0) AND (workspace_branch_name !~ '(^-|^/|/$|//|\.\.|@\{|\.$)'::text) AND (workspace_branch_name !~ '(^|/)\.'::text) AND (workspace_branch_name !~ '\.lock(?:/|$)'::text) AND (workspace_branch_name <> '@'::text)))),
    CONSTRAINT runner_session_placement_workspace_revision_hex CHECK (((workspace_revision IS NULL) OR (workspace_revision ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'::text))),
    CONSTRAINT runner_session_placement_workspace_shape CHECK ((((pinned_runner_id IS NULL) AND (workspace_repository_key IS NULL) AND (workspace_working_directory IS NULL) AND (workspace_manifest_id IS NULL) AND (workspace_placement_revision IS NULL) AND (workspace_clone_url_digest IS NULL) AND (workspace_credential_profile_name IS NULL) AND (workspace_sandbox_profile IS NULL) AND (workspace_relative_path IS NULL) AND (workspace_recovery_kind IS NULL) AND (workspace_branch_name IS NULL) AND (workspace_revision IS NULL)) OR ((pinned_runner_id IS NOT NULL) AND (workspace_requirement_kind = 'none'::text) AND (requested_repository_key IS NULL) AND (((workspace_repository_key IS NULL) AND (workspace_working_directory IS NULL) AND (workspace_manifest_id IS NULL) AND (workspace_placement_revision IS NULL) AND (workspace_clone_url_digest IS NULL) AND (workspace_credential_profile_name IS NULL) AND (workspace_sandbox_profile IS NULL) AND (workspace_relative_path IS NULL) AND (workspace_recovery_kind IS NULL) AND (workspace_branch_name IS NULL) AND (workspace_revision IS NULL) AND ((requested_sandbox_profile = 'ambient'::text) OR (directory_selection_kind = 'exact'::text))) OR ((requested_sandbox_profile = 'workspace_restricted'::text) AND (directory_selection_kind = 'runner_default'::text) AND (workspace_repository_key IS NULL) AND ((workspace_working_directory)::text = (pinned_working_directory)::text) AND (workspace_manifest_id IS NOT NULL) AND (workspace_placement_revision IS NOT NULL) AND (workspace_clone_url_digest IS NULL) AND (workspace_credential_profile_name IS NULL) AND (workspace_sandbox_profile = requested_sandbox_profile) AND (workspace_relative_path IS NOT NULL) AND (workspace_recovery_kind IS NULL) AND (workspace_branch_name IS NULL) AND (workspace_revision IS NULL)))) OR ((pinned_runner_id IS NOT NULL) AND (workspace_requirement_kind = 'repository_worktree'::text) AND (requested_repository_key IS NOT NULL) AND ((workspace_repository_key)::text = (requested_repository_key)::text) AND ((workspace_working_directory)::text = (pinned_working_directory)::text) AND (workspace_manifest_id IS NOT NULL) AND (workspace_placement_revision IS NOT NULL) AND (workspace_clone_url_digest IS NOT NULL) AND (NOT ((workspace_credential_profile_name)::text IS DISTINCT FROM (requested_credential_profile_name)::text)) AND (workspace_sandbox_profile = requested_sandbox_profile) AND (workspace_relative_path IS NOT NULL) AND (workspace_recovery_kind = ANY (ARRAY['commit'::text, 'branch'::text])) AND (workspace_revision IS NOT NULL) AND (((workspace_recovery_kind = 'commit'::text) AND (workspace_branch_name IS NULL)) OR ((workspace_recovery_kind = 'branch'::text) AND (workspace_branch_name IS NOT NULL))))))
);


--
-- Name: runner_session_placement_tool; Type: TABLE; Schema: public
--

CREATE TABLE runner_session_placement_tool (
    session_id uuid NOT NULL,
    event_ordinal numeric(20,0) NOT NULL,
    tool_name text NOT NULL,
    runner_required boolean NOT NULL
);


--
-- Name: runner_state_transition_outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE runner_state_transition_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    runner_id uuid NOT NULL,
    placement_revision numeric(20,0) CONSTRAINT runner_state_transition_outbox_even_placement_revision_not_null NOT NULL,
    sandbox_profile text NOT NULL,
    working_directory runner_exact_text,
    state_kind text NOT NULL,
    placement_event_ordinal numeric(20,0) CONSTRAINT runner_state_transition_outbox_placement_event_ordinal_not_null NOT NULL,
    connection_enrollment_id uuid,
    connection_epoch numeric(20,0),
    connection_event_ordinal numeric(20,0),
    CONSTRAINT runner_state_transition_outbox_kind_closed CHECK ((event_kind = 'runner_state_transition'::text)),
    CONSTRAINT runner_state_transition_outbox_positive_u64 CHECK (((placement_revision BETWEEN (1)::numeric AND '18446744073709551615'::numeric) AND (placement_event_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric) AND ((connection_epoch IS NULL) OR (connection_epoch BETWEEN (1)::numeric AND '18446744073709551615'::numeric)) AND ((connection_event_ordinal IS NULL) OR (connection_event_ordinal BETWEEN (1)::numeric AND '18446744073709551615'::numeric)))),
    CONSTRAINT runner_state_transition_outbox_sandbox_closed CHECK ((sandbox_profile = ANY (ARRAY['ambient'::text, 'workspace_restricted'::text]))),
    CONSTRAINT runner_state_transition_outbox_source_shape CHECK ((((state_kind = ANY (ARRAY['suspect'::text, 'connected'::text])) AND (connection_enrollment_id IS NOT NULL) AND (connection_epoch IS NOT NULL) AND (connection_event_ordinal IS NOT NULL)) OR ((state_kind <> ALL (ARRAY['suspect'::text, 'connected'::text])) AND (connection_enrollment_id IS NULL) AND (connection_epoch IS NULL) AND (connection_event_ordinal IS NULL)))),
    CONSTRAINT runner_state_transition_outbox_state_closed CHECK ((state_kind = ANY (ARRAY['pinned'::text, 'suspect'::text, 'connected'::text, 'runner_lost_before_pin'::text, 'runner_lost'::text, 'replaced'::text, 'working_directory_changed'::text, 'abandoned'::text]))),
    CONSTRAINT runner_state_transition_outbox_version_supported CHECK ((storage_version = 1))
);


--
-- Name: runner_tool_request_lease_binding; Type: TABLE; Schema: public
--

CREATE TABLE runner_tool_request_lease_binding (
    request_id uuid NOT NULL,
    lease_id uuid NOT NULL
);


--
-- Name: session_current_placement; Type: TABLE; Schema: public
--

CREATE TABLE session_current_placement (
    session_id uuid NOT NULL,
    current_version numeric(20,0) NOT NULL,
    CONSTRAINT session_current_placement_current_version_check CHECK (((current_version >= (1)::numeric) AND (current_version <= '18446744073709551615'::numeric)))
);


--
-- Name: session_placement_event; Type: TABLE; Schema: public
--

CREATE TABLE session_placement_event (
    session_id uuid NOT NULL,
    version numeric(20,0) NOT NULL,
    prior_version numeric(20,0),
    event_kind text NOT NULL,
    placement_path text,
    root_global_read_intent boolean NOT NULL,
    provenance_command_id uuid NOT NULL,
    recorded_at timestamp with time zone NOT NULL,
    CONSTRAINT session_placement_event_check CHECK ((((version = (1)::numeric) AND (prior_version IS NULL) AND (event_kind = 'created'::text)) OR ((version > (1)::numeric) AND (prior_version = (version - (1)::numeric)) AND (event_kind = 'updated'::text)))),
    CONSTRAINT session_placement_event_check1 CHECK ((root_global_read_intent = ((placement_path IS NOT NULL) AND (POSITION(('.'::text) IN (placement_path)) = 0)))),
    CONSTRAINT session_placement_event_event_kind_check CHECK ((event_kind = ANY (ARRAY['created'::text, 'updated'::text]))),
    CONSTRAINT session_placement_event_placement_path_check CHECK (((placement_path IS NULL) OR ((octet_length(placement_path) BETWEEN 1 AND 4159) AND (placement_path ~ '^[A-Za-z0-9_-]{1,64}(\.[A-Za-z0-9_-]{1,64}){0,63}$'::text)))),
    CONSTRAINT session_placement_event_prior_version_check CHECK (((prior_version >= (1)::numeric) AND (prior_version <= '18446744073709551615'::numeric))),
    CONSTRAINT session_placement_event_version_check CHECK (((version >= (1)::numeric) AND (version <= '18446744073709551615'::numeric)))
);


--
-- Name: update_session_placement_command; Type: TABLE; Schema: public
--

CREATE TABLE update_session_placement_command (
    command_id uuid NOT NULL,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    expected_version numeric(20,0) NOT NULL,
    replacement_path text,
    root_global_read_intent boolean CONSTRAINT update_session_placement_comma_root_global_read_intent_not_null NOT NULL,
    result_kind text NOT NULL,
    rejection_kind text,
    result_version numeric(20,0),
    result_current_version numeric(20,0),
    CONSTRAINT update_session_placement_command_check CHECK ((root_global_read_intent = ((replacement_path IS NOT NULL) AND (POSITION(('.'::text) IN (replacement_path)) = 0)))),
    CONSTRAINT update_session_placement_command_command_kind_check CHECK ((command_kind = 'update_session_placement'::text)),
    CONSTRAINT update_session_placement_command_expected_version_check CHECK (((expected_version >= (1)::numeric) AND (expected_version <= '18446744073709551615'::numeric))),
    CONSTRAINT update_session_placement_command_rejection_kind_check CHECK ((rejection_kind = ANY (ARRAY['session_not_found'::text, 'current_version_mismatch'::text, 'version_exhausted'::text]))),
    CONSTRAINT update_session_placement_command_replacement_path_check CHECK (((replacement_path IS NULL) OR ((octet_length(replacement_path) BETWEEN 1 AND 4159) AND (replacement_path ~ '^[A-Za-z0-9_-]{1,64}(\.[A-Za-z0-9_-]{1,64}){0,63}$'::text)))),
    CONSTRAINT update_session_placement_command_result_current_version_check CHECK (((result_current_version >= (1)::numeric) AND (result_current_version <= '18446744073709551615'::numeric))),
    CONSTRAINT update_session_placement_command_result_kind_check CHECK ((result_kind = ANY (ARRAY['applied'::text, 'rejected'::text]))),
    CONSTRAINT update_session_placement_command_result_shape CHECK ((((result_kind = 'applied'::text) AND (rejection_kind IS NULL) AND (result_version IS NOT NULL) AND (result_version = (expected_version + (1)::numeric)) AND (result_current_version IS NULL)) OR ((result_kind = 'rejected'::text) AND (rejection_kind = 'session_not_found'::text) AND (result_version IS NULL) AND (result_current_version IS NULL)) OR ((result_kind = 'rejected'::text) AND (rejection_kind = 'current_version_mismatch'::text) AND (result_version IS NULL) AND (result_current_version IS NOT NULL) AND (result_current_version <> expected_version)) OR ((result_kind = 'rejected'::text) AND (rejection_kind = 'version_exhausted'::text) AND (result_version IS NULL) AND (expected_version = '18446744073709551615'::numeric) AND (result_current_version = '18446744073709551615'::numeric)))),
    CONSTRAINT update_session_placement_command_result_version_check CHECK (((result_version >= (1)::numeric) AND (result_version <= '18446744073709551615'::numeric))),
    CONSTRAINT update_session_placement_command_storage_version_check CHECK ((storage_version = 1))
);


--
-- Constraints.
--

--
-- Name: runner_claimed_retry_attempt_authority runner_claimed_retry_attempt_authority_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_claimed_retry_attempt_authority
    ADD CONSTRAINT runner_claimed_retry_attempt_authority_pk PRIMARY KEY (source_lease_id, source_generation);


--
-- Name: runner_claimed_retry_attempt_authority runner_claimed_retry_attempt_authority_replacement_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_claimed_retry_attempt_authority
    ADD CONSTRAINT runner_claimed_retry_attempt_authority_replacement_key UNIQUE (replacement_attempt_id);


--
-- Name: runner_connection_authority_head runner_connection_authority_head_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_connection_authority_head
    ADD CONSTRAINT runner_connection_authority_head_pkey PRIMARY KEY (enrollment_id);


--
-- Name: runner_connection_event runner_connection_event_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_connection_event
    ADD CONSTRAINT runner_connection_event_pk PRIMARY KEY (enrollment_id, connection_epoch, event_ordinal);


--
-- Name: runner_connection_loss_epoch runner_connection_loss_epoch_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_connection_loss_epoch
    ADD CONSTRAINT runner_connection_loss_epoch_pk PRIMARY KEY (enrollment_id, loss_epoch);


--
-- Name: runner_connection_loss_epoch runner_connection_loss_epoch_source_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_connection_loss_epoch
    ADD CONSTRAINT runner_connection_loss_epoch_source_key UNIQUE (enrollment_id, connection_epoch);


--
-- Name: runner_connection_loss_propagation runner_connection_loss_propagation_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_connection_loss_propagation
    ADD CONSTRAINT runner_connection_loss_propagation_pk PRIMARY KEY (enrollment_id, loss_epoch);


--
-- Name: runner_credential_grant_audit runner_credential_grant_audit_head_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_credential_grant_audit
    ADD CONSTRAINT runner_credential_grant_audit_head_key UNIQUE (session_id, lineage_origin_event_ordinal, runner_id, grant_revision, audit_ordinal, event_kind);


--
-- Name: runner_credential_grant_audit runner_credential_grant_audit_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_credential_grant_audit
    ADD CONSTRAINT runner_credential_grant_audit_pk PRIMARY KEY (session_id, lineage_origin_event_ordinal, runner_id, grant_revision, audit_ordinal);


--
-- Name: runner_credential_grant runner_credential_grant_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_credential_grant
    ADD CONSTRAINT runner_credential_grant_pk PRIMARY KEY (session_id, lineage_origin_event_ordinal, runner_id, grant_revision);


--
-- Name: runner_credential_grant runner_credential_grant_profile_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_credential_grant
    ADD CONSTRAINT runner_credential_grant_profile_key UNIQUE (session_id, lineage_origin_event_ordinal, runner_id, grant_revision, credential_profile_name);


--
-- Name: runner_credential_grant_tool runner_credential_grant_tool_lease_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_credential_grant_tool
    ADD CONSTRAINT runner_credential_grant_tool_lease_key UNIQUE (session_id, lineage_origin_event_ordinal, runner_id, grant_revision, credential_profile_name, tool_name, approval_kind);


--
-- Name: runner_credential_grant_tool runner_credential_grant_tool_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_credential_grant_tool
    ADD CONSTRAINT runner_credential_grant_tool_pk PRIMARY KEY (session_id, lineage_origin_event_ordinal, runner_id, grant_revision, tool_name);


--
-- Name: runner_current_connection_loss runner_current_connection_loss_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_current_connection_loss
    ADD CONSTRAINT runner_current_connection_loss_pkey PRIMARY KEY (enrollment_id);


--
-- Name: runner_current_credential_grant_audit runner_current_credential_grant_audit_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_current_credential_grant_audit
    ADD CONSTRAINT runner_current_credential_grant_audit_pk PRIMARY KEY (session_id, lineage_origin_event_ordinal, runner_id, grant_revision);


--
-- Name: runner_current_lease_event runner_current_lease_event_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_current_lease_event
    ADD CONSTRAINT runner_current_lease_event_pk PRIMARY KEY (lease_id, generation);


--
-- Name: runner_current_registration runner_current_registration_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_current_registration
    ADD CONSTRAINT runner_current_registration_pkey PRIMARY KEY (enrollment_id);


--
-- Name: runner_current_session_placement runner_current_session_placement_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_current_session_placement
    ADD CONSTRAINT runner_current_session_placement_pkey PRIMARY KEY (session_id);


--
-- Name: runner_enrollment_allowed_class runner_enrollment_allowed_class_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_enrollment_allowed_class
    ADD CONSTRAINT runner_enrollment_allowed_class_pk PRIMARY KEY (enrollment_id, capability_class);


--
-- Name: runner_enrollment_audit_allowed_class runner_enrollment_audit_allowed_class_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_enrollment_audit_allowed_class
    ADD CONSTRAINT runner_enrollment_audit_allowed_class_pk PRIMARY KEY (enrollment_id, revision, capability_class);


--
-- Name: runner_enrollment_audit runner_enrollment_audit_identity_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_enrollment_audit
    ADD CONSTRAINT runner_enrollment_audit_identity_key UNIQUE (enrollment_id, revision, runner_id, authentication_reference_id, allowed_class_count);


--
-- Name: runner_enrollment_audit runner_enrollment_audit_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_enrollment_audit
    ADD CONSTRAINT runner_enrollment_audit_pk PRIMARY KEY (enrollment_id, revision);


--
-- Name: runner_enrollment runner_enrollment_authentication_reference_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_enrollment
    ADD CONSTRAINT runner_enrollment_authentication_reference_id_key UNIQUE (authentication_reference_id);


--
-- Name: runner_enrollment runner_enrollment_identity_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_enrollment
    ADD CONSTRAINT runner_enrollment_identity_key UNIQUE (enrollment_id, runner_id, authentication_reference_id);


--
-- Name: runner_enrollment runner_enrollment_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_enrollment
    ADD CONSTRAINT runner_enrollment_pkey PRIMARY KEY (enrollment_id);


--
-- Name: runner_enrollment_request_receipt runner_enrollment_request_recei_authentication_reference_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_enrollment_request_receipt
    ADD CONSTRAINT runner_enrollment_request_recei_authentication_reference_id_key UNIQUE (authentication_reference_id);


--
-- Name: runner_enrollment_request_receipt runner_enrollment_request_receipt_enrollment_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_enrollment_request_receipt
    ADD CONSTRAINT runner_enrollment_request_receipt_enrollment_id_key UNIQUE (enrollment_id);


--
-- Name: runner_enrollment_request_receipt runner_enrollment_request_receipt_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_enrollment_request_receipt
    ADD CONSTRAINT runner_enrollment_request_receipt_pkey PRIMARY KEY (request_id);


--
-- Name: runner_enrollment_request_receipt runner_enrollment_request_receipt_runner_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_enrollment_request_receipt
    ADD CONSTRAINT runner_enrollment_request_receipt_runner_id_key UNIQUE (runner_id);


--
-- Name: runner_enrollment runner_enrollment_runner_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_enrollment
    ADD CONSTRAINT runner_enrollment_runner_id_key UNIQUE (runner_id);


--
-- Name: runner_lease_event runner_lease_event_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_lease_event
    ADD CONSTRAINT runner_lease_event_pk PRIMARY KEY (lease_id, generation, event_ordinal);


--
-- Name: runner_lease_generation runner_lease_generation_correlation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_lease_generation
    ADD CONSTRAINT runner_lease_generation_correlation_key UNIQUE (lease_id, generation, attempt_id, session_id, runner_id, tool_name);


--
-- Name: runner_lease_generation runner_lease_generation_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_lease_generation
    ADD CONSTRAINT runner_lease_generation_pk PRIMARY KEY (lease_id, generation);


--
-- Name: runner_lease_no_execution_proof runner_lease_no_execution_proof_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_lease_no_execution_proof
    ADD CONSTRAINT runner_lease_no_execution_proof_pk PRIMARY KEY (lease_id, generation);


--
-- Name: runner_physical_attempt_lease_binding runner_physical_attempt_lease_binding_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_physical_attempt_lease_binding
    ADD CONSTRAINT runner_physical_attempt_lease_binding_pkey PRIMARY KEY (attempt_id);


--
-- Name: runner_registration_class runner_registration_class_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration_class
    ADD CONSTRAINT runner_registration_class_pk PRIMARY KEY (enrollment_id, registration_revision, capability_class);


--
-- Name: runner_registration runner_registration_identity_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration
    ADD CONSTRAINT runner_registration_identity_key UNIQUE (enrollment_id, registration_revision, runner_id, authentication_reference_id);


--
-- Name: runner_registration runner_registration_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration
    ADD CONSTRAINT runner_registration_pk PRIMARY KEY (enrollment_id, registration_revision);


--
-- Name: runner_registration_profile_approval runner_registration_profile_approval_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration_profile_approval
    ADD CONSTRAINT runner_registration_profile_approval_pk PRIMARY KEY (enrollment_id, registration_revision, credential_profile_name, tool_name);


--
-- Name: runner_registration_profile runner_registration_profile_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration_profile
    ADD CONSTRAINT runner_registration_profile_pk PRIMARY KEY (enrollment_id, registration_revision, credential_profile_name);


--
-- Name: runner_registration_repository runner_registration_repository_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration_repository
    ADD CONSTRAINT runner_registration_repository_pk PRIMARY KEY (enrollment_id, registration_revision, repository_key);


--
-- Name: runner_registration runner_registration_runner_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration
    ADD CONSTRAINT runner_registration_runner_key UNIQUE (enrollment_id, registration_revision, runner_id);


--
-- Name: runner_registration_sandbox runner_registration_sandbox_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration_sandbox
    ADD CONSTRAINT runner_registration_sandbox_pk PRIMARY KEY (enrollment_id, registration_revision, sandbox_profile);


--
-- Name: runner_registration_tool runner_registration_tool_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration_tool
    ADD CONSTRAINT runner_registration_tool_pk PRIMARY KEY (enrollment_id, registration_revision, tool_name);


--
-- Name: runner_registration_workspace runner_registration_workspace_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration_workspace
    ADD CONSTRAINT runner_registration_workspace_pk PRIMARY KEY (enrollment_id, registration_revision, workspace_kind);


--
-- Name: runner_session_placement_permission_override runner_session_placement_permission_override_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_session_placement_permission_override
    ADD CONSTRAINT runner_session_placement_permission_override_pk PRIMARY KEY (session_id, event_ordinal, tool_name);


--
-- Name: runner_session_placement_record runner_session_placement_record_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_session_placement_record
    ADD CONSTRAINT runner_session_placement_record_pk PRIMARY KEY (session_id, event_ordinal);


--
-- Name: runner_session_placement_record runner_session_placement_record_revision_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_session_placement_record
    ADD CONSTRAINT runner_session_placement_record_revision_key UNIQUE (session_id, event_ordinal, placement_revision);


--
-- Name: runner_session_placement_tool runner_session_placement_tool_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_session_placement_tool
    ADD CONSTRAINT runner_session_placement_tool_pk PRIMARY KEY (session_id, event_ordinal, tool_name);


--
-- Name: runner_state_transition_outbox_event runner_state_transition_outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_state_transition_outbox_event
    ADD CONSTRAINT runner_state_transition_outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: runner_state_transition_outbox_event runner_state_transition_outbox_source_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_state_transition_outbox_event
    ADD CONSTRAINT runner_state_transition_outbox_source_key UNIQUE NULLS NOT DISTINCT (session_id, placement_event_ordinal, connection_enrollment_id, connection_epoch, connection_event_ordinal);


--
-- Name: runner_tool_request_lease_binding runner_tool_request_lease_binding_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_tool_request_lease_binding
    ADD CONSTRAINT runner_tool_request_lease_binding_pkey PRIMARY KEY (request_id);


--
-- Name: session_current_placement session_current_placement_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_current_placement
    ADD CONSTRAINT session_current_placement_pkey PRIMARY KEY (session_id);


--
-- Name: session_placement_event session_placement_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_placement_event
    ADD CONSTRAINT session_placement_event_pkey PRIMARY KEY (session_id, version);


--
-- Name: session_placement_event session_placement_event_session_id_version_provenance_comma_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_placement_event
    ADD CONSTRAINT session_placement_event_session_id_version_provenance_comma_key UNIQUE (session_id, version, provenance_command_id);


--
-- Name: update_session_placement_command update_session_placement_command_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY update_session_placement_command
    ADD CONSTRAINT update_session_placement_command_pkey PRIMARY KEY (command_id);


--
-- Indexes.
--

--
-- Name: runner_session_placement_exact_loss_propagation_page; Type: INDEX; Schema: public
--

CREATE INDEX runner_session_placement_exact_loss_propagation_page ON runner_session_placement_record USING btree (selector_runner_id, session_id, event_ordinal) WHERE ((loss_fence_enrollment_id IS NULL) AND (state_kind = 'unpinned'::text) AND (selector_kind = 'identity'::text));


--
-- Name: runner_session_placement_loss_propagation_page; Type: INDEX; Schema: public
--

CREATE INDEX runner_session_placement_loss_propagation_page ON runner_session_placement_record USING btree (loss_fence_enrollment_id, session_id, event_ordinal);


--
-- Name: runner_session_placement_record_enrollment_pin_lookup; Type: INDEX; Schema: public
--

CREATE INDEX runner_session_placement_record_enrollment_pin_lookup ON runner_session_placement_record USING btree (registration_enrollment_id, session_id, event_ordinal) WHERE (state_kind = 'pinned'::text);


--
-- Triggers.
--

--
-- Name: create_session_from_imported_frontier_command legacy_imported_creation_materializes_placement; Type: TRIGGER; Schema: public
--

CREATE TRIGGER legacy_imported_creation_materializes_placement AFTER INSERT ON create_session_from_imported_frontier_command FOR EACH ROW EXECUTE FUNCTION materialize_legacy_creation_placement();


--
-- Name: create_session_command legacy_native_creation_materializes_placement; Type: TRIGGER; Schema: public
--

CREATE TRIGGER legacy_native_creation_materializes_placement AFTER INSERT ON create_session_command FOR EACH ROW EXECUTE FUNCTION materialize_legacy_creation_placement();


--
-- Name: runner_claimed_retry_attempt_authority runner_claimed_retry_attempt_authority_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_claimed_retry_attempt_authority_is_append_only BEFORE DELETE OR UPDATE ON runner_claimed_retry_attempt_authority FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_claimed_retry_attempt_authority runner_claimed_retry_attempt_authority_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_claimed_retry_attempt_authority_is_guarded BEFORE INSERT ON runner_claimed_retry_attempt_authority FOR EACH ROW EXECUTE FUNCTION guard_runner_claimed_retry_attempt_authority();


--
-- Name: runner_claimed_retry_attempt_authority runner_claimed_retry_attempt_authority_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_claimed_retry_attempt_authority_rejects_truncate BEFORE TRUNCATE ON runner_claimed_retry_attempt_authority FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_connection_authority_head runner_connection_authority_head_advances; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_connection_authority_head_advances BEFORE INSERT OR DELETE OR UPDATE ON runner_connection_authority_head FOR EACH ROW EXECUTE FUNCTION guard_runner_connection_authority_head();


--
-- Name: runner_connection_authority_head runner_connection_authority_head_is_complete; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_connection_authority_head_is_complete AFTER INSERT OR UPDATE ON runner_connection_authority_head DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_connection_authority_head_complete();


--
-- Name: runner_connection_authority_head runner_connection_authority_head_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_connection_authority_head_rejects_truncate BEFORE TRUNCATE ON runner_connection_authority_head FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_connection_event runner_connection_event_insert_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_connection_event_insert_is_guarded BEFORE INSERT ON runner_connection_event FOR EACH ROW EXECUTE FUNCTION guard_runner_connection_event_insert();


--
-- Name: runner_connection_event runner_connection_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_connection_event_is_append_only BEFORE DELETE OR UPDATE ON runner_connection_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_connection_event runner_connection_event_loss_is_complete; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_connection_event_loss_is_complete AFTER INSERT ON runner_connection_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_connection_loss_complete();


--
-- Name: runner_connection_event runner_connection_event_rechecks_authority_head; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_connection_event_rechecks_authority_head AFTER INSERT ON runner_connection_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_connection_authority_head_complete();


--
-- Name: runner_connection_event runner_connection_event_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_connection_event_rejects_truncate BEFORE TRUNCATE ON runner_connection_event FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_connection_loss_epoch runner_connection_loss_epoch_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_connection_loss_epoch_is_append_only BEFORE DELETE OR UPDATE ON runner_connection_loss_epoch FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_connection_loss_epoch runner_connection_loss_epoch_is_complete; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_connection_loss_epoch_is_complete AFTER INSERT ON runner_connection_loss_epoch DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_connection_loss_complete();


--
-- Name: runner_connection_loss_epoch runner_connection_loss_epoch_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_connection_loss_epoch_is_guarded BEFORE INSERT ON runner_connection_loss_epoch FOR EACH ROW EXECUTE FUNCTION guard_runner_connection_loss_epoch();


--
-- Name: runner_connection_loss_epoch runner_connection_loss_epoch_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_connection_loss_epoch_rejects_truncate BEFORE TRUNCATE ON runner_connection_loss_epoch FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_connection_loss_epoch runner_connection_loss_has_propagation; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_connection_loss_has_propagation AFTER INSERT ON runner_connection_loss_epoch DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_connection_loss_has_propagation();


--
-- Name: runner_connection_loss_propagation runner_connection_loss_propagation_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_connection_loss_propagation_is_guarded BEFORE INSERT OR DELETE OR UPDATE ON runner_connection_loss_propagation FOR EACH ROW EXECUTE FUNCTION guard_runner_connection_loss_propagation();


--
-- Name: runner_connection_loss_propagation runner_connection_loss_propagation_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_connection_loss_propagation_rejects_truncate BEFORE TRUNCATE ON runner_connection_loss_propagation FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_connection_loss_epoch runner_connection_loss_rechecks_authority_head; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_connection_loss_rechecks_authority_head AFTER INSERT ON runner_connection_loss_epoch DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_connection_authority_head_complete();


--
-- Name: runner_credential_grant_audit runner_credential_grant_audit_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_credential_grant_audit_is_append_only BEFORE DELETE OR UPDATE ON runner_credential_grant_audit FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_credential_grant_audit runner_credential_grant_audit_rechecks_evidence; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_credential_grant_audit_rechecks_evidence AFTER INSERT OR DELETE OR UPDATE ON runner_credential_grant_audit DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_grant_complete();


--
-- Name: runner_credential_grant_audit runner_credential_grant_audit_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_credential_grant_audit_rejects_truncate BEFORE TRUNCATE ON runner_credential_grant_audit FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_credential_grant runner_credential_grant_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_credential_grant_is_append_only BEFORE DELETE OR UPDATE ON runner_credential_grant FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_credential_grant runner_credential_grant_requires_complete_evidence; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_credential_grant_requires_complete_evidence AFTER INSERT OR DELETE OR UPDATE ON runner_credential_grant DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_grant_complete();


--
-- Name: runner_credential_grant_tool runner_credential_grant_tool_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_credential_grant_tool_is_append_only BEFORE DELETE OR UPDATE ON runner_credential_grant_tool FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_credential_grant_tool runner_credential_grant_tool_rechecks_evidence; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_credential_grant_tool_rechecks_evidence AFTER INSERT OR DELETE OR UPDATE ON runner_credential_grant_tool DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_grant_complete();


--
-- Name: runner_current_connection_loss runner_current_connection_loss_advances; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_current_connection_loss_advances BEFORE INSERT OR DELETE OR UPDATE ON runner_current_connection_loss FOR EACH ROW EXECUTE FUNCTION guard_runner_current_connection_loss();


--
-- Name: runner_current_connection_loss runner_current_connection_loss_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_current_connection_loss_rejects_truncate BEFORE TRUNCATE ON runner_current_connection_loss FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_current_credential_grant_audit runner_current_credential_grant_audit_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_current_credential_grant_audit_rejects_truncate BEFORE TRUNCATE ON runner_current_credential_grant_audit FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_current_credential_grant_audit runner_current_grant_audit_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_current_grant_audit_is_guarded BEFORE INSERT OR DELETE OR UPDATE ON runner_current_credential_grant_audit FOR EACH ROW EXECUTE FUNCTION guard_runner_current_grant_audit();


--
-- Name: runner_current_lease_event runner_current_lease_event_advances; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_current_lease_event_advances BEFORE INSERT OR DELETE OR UPDATE ON runner_current_lease_event FOR EACH ROW EXECUTE FUNCTION guard_runner_current_lease_event();


--
-- Name: runner_current_lease_event runner_current_lease_event_rechecks_generation; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_current_lease_event_rechecks_generation AFTER INSERT OR DELETE OR UPDATE ON runner_current_lease_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_lease_generation_complete();


--
-- Name: runner_current_lease_event runner_current_lease_event_rechecks_turn_recovery; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_current_lease_event_rechecks_turn_recovery AFTER INSERT OR DELETE OR UPDATE ON runner_current_lease_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION recheck_session_turn_runner_recovery();


--
-- Name: runner_current_lease_event runner_current_lease_event_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_current_lease_event_rejects_truncate BEFORE TRUNCATE ON runner_current_lease_event FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_current_registration runner_current_registration_advances; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_current_registration_advances BEFORE INSERT OR DELETE OR UPDATE ON runner_current_registration FOR EACH ROW EXECUTE FUNCTION guard_runner_current_registration();


--
-- Name: runner_current_registration runner_current_registration_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_current_registration_rejects_truncate BEFORE TRUNCATE ON runner_current_registration FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_current_session_placement runner_current_session_placement_advances; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_current_session_placement_advances BEFORE INSERT OR DELETE OR UPDATE ON runner_current_session_placement FOR EACH ROW EXECUTE FUNCTION guard_runner_current_placement();


--
-- Name: runner_current_session_placement runner_current_session_placement_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_current_session_placement_rejects_truncate BEFORE TRUNCATE ON runner_current_session_placement FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_enrollment runner_enrollment_00_serializes_loss_identity; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_enrollment_00_serializes_loss_identity BEFORE INSERT ON runner_enrollment FOR EACH ROW EXECUTE FUNCTION serialize_runner_enrollment_loss_identity();


--
-- Name: runner_enrollment_allowed_class runner_enrollment_allowed_class_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_enrollment_allowed_class_is_append_only BEFORE DELETE OR UPDATE ON runner_enrollment_allowed_class FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_enrollment_allowed_class runner_enrollment_allowed_class_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_enrollment_allowed_class_rejects_truncate BEFORE TRUNCATE ON runner_enrollment_allowed_class FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_enrollment_audit_allowed_class runner_enrollment_audit_allowed_class_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_enrollment_audit_allowed_class_is_append_only BEFORE DELETE OR UPDATE ON runner_enrollment_audit_allowed_class FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_enrollment_audit_allowed_class runner_enrollment_audit_allowed_class_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_enrollment_audit_allowed_class_rejects_truncate BEFORE TRUNCATE ON runner_enrollment_audit_allowed_class FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_enrollment_audit_allowed_class runner_enrollment_audit_class_rechecks_inventory; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_enrollment_audit_class_rechecks_inventory AFTER INSERT OR DELETE OR UPDATE ON runner_enrollment_audit_allowed_class DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_enrollment_audit_complete();


--
-- Name: runner_enrollment_audit runner_enrollment_audit_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_enrollment_audit_is_append_only BEFORE DELETE OR UPDATE ON runner_enrollment_audit FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_enrollment_audit runner_enrollment_audit_requires_complete_classes; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_enrollment_audit_requires_complete_classes AFTER INSERT OR DELETE OR UPDATE ON runner_enrollment_audit DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_enrollment_audit_complete();


--
-- Name: runner_enrollment_audit runner_enrollment_audit_requires_installation; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_enrollment_audit_requires_installation AFTER INSERT OR DELETE OR UPDATE ON runner_enrollment_audit DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_enrollment_audit_installed();


--
-- Name: runner_enrollment runner_enrollment_changes_are_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_enrollment_changes_are_guarded BEFORE INSERT OR DELETE OR UPDATE ON runner_enrollment FOR EACH ROW EXECUTE FUNCTION guard_runner_enrollment_change();


--
-- Name: runner_enrollment_allowed_class runner_enrollment_class_rechecks_inventory; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_enrollment_class_rechecks_inventory AFTER INSERT OR DELETE OR UPDATE ON runner_enrollment_allowed_class DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_enrollment_complete();


--
-- Name: runner_enrollment_request_receipt runner_enrollment_request_receipt_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_enrollment_request_receipt_is_append_only BEFORE DELETE OR UPDATE ON runner_enrollment_request_receipt FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_enrollment_request_receipt runner_enrollment_request_receipt_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_enrollment_request_receipt_rejects_truncate BEFORE TRUNCATE ON runner_enrollment_request_receipt FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_enrollment runner_enrollment_requires_complete_classes; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_enrollment_requires_complete_classes AFTER INSERT OR DELETE OR UPDATE ON runner_enrollment DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_enrollment_complete();


--
-- Name: runner_credential_grant_audit runner_grant_audit_advances_current_head; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_grant_audit_advances_current_head AFTER INSERT ON runner_credential_grant_audit FOR EACH ROW EXECUTE FUNCTION advance_runner_current_grant_audit();


--
-- Name: runner_session_placement_record runner_initial_pin_requires_lease; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_initial_pin_requires_lease AFTER INSERT ON runner_session_placement_record DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_initial_pin_has_lease();


--
-- Name: runner_lease_event runner_lease_claim_connection_loss_fence; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_lease_claim_connection_loss_fence BEFORE INSERT ON runner_lease_event FOR EACH ROW EXECUTE FUNCTION reject_runner_lease_claim_after_connection_loss();


--
-- Name: runner_lease_event runner_lease_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_lease_event_is_append_only BEFORE DELETE OR UPDATE ON runner_lease_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_lease_event runner_lease_event_rechecks_generation; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_lease_event_rechecks_generation AFTER INSERT OR DELETE OR UPDATE ON runner_lease_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_lease_generation_complete();


--
-- Name: runner_lease_event runner_lease_event_rechecks_turn_recovery; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_lease_event_rechecks_turn_recovery AFTER INSERT OR DELETE OR UPDATE ON runner_lease_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION recheck_session_turn_runner_recovery();


--
-- Name: runner_lease_event runner_lease_event_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_lease_event_rejects_truncate BEFORE TRUNCATE ON runner_lease_event FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_lease_event runner_lease_events_are_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_lease_events_are_guarded BEFORE INSERT ON runner_lease_event FOR EACH ROW EXECUTE FUNCTION guard_runner_lease_event();


--
-- Name: runner_lease_generation runner_lease_generation_connection_loss_fence; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_lease_generation_connection_loss_fence BEFORE INSERT ON runner_lease_generation FOR EACH ROW EXECUTE FUNCTION reject_runner_lease_generation_after_connection_loss();


--
-- Name: runner_lease_generation runner_lease_generation_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_lease_generation_is_append_only BEFORE DELETE OR UPDATE ON runner_lease_generation FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_lease_generation runner_lease_generation_requires_events; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_lease_generation_requires_events AFTER INSERT OR DELETE OR UPDATE ON runner_lease_generation DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_lease_generation_complete();


--
-- Name: runner_lease_generation runner_lease_generation_wire_approval_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_lease_generation_wire_approval_is_guarded BEFORE INSERT ON runner_lease_generation FOR EACH ROW EXECUTE FUNCTION guard_runner_wire_lease_approval();


--
-- Name: runner_lease_generation runner_lease_generations_are_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_lease_generations_are_guarded BEFORE INSERT ON runner_lease_generation FOR EACH ROW EXECUTE FUNCTION guard_runner_lease_generation();


--
-- Name: runner_lease_no_execution_proof runner_lease_no_execution_proof_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_lease_no_execution_proof_is_append_only BEFORE DELETE OR UPDATE ON runner_lease_no_execution_proof FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_lease_no_execution_proof runner_lease_no_execution_proof_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_lease_no_execution_proof_rejects_truncate BEFORE TRUNCATE ON runner_lease_no_execution_proof FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_current_lease_event runner_loss_head_rechecks_no_execution_proof; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_loss_head_rechecks_no_execution_proof AFTER INSERT OR DELETE OR UPDATE ON runner_current_lease_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_no_execution_proof_complete();


--
-- Name: runner_lease_event runner_loss_rechecks_no_execution_proof; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_loss_rechecks_no_execution_proof AFTER INSERT OR DELETE OR UPDATE ON runner_lease_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_no_execution_proof_complete();


--
-- Name: runner_lease_no_execution_proof runner_no_execution_proof_requires_loss; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_no_execution_proof_requires_loss AFTER INSERT OR DELETE OR UPDATE ON runner_lease_no_execution_proof DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_no_execution_proof_complete();


--
-- Name: runner_physical_attempt_lease_binding runner_physical_attempt_lease_binding_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_physical_attempt_lease_binding_is_append_only BEFORE DELETE OR UPDATE ON runner_physical_attempt_lease_binding FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_physical_attempt_lease_binding runner_physical_attempt_lease_binding_requires_lineage; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_physical_attempt_lease_binding_requires_lineage AFTER INSERT OR DELETE OR UPDATE ON runner_physical_attempt_lease_binding DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_physical_attempt_lease_binding_complete();


--
-- Name: runner_session_placement_record runner_placement_interrupted_attempt_is_complete; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_placement_interrupted_attempt_is_complete AFTER INSERT ON runner_session_placement_record DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_placement_interrupted_attempt_complete();


--
-- Name: runner_current_session_placement runner_placement_rechecks_turn_recovery; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_placement_rechecks_turn_recovery AFTER INSERT OR DELETE OR UPDATE ON runner_current_session_placement DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION recheck_session_turn_runner_recovery();


--
-- Name: runner_session_placement_record runner_profileless_grant_is_terminal; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_profileless_grant_is_terminal AFTER INSERT ON runner_session_placement_record DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_profileless_grant_tombstone();


--
-- Name: runner_registration_class runner_registration_class_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_registration_class_is_append_only BEFORE DELETE OR UPDATE ON runner_registration_class FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_registration_class runner_registration_class_rechecks_inventory; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_registration_class_rechecks_inventory AFTER INSERT OR DELETE OR UPDATE ON runner_registration_class DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_registration_complete();


--
-- Name: runner_registration_class runner_registration_class_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_registration_class_rejects_truncate BEFORE TRUNCATE ON runner_registration_class FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_registration runner_registration_insert_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_registration_insert_is_guarded BEFORE INSERT ON runner_registration FOR EACH ROW EXECUTE FUNCTION guard_runner_registration_insert();


--
-- Name: runner_registration runner_registration_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_registration_is_append_only BEFORE DELETE OR UPDATE ON runner_registration FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_registration_profile_approval runner_registration_profile_approval_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_registration_profile_approval_is_append_only BEFORE DELETE OR UPDATE ON runner_registration_profile_approval FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_registration_profile_approval runner_registration_profile_approval_rechecks_inventory; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_registration_profile_approval_rechecks_inventory AFTER INSERT OR DELETE OR UPDATE ON runner_registration_profile_approval DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_registration_complete();


--
-- Name: runner_registration_profile_approval runner_registration_profile_approval_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_registration_profile_approval_rejects_truncate BEFORE TRUNCATE ON runner_registration_profile_approval FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_registration_profile runner_registration_profile_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_registration_profile_is_append_only BEFORE DELETE OR UPDATE ON runner_registration_profile FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_registration_profile runner_registration_profile_rechecks_inventory; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_registration_profile_rechecks_inventory AFTER INSERT OR DELETE OR UPDATE ON runner_registration_profile DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_registration_complete();


--
-- Name: runner_registration_profile runner_registration_profile_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_registration_profile_rejects_truncate BEFORE TRUNCATE ON runner_registration_profile FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_registration runner_registration_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_registration_rejects_truncate BEFORE TRUNCATE ON runner_registration FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_registration_repository runner_registration_repository_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_registration_repository_is_append_only BEFORE DELETE OR UPDATE ON runner_registration_repository FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_registration_repository runner_registration_repository_rechecks_inventory; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_registration_repository_rechecks_inventory AFTER INSERT OR DELETE OR UPDATE ON runner_registration_repository DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_wire_registration_complete();


--
-- Name: runner_registration_repository runner_registration_repository_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_registration_repository_rejects_truncate BEFORE TRUNCATE ON runner_registration_repository FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_registration runner_registration_requires_complete_inventory; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_registration_requires_complete_inventory AFTER INSERT OR DELETE OR UPDATE ON runner_registration DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_registration_complete();


--
-- Name: runner_registration runner_registration_requires_wire_inventory; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_registration_requires_wire_inventory AFTER INSERT OR DELETE OR UPDATE ON runner_registration DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_wire_registration_complete();


--
-- Name: runner_registration_sandbox runner_registration_sandbox_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_registration_sandbox_is_append_only BEFORE DELETE OR UPDATE ON runner_registration_sandbox FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_registration_sandbox runner_registration_sandbox_rechecks_inventory; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_registration_sandbox_rechecks_inventory AFTER INSERT OR DELETE OR UPDATE ON runner_registration_sandbox DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_wire_registration_complete();


--
-- Name: runner_registration_sandbox runner_registration_sandbox_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_registration_sandbox_rejects_truncate BEFORE TRUNCATE ON runner_registration_sandbox FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_registration_tool runner_registration_tool_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_registration_tool_is_append_only BEFORE DELETE OR UPDATE ON runner_registration_tool FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_registration_tool runner_registration_tool_rechecks_inventory; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_registration_tool_rechecks_inventory AFTER INSERT OR DELETE OR UPDATE ON runner_registration_tool DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_registration_complete();


--
-- Name: runner_registration_tool runner_registration_tool_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_registration_tool_rejects_truncate BEFORE TRUNCATE ON runner_registration_tool FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_registration_workspace runner_registration_workspace_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_registration_workspace_is_append_only BEFORE DELETE OR UPDATE ON runner_registration_workspace FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_registration_workspace runner_registration_workspace_rechecks_inventory; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_registration_workspace_rechecks_inventory AFTER INSERT OR DELETE OR UPDATE ON runner_registration_workspace DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_registration_complete();


--
-- Name: runner_registration_workspace runner_registration_workspace_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_registration_workspace_rejects_truncate BEFORE TRUNCATE ON runner_registration_workspace FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_lease_event runner_retryable_loss_requires_live_attempt; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_retryable_loss_requires_live_attempt AFTER INSERT ON runner_lease_event FOR EACH ROW EXECUTE FUNCTION require_runner_retryable_loss_live_attempt();


--
-- Name: runner_session_placement_record runner_session_placement_00_serializes_loss_identity; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_session_placement_00_serializes_loss_identity BEFORE INSERT ON runner_session_placement_record FOR EACH ROW EXECUTE FUNCTION serialize_runner_placement_loss_identity();


--
-- Name: runner_session_placement_record runner_session_placement_01_sets_loss_baseline; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_session_placement_01_sets_loss_baseline BEFORE INSERT ON runner_session_placement_record FOR EACH ROW EXECUTE FUNCTION set_runner_placement_loss_baseline();


--
-- Name: runner_session_placement_permission_override runner_session_placement_permission_override_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_session_placement_permission_override_is_append_only BEFORE DELETE OR UPDATE ON runner_session_placement_permission_override FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_session_placement_permission_override runner_session_placement_permission_override_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_session_placement_permission_override_rejects_truncate BEFORE TRUNCATE ON runner_session_placement_permission_override FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_session_placement_permission_override runner_session_placement_permission_rechecks_inventory; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_session_placement_permission_rechecks_inventory AFTER INSERT OR DELETE OR UPDATE ON runner_session_placement_permission_override DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_wire_placement_complete();


--
-- Name: runner_session_placement_record runner_session_placement_record_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_session_placement_record_is_append_only BEFORE DELETE OR UPDATE ON runner_session_placement_record FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_session_placement_record runner_session_placement_records_are_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_session_placement_records_are_guarded BEFORE INSERT ON runner_session_placement_record FOR EACH ROW EXECUTE FUNCTION guard_runner_placement_record();


--
-- Name: runner_session_placement_record runner_session_placement_requires_permission_overrides; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_session_placement_requires_permission_overrides AFTER INSERT OR DELETE OR UPDATE ON runner_session_placement_record DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_wire_placement_complete();


--
-- Name: runner_session_placement_record runner_session_placement_requires_tools; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_session_placement_requires_tools AFTER INSERT OR DELETE OR UPDATE ON runner_session_placement_record DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_placement_complete();


--
-- Name: runner_session_placement_tool runner_session_placement_tool_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_session_placement_tool_is_append_only BEFORE DELETE OR UPDATE ON runner_session_placement_tool FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_session_placement_tool runner_session_placement_tool_rechecks_inventory; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_session_placement_tool_rechecks_inventory AFTER INSERT OR DELETE OR UPDATE ON runner_session_placement_tool DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_placement_complete();


--
-- Name: runner_state_transition_outbox_event runner_state_transition_outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_state_transition_outbox_event_cannot_be_truncated BEFORE TRUNCATE ON runner_state_transition_outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: runner_state_transition_outbox_event runner_state_transition_outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_state_transition_outbox_event_is_append_only BEFORE DELETE OR UPDATE ON runner_state_transition_outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_state_transition_outbox_event runner_state_transition_outbox_event_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_state_transition_outbox_event_is_guarded BEFORE INSERT ON runner_state_transition_outbox_event FOR EACH ROW EXECUTE FUNCTION guard_runner_state_transition_outbox_event();


--
-- Name: runner_tool_request_lease_binding runner_tool_request_lease_binding_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_tool_request_lease_binding_is_append_only BEFORE DELETE OR UPDATE ON runner_tool_request_lease_binding FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_tool_request_lease_binding runner_tool_request_lease_binding_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_tool_request_lease_binding_rejects_truncate BEFORE TRUNCATE ON runner_tool_request_lease_binding FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: runner_tool_request_lease_binding runner_tool_request_lease_binding_requires_lineage; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER runner_tool_request_lease_binding_requires_lineage AFTER INSERT OR DELETE OR UPDATE ON runner_tool_request_lease_binding DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_tool_request_lease_binding_complete();


--
-- Name: runner_session_placement_record runner_wire_placement_records_are_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER runner_wire_placement_records_are_guarded BEFORE INSERT ON runner_session_placement_record FOR EACH ROW EXECUTE FUNCTION guard_runner_wire_placement_record();


--
-- Name: session_placement_event session_placement_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_placement_event_is_append_only BEFORE DELETE OR UPDATE ON session_placement_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_placement_event session_placement_event_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_placement_event_reject_truncate BEFORE TRUNCATE ON session_placement_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: session_current_placement session_placement_head_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_placement_head_is_guarded BEFORE INSERT OR DELETE OR UPDATE ON session_current_placement FOR EACH ROW EXECUTE FUNCTION guard_session_placement_head();


--
-- Name: session_current_placement session_placement_head_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_placement_head_reject_truncate BEFORE TRUNCATE ON session_current_placement FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: session session_requires_placement; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_requires_placement AFTER INSERT ON session DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_session_placement();


--
-- Name: tool_attempt tool_attempt_replacement_commits_with_successor_lease; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER tool_attempt_replacement_commits_with_successor_lease AFTER INSERT ON tool_attempt DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_runner_retry_replacement_successor_lease();


--
-- Name: tool_attempt tool_attempt_runner_retry_is_authorized; Type: TRIGGER; Schema: public
--

CREATE TRIGGER tool_attempt_runner_retry_is_authorized BEFORE INSERT ON tool_attempt FOR EACH ROW EXECUTE FUNCTION require_runner_retry_attempt_authority();


--
-- Name: tool_round tool_round_00_locks_scheduler_before_insert; Type: TRIGGER; Schema: public
--

CREATE TRIGGER tool_round_00_locks_scheduler_before_insert BEFORE INSERT ON tool_round FOR EACH ROW EXECUTE FUNCTION lock_scheduler_before_runner_recovery_dependency_insert();


--
-- Name: turn_attempt turn_attempt_00_locks_scheduler_before_insert; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_attempt_00_locks_scheduler_before_insert BEFORE INSERT ON turn_attempt FOR EACH ROW EXECUTE FUNCTION lock_scheduler_before_runner_recovery_dependency_insert();


--
-- Name: turn_lifecycle turn_lifecycle_runner_recovery_does_not_reopen; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_lifecycle_runner_recovery_does_not_reopen BEFORE UPDATE ON turn_lifecycle FOR EACH ROW EXECUTE FUNCTION reject_runner_recovery_reopen();


--
-- Name: update_session_placement_command update_session_placement_command_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER update_session_placement_command_is_append_only BEFORE DELETE OR UPDATE ON update_session_placement_command FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: update_session_placement_command update_session_placement_command_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER update_session_placement_command_reject_truncate BEFORE TRUNCATE ON update_session_placement_command FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Foreign keys.
--

--
-- Name: runner_claimed_retry_attempt_authority runner_claimed_retry_attempt_authority_request_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_claimed_retry_attempt_authority
    ADD CONSTRAINT runner_claimed_retry_attempt_authority_request_fk FOREIGN KEY (replacement_request_id, replacement_turn_id, replacement_session_id) REFERENCES tool_request(request_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_claimed_retry_attempt_authority runner_claimed_retry_attempt_authority_source_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_claimed_retry_attempt_authority
    ADD CONSTRAINT runner_claimed_retry_attempt_authority_source_fk FOREIGN KEY (source_lease_id, source_generation) REFERENCES runner_lease_generation(lease_id, generation) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_connection_authority_head runner_connection_authority_head_connection_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_connection_authority_head
    ADD CONSTRAINT runner_connection_authority_head_connection_fk FOREIGN KEY (enrollment_id, connection_epoch, connection_event_ordinal) REFERENCES runner_connection_event(enrollment_id, connection_epoch, event_ordinal) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_connection_authority_head runner_connection_authority_head_loss_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_connection_authority_head
    ADD CONSTRAINT runner_connection_authority_head_loss_fk FOREIGN KEY (enrollment_id, latest_loss_epoch) REFERENCES runner_connection_loss_epoch(enrollment_id, loss_epoch) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_connection_event runner_connection_event_enrollment_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_connection_event
    ADD CONSTRAINT runner_connection_event_enrollment_fk FOREIGN KEY (enrollment_id) REFERENCES runner_enrollment(enrollment_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_connection_loss_epoch runner_connection_loss_epoch_source_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_connection_loss_epoch
    ADD CONSTRAINT runner_connection_loss_epoch_source_fk FOREIGN KEY (enrollment_id, connection_epoch, connection_event_ordinal) REFERENCES runner_connection_event(enrollment_id, connection_epoch, event_ordinal) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_connection_loss_propagation runner_connection_loss_propagation_loss_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_connection_loss_propagation
    ADD CONSTRAINT runner_connection_loss_propagation_loss_fk FOREIGN KEY (enrollment_id, loss_epoch) REFERENCES runner_connection_loss_epoch(enrollment_id, loss_epoch) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_connection_loss_propagation runner_connection_loss_propagation_session_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_connection_loss_propagation
    ADD CONSTRAINT runner_connection_loss_propagation_session_fk FOREIGN KEY (propagated_through_session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_credential_grant_audit runner_credential_grant_audit_grant_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_credential_grant_audit
    ADD CONSTRAINT runner_credential_grant_audit_grant_fk FOREIGN KEY (session_id, lineage_origin_event_ordinal, runner_id, grant_revision, credential_profile_name) REFERENCES runner_credential_grant(session_id, lineage_origin_event_ordinal, runner_id, grant_revision, credential_profile_name) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_credential_grant runner_credential_grant_placement_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_credential_grant
    ADD CONSTRAINT runner_credential_grant_placement_fk FOREIGN KEY (session_id, placement_event_ordinal) REFERENCES runner_session_placement_record(session_id, event_ordinal) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_credential_grant runner_credential_grant_prior_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_credential_grant
    ADD CONSTRAINT runner_credential_grant_prior_fk FOREIGN KEY (session_id, lineage_origin_event_ordinal, prior_runner_id, prior_grant_revision) REFERENCES runner_credential_grant(session_id, lineage_origin_event_ordinal, runner_id, grant_revision) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_credential_grant runner_credential_grant_registration_profile_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_credential_grant
    ADD CONSTRAINT runner_credential_grant_registration_profile_fk FOREIGN KEY (registration_enrollment_id, registration_revision, credential_profile_name) REFERENCES runner_registration_profile(enrollment_id, registration_revision, credential_profile_name) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_credential_grant_tool runner_credential_grant_tool_grant_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_credential_grant_tool
    ADD CONSTRAINT runner_credential_grant_tool_grant_fk FOREIGN KEY (session_id, lineage_origin_event_ordinal, runner_id, grant_revision, credential_profile_name) REFERENCES runner_credential_grant(session_id, lineage_origin_event_ordinal, runner_id, grant_revision, credential_profile_name) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_current_connection_loss runner_current_connection_loss_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_current_connection_loss
    ADD CONSTRAINT runner_current_connection_loss_fk FOREIGN KEY (enrollment_id, loss_epoch) REFERENCES runner_connection_loss_epoch(enrollment_id, loss_epoch) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_current_credential_grant_audit runner_current_credential_grant_audit_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_current_credential_grant_audit
    ADD CONSTRAINT runner_current_credential_grant_audit_fk FOREIGN KEY (session_id, lineage_origin_event_ordinal, runner_id, grant_revision, audit_ordinal, event_kind) REFERENCES runner_credential_grant_audit(session_id, lineage_origin_event_ordinal, runner_id, grant_revision, audit_ordinal, event_kind) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_current_lease_event runner_current_lease_event_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_current_lease_event
    ADD CONSTRAINT runner_current_lease_event_fk FOREIGN KEY (lease_id, generation, event_ordinal) REFERENCES runner_lease_event(lease_id, generation, event_ordinal) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_current_registration runner_current_registration_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_current_registration
    ADD CONSTRAINT runner_current_registration_fk FOREIGN KEY (enrollment_id, registration_revision) REFERENCES runner_registration(enrollment_id, registration_revision) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_current_session_placement runner_current_session_placement_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_current_session_placement
    ADD CONSTRAINT runner_current_session_placement_fk FOREIGN KEY (session_id, event_ordinal) REFERENCES runner_session_placement_record(session_id, event_ordinal) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_enrollment_allowed_class runner_enrollment_allowed_class_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_enrollment_allowed_class
    ADD CONSTRAINT runner_enrollment_allowed_class_fk FOREIGN KEY (enrollment_id) REFERENCES runner_enrollment(enrollment_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_enrollment_audit_allowed_class runner_enrollment_audit_allowed_class_audit_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_enrollment_audit_allowed_class
    ADD CONSTRAINT runner_enrollment_audit_allowed_class_audit_fk FOREIGN KEY (enrollment_id, revision) REFERENCES runner_enrollment_audit(enrollment_id, revision) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_enrollment runner_enrollment_audit_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_enrollment
    ADD CONSTRAINT runner_enrollment_audit_fk FOREIGN KEY (enrollment_id, revision, runner_id, authentication_reference_id, allowed_class_count) REFERENCES runner_enrollment_audit(enrollment_id, revision, runner_id, authentication_reference_id, allowed_class_count) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_enrollment_request_receipt runner_enrollment_request_receipt_registration_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_enrollment_request_receipt
    ADD CONSTRAINT runner_enrollment_request_receipt_registration_fk FOREIGN KEY (enrollment_id, registration_revision, runner_id, authentication_reference_id) REFERENCES runner_registration(enrollment_id, registration_revision, runner_id, authentication_reference_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_lease_generation runner_lease_attempt_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_lease_generation
    ADD CONSTRAINT runner_lease_attempt_fk FOREIGN KEY (attempt_id, session_id) REFERENCES tool_attempt(attempt_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_lease_event runner_lease_event_generation_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_lease_event
    ADD CONSTRAINT runner_lease_event_generation_fk FOREIGN KEY (lease_id, generation) REFERENCES runner_lease_generation(lease_id, generation) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_lease_generation runner_lease_grant_tool_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_lease_generation
    ADD CONSTRAINT runner_lease_grant_tool_fk FOREIGN KEY (session_id, credential_grant_lineage_origin_ordinal, runner_id, credential_grant_revision, credential_profile_name, tool_name, credential_approval_kind) REFERENCES runner_credential_grant_tool(session_id, lineage_origin_event_ordinal, runner_id, grant_revision, credential_profile_name, tool_name, approval_kind) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_lease_no_execution_proof runner_lease_no_execution_proof_attempt_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_lease_no_execution_proof
    ADD CONSTRAINT runner_lease_no_execution_proof_attempt_fk FOREIGN KEY (attempt_id, request_id, issuing_turn_attempt_id, dispatch_generation) REFERENCES tool_attempt(attempt_id, request_id, issuing_turn_attempt_id, dispatch_generation) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_lease_no_execution_proof runner_lease_no_execution_proof_generation_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_lease_no_execution_proof
    ADD CONSTRAINT runner_lease_no_execution_proof_generation_fk FOREIGN KEY (lease_id, generation, attempt_id, session_id, runner_id, tool_name) REFERENCES runner_lease_generation(lease_id, generation, attempt_id, session_id, runner_id, tool_name) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_lease_no_execution_proof runner_lease_no_execution_proof_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_lease_no_execution_proof
    ADD CONSTRAINT runner_lease_no_execution_proof_turn_fk FOREIGN KEY (attempt_id, turn_id, session_id) REFERENCES tool_attempt(attempt_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_lease_generation runner_lease_offer_connection_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_lease_generation
    ADD CONSTRAINT runner_lease_offer_connection_fk FOREIGN KEY (registration_enrollment_id, offer_connection_epoch, offer_connection_event_ordinal) REFERENCES runner_connection_event(enrollment_id, connection_epoch, event_ordinal) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_lease_generation runner_lease_offer_loss_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_lease_generation
    ADD CONSTRAINT runner_lease_offer_loss_fk FOREIGN KEY (registration_enrollment_id, offer_loss_epoch) REFERENCES runner_connection_loss_epoch(enrollment_id, loss_epoch) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_lease_generation runner_lease_placement_tool_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_lease_generation
    ADD CONSTRAINT runner_lease_placement_tool_fk FOREIGN KEY (session_id, placement_event_ordinal, tool_name) REFERENCES runner_session_placement_tool(session_id, event_ordinal, tool_name) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_lease_generation runner_lease_registration_tool_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_lease_generation
    ADD CONSTRAINT runner_lease_registration_tool_fk FOREIGN KEY (registration_enrollment_id, registration_revision, tool_name) REFERENCES runner_registration_tool(enrollment_id, registration_revision, tool_name) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_physical_attempt_lease_binding runner_physical_attempt_lease_binding_attempt_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_physical_attempt_lease_binding
    ADD CONSTRAINT runner_physical_attempt_lease_binding_attempt_fk FOREIGN KEY (attempt_id) REFERENCES tool_attempt(attempt_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_registration_class runner_registration_class_enrollment_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration_class
    ADD CONSTRAINT runner_registration_class_enrollment_fk FOREIGN KEY (enrollment_id, capability_class) REFERENCES runner_enrollment_allowed_class(enrollment_id, capability_class) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_registration_class runner_registration_class_registration_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration_class
    ADD CONSTRAINT runner_registration_class_registration_fk FOREIGN KEY (enrollment_id, registration_revision) REFERENCES runner_registration(enrollment_id, registration_revision) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_registration runner_registration_enrollment_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration
    ADD CONSTRAINT runner_registration_enrollment_fk FOREIGN KEY (enrollment_id, runner_id, authentication_reference_id) REFERENCES runner_enrollment(enrollment_id, runner_id, authentication_reference_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_registration_profile_approval runner_registration_profile_approval_profile_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration_profile_approval
    ADD CONSTRAINT runner_registration_profile_approval_profile_fk FOREIGN KEY (enrollment_id, registration_revision, credential_profile_name) REFERENCES runner_registration_profile(enrollment_id, registration_revision, credential_profile_name) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_registration_profile runner_registration_profile_registration_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration_profile
    ADD CONSTRAINT runner_registration_profile_registration_fk FOREIGN KEY (enrollment_id, registration_revision) REFERENCES runner_registration(enrollment_id, registration_revision) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_registration_repository runner_registration_repository_profile_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration_repository
    ADD CONSTRAINT runner_registration_repository_profile_fk FOREIGN KEY (enrollment_id, registration_revision, credential_profile_name) REFERENCES runner_registration_profile(enrollment_id, registration_revision, credential_profile_name) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_registration_repository runner_registration_repository_registration_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration_repository
    ADD CONSTRAINT runner_registration_repository_registration_fk FOREIGN KEY (enrollment_id, registration_revision) REFERENCES runner_registration(enrollment_id, registration_revision) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_registration_sandbox runner_registration_sandbox_registration_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration_sandbox
    ADD CONSTRAINT runner_registration_sandbox_registration_fk FOREIGN KEY (enrollment_id, registration_revision) REFERENCES runner_registration(enrollment_id, registration_revision) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_registration_tool runner_registration_tool_class_selector_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration_tool
    ADD CONSTRAINT runner_registration_tool_class_selector_fk FOREIGN KEY (enrollment_id, registration_revision, selector_capability_class) REFERENCES runner_registration_class(enrollment_id, registration_revision, capability_class) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_registration_tool runner_registration_tool_identity_selector_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration_tool
    ADD CONSTRAINT runner_registration_tool_identity_selector_fk FOREIGN KEY (enrollment_id, registration_revision, selector_runner_id) REFERENCES runner_registration(enrollment_id, registration_revision, runner_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_registration_tool runner_registration_tool_registration_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration_tool
    ADD CONSTRAINT runner_registration_tool_registration_fk FOREIGN KEY (enrollment_id, registration_revision) REFERENCES runner_registration(enrollment_id, registration_revision) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_registration_workspace runner_registration_workspace_registration_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_registration_workspace
    ADD CONSTRAINT runner_registration_workspace_registration_fk FOREIGN KEY (enrollment_id, registration_revision) REFERENCES runner_registration(enrollment_id, registration_revision) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_session_placement_record runner_session_placement_grant_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_session_placement_record
    ADD CONSTRAINT runner_session_placement_grant_fk FOREIGN KEY (session_id, credential_grant_lineage_origin_ordinal, credential_grant_runner_id, credential_grant_revision) REFERENCES runner_credential_grant(session_id, lineage_origin_event_ordinal, runner_id, grant_revision) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_session_placement_record runner_session_placement_interrupted_attempt_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_session_placement_record
    ADD CONSTRAINT runner_session_placement_interrupted_attempt_fk FOREIGN KEY (interrupted_tool_attempt_id, session_id) REFERENCES tool_attempt(attempt_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_session_placement_record runner_session_placement_loss_fence_enrollment_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_session_placement_record
    ADD CONSTRAINT runner_session_placement_loss_fence_enrollment_fk FOREIGN KEY (loss_fence_enrollment_id) REFERENCES runner_enrollment(enrollment_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_session_placement_record runner_session_placement_observed_loss_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_session_placement_record
    ADD CONSTRAINT runner_session_placement_observed_loss_fk FOREIGN KEY (loss_fence_enrollment_id, observed_runner_loss_epoch) REFERENCES runner_connection_loss_epoch(enrollment_id, loss_epoch) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_session_placement_permission_override runner_session_placement_permission_override_record_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_session_placement_permission_override
    ADD CONSTRAINT runner_session_placement_permission_override_record_fk FOREIGN KEY (session_id, event_ordinal) REFERENCES runner_session_placement_record(session_id, event_ordinal) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_session_placement_record runner_session_placement_registration_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_session_placement_record
    ADD CONSTRAINT runner_session_placement_registration_fk FOREIGN KEY (registration_enrollment_id, registration_revision, pinned_runner_id) REFERENCES runner_registration(enrollment_id, registration_revision, runner_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_session_placement_record runner_session_placement_session_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_session_placement_record
    ADD CONSTRAINT runner_session_placement_session_fk FOREIGN KEY (session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_session_placement_tool runner_session_placement_tool_record_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_session_placement_tool
    ADD CONSTRAINT runner_session_placement_tool_record_fk FOREIGN KEY (session_id, event_ordinal) REFERENCES runner_session_placement_record(session_id, event_ordinal) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_state_transition_outbox_event runner_state_transition_outbox_connection_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_state_transition_outbox_event
    ADD CONSTRAINT runner_state_transition_outbox_connection_fk FOREIGN KEY (connection_enrollment_id, connection_epoch, connection_event_ordinal) REFERENCES runner_connection_event(enrollment_id, connection_epoch, event_ordinal) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_state_transition_outbox_event runner_state_transition_outbox_header_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_state_transition_outbox_event
    ADD CONSTRAINT runner_state_transition_outbox_header_fk FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: runner_state_transition_outbox_event runner_state_transition_outbox_placement_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_state_transition_outbox_event
    ADD CONSTRAINT runner_state_transition_outbox_placement_fk FOREIGN KEY (session_id, placement_event_ordinal, placement_revision) REFERENCES runner_session_placement_record(session_id, event_ordinal, placement_revision) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: runner_tool_request_lease_binding runner_tool_request_lease_binding_request_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY runner_tool_request_lease_binding
    ADD CONSTRAINT runner_tool_request_lease_binding_request_fk FOREIGN KEY (request_id) REFERENCES tool_request(request_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: session_current_placement session_current_placement_session_id_current_version_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_current_placement
    ADD CONSTRAINT session_current_placement_session_id_current_version_fkey FOREIGN KEY (session_id, current_version) REFERENCES session_placement_event(session_id, version) ON DELETE RESTRICT;


--
-- Name: session_current_placement session_current_placement_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_current_placement
    ADD CONSTRAINT session_current_placement_session_id_fkey FOREIGN KEY (session_id) REFERENCES session(session_id) ON DELETE RESTRICT;


--
-- Name: session_placement_event session_placement_event_provenance_command_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_placement_event
    ADD CONSTRAINT session_placement_event_provenance_command_id_fkey FOREIGN KEY (provenance_command_id) REFERENCES durable_command(command_id) ON DELETE RESTRICT;


--
-- Name: session_placement_event session_placement_event_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_placement_event
    ADD CONSTRAINT session_placement_event_session_id_fkey FOREIGN KEY (session_id) REFERENCES session(session_id) ON DELETE RESTRICT;


--
-- Name: session_placement_event session_placement_event_session_id_prior_version_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_placement_event
    ADD CONSTRAINT session_placement_event_session_id_prior_version_fkey FOREIGN KEY (session_id, prior_version) REFERENCES session_placement_event(session_id, version) ON DELETE RESTRICT;


--
-- Name: update_session_placement_command update_session_placement_comm_command_id_command_kind_stor_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY update_session_placement_command
    ADD CONSTRAINT update_session_placement_comm_command_id_command_kind_stor_fkey FOREIGN KEY (command_id, command_kind, storage_version) REFERENCES durable_command(command_id, command_kind, storage_version) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: update_session_placement_command update_session_placement_comm_session_id_result_version_co_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY update_session_placement_command
    ADD CONSTRAINT update_session_placement_comm_session_id_result_version_co_fkey FOREIGN KEY (session_id, result_version, command_id) REFERENCES session_placement_event(session_id, version, provenance_command_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Restore body validation for anything applied after this file.
--

RESET check_function_bodies;
