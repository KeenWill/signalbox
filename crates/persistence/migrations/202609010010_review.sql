-- Review: review runs, passes, targets, and findings with their event log;
-- external links, observations, and object identities; and the review
-- orchestration substrate — commands, concerns, fan-out, judgment, repair,
-- and publication, each with its sealed inventories.

-- Function bodies may read tables a later section or file creates;
-- 202609010000_core.sql explains why validation is deferred.
SET check_function_bodies = false;

--
-- Functions.
--

--
-- Name: advance_review_finding_event_head(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION advance_review_finding_event_head() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    new_event_pass_kind text;
BEGIN
    SELECT pass_kind INTO new_event_pass_kind
      FROM review_pass
     WHERE pass_id = NEW.event_pass_id
       AND run_id = NEW.event_pass_run_id
       AND target_id = NEW.target_id;
    IF new_event_pass_kind IS NULL THEN
        RAISE EXCEPTION 'finding event pass is missing'
            USING ERRCODE = '23514';
    END IF;

    UPDATE review_finding_event_head
       SET event_ordinal = NEW.event_ordinal,
           status = NEW.event_kind,
           event_pass_kind = new_event_pass_kind,
           external_link_id = NEW.external_link_id
     WHERE finding_id = NEW.finding_id;
    RETURN NULL;
END;
$$;


--
-- Name: assert_review_finding_event_head_complete(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_review_finding_event_head_complete(checked_finding uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM review_finding AS finding
          JOIN review_finding_event_head AS head
            ON head.finding_id = finding.finding_id
          LEFT JOIN review_finding_event AS event
            ON event.finding_id = head.finding_id
           AND event.event_ordinal = head.event_ordinal
          LEFT JOIN review_pass AS event_pass
            ON event_pass.pass_id = event.event_pass_id
           AND event_pass.run_id = event.event_pass_run_id
           AND event_pass.target_id = event.target_id
         WHERE finding.finding_id = checked_finding
           AND (
               (
                   head.event_ordinal IS NULL
                   AND head.status = 'open'
                   AND head.event_pass_kind IS NULL
                   AND head.external_link_id IS NULL
                   AND NOT EXISTS (
                       SELECT 1
                         FROM review_finding_event AS existing
                        WHERE existing.finding_id = checked_finding
                   )
               )
               OR (
                   head.event_ordinal IS NOT NULL
                   AND event.event_kind = head.status
                   AND event_pass.pass_kind = head.event_pass_kind
                   AND event.external_link_id
                        IS NOT DISTINCT FROM head.external_link_id
                   AND head.event_ordinal = (
                       SELECT max(latest.event_ordinal)
                         FROM review_finding_event AS latest
                        WHERE latest.finding_id = checked_finding
                   )
                   AND head.event_ordinal = (
                       SELECT count(*)
                         FROM review_finding_event AS existing
                        WHERE existing.finding_id = checked_finding
                   )
               )
           )
    )
    THEN
        RAISE EXCEPTION
            'review finding event head does not name the exact latest event'
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: authenticate_review_finding_event_head(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION authenticate_review_finding_event_head() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    locked_head record;
    subject_ordinal bigint;
    subject_status text;
    subject_pass_kind text;
    subject_external_link uuid;
    referenced_status text;
BEGIN
    FOR locked_head IN
        SELECT head.finding_id,
               head.event_ordinal,
               head.status,
               head.event_pass_kind,
               head.external_link_id
          FROM review_finding_event_head AS head
         WHERE head.finding_id IN (
                   NEW.finding_id,
                   NEW.referenced_finding_id
               )
         ORDER BY head.finding_id
         FOR UPDATE
    LOOP
        IF locked_head.finding_id = NEW.finding_id THEN
            subject_ordinal := locked_head.event_ordinal;
            subject_status := locked_head.status;
            subject_pass_kind := locked_head.event_pass_kind;
            subject_external_link := locked_head.external_link_id;
        END IF;
        IF locked_head.finding_id = NEW.referenced_finding_id THEN
            referenced_status := locked_head.status;
        END IF;
    END LOOP;

    IF subject_status IS NULL THEN
        RAISE EXCEPTION 'finding event subject lacks its transition head'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.event_ordinal IS DISTINCT FROM
        COALESCE(subject_ordinal, 0) + 1
    THEN
        RAISE EXCEPTION
            'finding event ordinal %, expected %',
            NEW.event_ordinal,
            COALESCE(subject_ordinal, 0) + 1
            USING ERRCODE = '23514';
    END IF;
    IF NEW.referenced_finding_id IS NOT NULL
       AND (
           referenced_status NOT IN ('open', 'accepted')
           OR NEW.referenced_finding_status
                IS DISTINCT FROM referenced_status
       )
    THEN
        RAISE EXCEPTION
            'referenced finding status % is not eligible or authenticated',
            referenced_status
            USING ERRCODE = '23514';
    END IF;
    IF NOT (
        (
            subject_status = 'open'
            AND NEW.event_kind IN (
                'accepted',
                'rejected',
                'duplicate',
                'superseded',
                'stale'
            )
        )
        OR (
            subject_status = 'accepted'
            AND NEW.event_kind IN (
                'duplicate',
                'superseded',
                'stale',
                'posted',
                'fixed',
                'blocked_with_reason'
            )
        )
        OR (
            subject_status = 'posted'
            AND NEW.event_kind IN (
                'superseded',
                'stale',
                'fixed',
                'blocked_with_reason'
            )
        )
        OR (
            subject_status = 'blocked_with_reason'
            AND (
                NEW.event_kind IN ('superseded', 'stale', 'fixed')
                OR (
                    NEW.event_kind = 'posted'
                    AND subject_pass_kind = 'publish'
                    AND NEW.external_link_id
                        IS NOT DISTINCT FROM subject_external_link
                )
            )
        )
    ) THEN
        RAISE EXCEPTION
            'invalid finding transition from % through %',
            subject_status,
            NEW.event_kind
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: create_review_finding_event_head(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION create_review_finding_event_head() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO review_finding_event_head (
        finding_id,
        event_ordinal,
        status,
        event_pass_kind,
        external_link_id
    )
    VALUES (NEW.finding_id, NULL, 'open', NULL, NULL);
    RETURN NULL;
END;
$$;


--
-- Name: guard_bound_review_pass_referenced_target(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_bound_review_pass_referenced_target() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF OLD.result_kind IS NOT NULL
       AND NEW.result_referenced_finding_target_id
            IS DISTINCT FROM OLD.result_referenced_finding_target_id
    THEN
        RAISE EXCEPTION 'bound review pass result cannot change'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: guard_review_external_link_attachment_insert(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_review_external_link_attachment_insert() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    canonical_pass_kind text;
    canonical_pass_state text;
    canonical_result_kind text;
    canonical_result_link uuid;
    canonical_result_object text;
    canonical_result_event_kind text;
    logical_target uuid;
BEGIN
    PERFORM 1
      FROM review_external_link
     WHERE external_link_id = NEW.external_link_id
     FOR NO KEY UPDATE;

    SELECT pass_kind, state_kind, result_kind,
           result_external_link_id, result_external_object_key,
           result_event_kind
      INTO canonical_pass_kind, canonical_pass_state,
           canonical_result_kind, canonical_result_link,
           canonical_result_object, canonical_result_event_kind
      FROM review_pass
     WHERE pass_id = NEW.pass_id
       AND run_id = NEW.pass_run_id
       AND target_id = NEW.target_id;
    IF canonical_pass_kind NOT IN ('publish', 'import_external_context')
       OR canonical_pass_state IS DISTINCT FROM 'succeeded'
       OR canonical_result_kind
            IS DISTINCT FROM 'external_link_attachment'
       OR canonical_result_link
            IS DISTINCT FROM NEW.external_link_id
       OR canonical_result_object
            IS DISTINCT FROM NEW.external_object_key
    THEN
        RAISE EXCEPTION
            'external attachment requires a succeeded attaching pass'
            USING ERRCODE = '23514';
    END IF;
    IF canonical_result_event_kind IS DISTINCT FROM 'posted'
       AND EXISTS (
           SELECT 1
             FROM review_external_link AS link
             JOIN LATERAL (
                 SELECT event_kind, external_link_id
                   FROM review_finding_event
                  WHERE finding_id = link.finding_id
                  ORDER BY event_ordinal DESC
                  LIMIT 1
             ) AS latest ON true
            WHERE link.external_link_id = NEW.external_link_id
              AND link.association_kind = 'finding'
              AND latest.event_kind = 'blocked_with_reason'
              AND latest.external_link_id = NEW.external_link_id
       )
    THEN
        RAISE EXCEPTION
            'blocked publication attachment requires an atomic posted event'
            USING ERRCODE = '23514';
    END IF;

    PERFORM pg_advisory_xact_lock(
        hashtextextended(
            NEW.provider_key
                || chr(31)
                || NEW.object_kind
                || chr(31)
                || NEW.external_object_key,
            0
        )
    );
    IF EXISTS (
        SELECT 1
          FROM review_external_link_attachment
         WHERE identity_digest = md5(
                   NEW.provider_key
                       || chr(31)
                       || NEW.object_kind
                       || chr(31)
                       || NEW.external_object_key
               )
           AND target_id = NEW.target_id
           AND provider_key = NEW.provider_key
           AND object_kind = NEW.object_kind
           AND external_object_key = NEW.external_object_key
    )
    THEN
        RAISE EXCEPTION
            'external object identity is already attached to this target'
            USING ERRCODE = '23505';
    END IF;
    SELECT logical_target_id
      INTO logical_target
      FROM review_external_object_identity
     WHERE identity_digest = md5(
               NEW.provider_key
                   || chr(31)
                   || NEW.object_kind
                   || chr(31)
                   || NEW.external_object_key
           )
       AND provider_key = NEW.provider_key
       AND object_kind = NEW.object_kind
       AND external_object_key = NEW.external_object_key;
    IF NOT FOUND THEN
        INSERT INTO review_external_object_identity
            (provider_key, object_kind, external_object_key,
             logical_target_id)
        VALUES
            (NEW.provider_key, NEW.object_kind, NEW.external_object_key,
             NEW.target_id);
    ELSIF logical_target <> NEW.target_id
          AND NOT EXISTS (
              SELECT 1
                FROM review_target AS established
                JOIN review_target AS candidate
                  ON candidate.target_id = NEW.target_id
               WHERE established.target_id = logical_target
                 AND established.subject_kind = 'change_request'
                 AND candidate.subject_kind = 'change_request'
                 AND established.provider_key = candidate.provider_key
                 AND established.repository_key = candidate.repository_key
                 AND established.change_request_number
                        = candidate.change_request_number
          )
    THEN
        RAISE EXCEPTION
            'external object identity belongs to another logical target'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: guard_review_external_link_insert(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_review_external_link_insert() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    canonical_provider text;
BEGIN
    SELECT provider_key
      INTO canonical_provider
      FROM review_target
     WHERE target_id = NEW.target_id;
    IF canonical_provider IS DISTINCT FROM NEW.provider_key THEN
        RAISE EXCEPTION
            'external-link provider differs from canonical target provider'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: guard_review_external_object_identity_insert(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_review_external_object_identity_insert() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    canonical_provider text;
BEGIN
    PERFORM pg_advisory_xact_lock(
        hashtextextended(
            NEW.provider_key
                || chr(31)
                || NEW.object_kind
                || chr(31)
                || NEW.external_object_key,
            0
        )
    );

    SELECT provider_key
      INTO canonical_provider
      FROM review_target
     WHERE target_id = NEW.logical_target_id;
    IF canonical_provider IS DISTINCT FROM NEW.provider_key THEN
        RAISE EXCEPTION
            'external object identity target provider mismatch'
            USING ERRCODE = '23514';
    END IF;

    IF EXISTS (
        SELECT 1
         FROM review_external_object_identity
         WHERE identity_digest = md5(
                   NEW.provider_key
                       || chr(31)
                       || NEW.object_kind
                       || chr(31)
                       || NEW.external_object_key
               )
           AND provider_key = NEW.provider_key
           AND object_kind = NEW.object_kind
           AND external_object_key = NEW.external_object_key
    )
    THEN
        RAISE EXCEPTION
            'external object identity is already established'
            USING ERRCODE = '23505';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: guard_review_finding_event_head_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_review_finding_event_head_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'review finding event head is not deletable'
            USING ERRCODE = '23514';
    END IF;
    IF TG_OP = 'INSERT' THEN
        IF NEW.event_ordinal IS NOT NULL
           OR NEW.status <> 'open'
           OR NEW.event_pass_kind IS NOT NULL
           OR NEW.external_link_id IS NOT NULL
           OR EXISTS (
               SELECT 1
                 FROM review_finding_event
                WHERE finding_id = NEW.finding_id
           )
        THEN
            RAISE EXCEPTION 'review finding event head must begin open'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;
    IF NEW.finding_id <> OLD.finding_id
       OR NEW.event_ordinal IS DISTINCT FROM
            COALESCE(OLD.event_ordinal, 0) + 1
       OR NEW.status = 'open'
       OR NEW.event_pass_kind IS NULL
       OR NOT EXISTS (
           SELECT 1
             FROM review_finding_event AS event
             JOIN review_pass AS event_pass
               ON event_pass.pass_id = event.event_pass_id
              AND event_pass.run_id = event.event_pass_run_id
              AND event_pass.target_id = event.target_id
            WHERE event.finding_id = NEW.finding_id
              AND event.event_ordinal = NEW.event_ordinal
              AND event.event_kind = NEW.status
              AND event_pass.pass_kind = NEW.event_pass_kind
              AND event.external_link_id
                    IS NOT DISTINCT FROM NEW.external_link_id
       )
    THEN
        RAISE EXCEPTION
            'review finding event head must advance to its exact durable event'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: guard_review_finding_insert(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_review_finding_insert() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    canonical_pass_kind text;
    canonical_pass_state text;
    canonical_result_kind text;
BEGIN
    IF NEW.diff_side IS NOT NULL
       AND NOT EXISTS (
           SELECT 1
             FROM review_target
            WHERE target_id = NEW.target_id
              AND base_revision IS NOT NULL
       )
    THEN
        RAISE EXCEPTION 'diff-relative finding requires a target base'
            USING ERRCODE = '23514';
    END IF;
    SELECT pass_kind, state_kind, result_kind
      INTO canonical_pass_kind, canonical_pass_state,
           canonical_result_kind
      FROM review_pass
     WHERE pass_id = NEW.producing_pass_id
       AND run_id = NEW.run_id
       AND target_id = NEW.target_id;
    IF canonical_pass_kind IS DISTINCT FROM 'read_only_review'
       OR canonical_pass_state IS DISTINCT FROM 'succeeded'
       OR canonical_result_kind IS DISTINCT FROM 'produced_findings'
    THEN
        RAISE EXCEPTION
            'finding producer must be a succeeded read-only-review pass'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: guard_review_pass_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_review_pass_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    canonical_run_workflow text;
    canonical_turn_state text;
    canonical_turn_disposition text;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.state_kind IS DISTINCT FROM 'queued'
           OR NEW.turn_id IS NOT NULL
           OR NEW.output_frontier_id IS NOT NULL
        THEN
            RAISE EXCEPTION 'review pass must begin queued'
                USING ERRCODE = '23514';
        END IF;
        SELECT workflow_kind
          INTO canonical_run_workflow
          FROM review_run
         WHERE run_id = NEW.run_id
           AND target_id = NEW.target_id;
        IF canonical_run_workflow IS NULL
           OR NOT (
               (canonical_run_workflow = 'import_external_context'
                AND NEW.pass_kind = 'import_external_context')
               OR (canonical_run_workflow = 'read_only_review'
                   AND NEW.pass_kind = 'read_only_review')
               OR (canonical_run_workflow = 'judge_findings'
                   AND NEW.pass_kind = 'judge')
               OR (canonical_run_workflow = 'dedupe_findings'
                   AND NEW.pass_kind = 'dedupe')
               OR (canonical_run_workflow = 'publish_review'
                   AND NEW.pass_kind = 'publish')
               OR (canonical_run_workflow = 'fix_findings'
                   AND NEW.pass_kind = 'fix')
               OR (canonical_run_workflow = 'propagate_stack'
                   AND NEW.pass_kind = 'propagate_stack')
           )
        THEN
            RAISE EXCEPTION
                'review pass kind % contradicts run workflow %',
                NEW.pass_kind,
                canonical_run_workflow
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF (NEW.pass_id, NEW.run_id, NEW.target_id, NEW.pass_kind,
        NEW.session_id, NEW.accepted_input_id, NEW.origin_turn_id)
       IS DISTINCT FROM
       (OLD.pass_id, OLD.run_id, OLD.target_id, OLD.pass_kind,
        OLD.session_id, OLD.accepted_input_id, OLD.origin_turn_id)
    THEN
        RAISE EXCEPTION 'review pass immutable facts cannot change'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.result_kind IS NOT NULL
       AND (
           NEW.result_kind,
           NEW.result_finding_id,
           NEW.result_finding_run_id,
           NEW.result_finding_pass_id,
           NEW.result_event_ordinal,
           NEW.result_event_kind,
           NEW.result_reason,
           NEW.result_referenced_finding_id,
           NEW.result_referenced_finding_run_id,
           NEW.result_referenced_finding_pass_id,
           NEW.result_referenced_finding_status,
           NEW.result_external_link_id,
           NEW.result_external_object_key,
           NEW.result_observation_state
       ) IS DISTINCT FROM (
           OLD.result_kind,
           OLD.result_finding_id,
           OLD.result_finding_run_id,
           OLD.result_finding_pass_id,
           OLD.result_event_ordinal,
           OLD.result_event_kind,
           OLD.result_reason,
           OLD.result_referenced_finding_id,
           OLD.result_referenced_finding_run_id,
           OLD.result_referenced_finding_pass_id,
           OLD.result_referenced_finding_status,
           OLD.result_external_link_id,
           OLD.result_external_object_key,
           OLD.result_observation_state
       )
    THEN
        RAISE EXCEPTION 'bound review pass result cannot change'
            USING ERRCODE = '23514';
    END IF;

    IF (NEW.state_kind, NEW.turn_id, NEW.output_frontier_id)
       IS NOT DISTINCT FROM
       (OLD.state_kind, OLD.turn_id, OLD.output_frontier_id)
    THEN
        IF OLD.result_kind IS NULL
           AND NEW.result_kind IS NOT NULL
           AND NOT (
               (
                   NEW.result_kind = 'produced_findings'
                   AND NEW.state_kind = 'succeeded'
                   AND NEW.pass_kind = 'read_only_review'
               )
               OR (
                   NEW.result_kind = 'finding_event'
                   AND (
                       (
                           NEW.result_event_kind IN (
                               'accepted',
                               'rejected',
                               'stale'
                           )
                           AND NEW.state_kind = 'succeeded'
                           AND NEW.pass_kind = 'judge'
                       )
                       OR (
                           NEW.result_event_kind IN (
                               'duplicate',
                               'superseded'
                           )
                           AND NEW.state_kind = 'succeeded'
                           AND NEW.pass_kind = 'dedupe'
                       )
                       OR (
                           NEW.result_event_kind = 'fixed'
                           AND NEW.state_kind = 'succeeded'
                           AND NEW.pass_kind = 'fix'
                       )
                       OR (
                           NEW.result_event_kind = 'blocked_with_reason'
                           AND NEW.state_kind = 'blocked'
                           AND (
                               (
                                   NEW.pass_kind = 'publish'
                                   AND NEW.result_external_link_id
                                       IS NOT NULL
                               )
                               OR (
                                   NEW.pass_kind = 'fix'
                                   AND NEW.result_external_link_id IS NULL
                               )
                           )
                       )
                   )
               )
               OR (
                   NEW.result_kind = 'external_link_attachment'
                   AND NEW.state_kind = 'succeeded'
                   AND NEW.pass_kind IN (
                       'publish',
                       'import_external_context'
                   )
               )
               OR (
                   NEW.result_kind = 'external_link_observation'
                   AND NEW.state_kind = 'succeeded'
                   AND NEW.pass_kind = 'import_external_context'
               )
               OR (
                   NEW.result_kind = 'external_link_no_change'
                   AND NEW.state_kind = 'succeeded'
                   AND NEW.pass_kind = 'import_external_context'
               )
               OR (
                   NEW.result_kind = 'external_link_publication_blocked'
                   AND NEW.state_kind = 'blocked'
                   AND NEW.pass_kind = 'publish'
               )
           )
        THEN
            RAISE EXCEPTION
                'review pass result is incompatible with pass outcome'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF NEW.result_kind IS NOT NULL THEN
        RAISE EXCEPTION
            'review pass lifecycle transition cannot bind an effect result'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'queued' THEN
        IF NOT (
            NEW.state_kind = 'running'
            OR (
                NEW.state_kind = 'cancelled'
                AND NEW.turn_id IS NULL
            )
        ) THEN
            RAISE EXCEPTION 'invalid queued review pass transition'
                USING ERRCODE = '23514';
        END IF;
    ELSIF OLD.state_kind = 'running' THEN
        IF NOT (
            NEW.state_kind IN (
                'succeeded',
                'failed',
                'blocked',
                'cancelled'
            )
            AND NEW.turn_id IS NOT DISTINCT FROM OLD.turn_id
        ) THEN
            RAISE EXCEPTION 'invalid running review pass transition'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        RAISE EXCEPTION 'terminal review pass cannot transition'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.turn_id IS NOT NULL THEN
        SELECT state_kind, terminal_disposition_kind
          INTO canonical_turn_state, canonical_turn_disposition
          FROM turn_lifecycle
         WHERE turn_id = NEW.turn_id
           AND session_id = NEW.session_id
           AND origin_accepted_input_id = NEW.accepted_input_id;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'review pass referenced turn is missing'
                USING ERRCODE = '23514';
        END IF;
        IF NOT (
            (
                NEW.state_kind = 'running'
                AND canonical_turn_state = 'active'
                AND canonical_turn_disposition IS NULL
            )
            OR (
                NEW.state_kind = 'succeeded'
                AND canonical_turn_state = 'terminal'
                AND canonical_turn_disposition = 'completed'
            )
            OR (
                NEW.state_kind = 'failed'
                AND canonical_turn_state = 'terminal'
                AND canonical_turn_disposition IN (
                    'completed',
                    'failed',
                    'refused'
                )
            )
            OR (
                NEW.state_kind = 'blocked'
                AND canonical_turn_state = 'terminal'
                AND canonical_turn_disposition = 'reconciliation_required'
            )
            OR (
                NEW.state_kind = 'cancelled'
                AND canonical_turn_state = 'terminal'
                AND canonical_turn_disposition = 'cancelled'
            )
        ) THEN
            RAISE EXCEPTION 'review pass state contradicts canonical turn'
                USING ERRCODE = '23514';
        END IF;
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: guard_review_pass_finding_inventory_seal(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_review_pass_finding_inventory_seal() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    canonical_kind text;
    canonical_findings integer;
    inventory_members integer;
BEGIN
    SELECT result_kind
      INTO canonical_kind
      FROM review_pass
     WHERE pass_id = NEW.pass_id;
    SELECT count(*)
      INTO canonical_findings
      FROM review_finding
     WHERE producing_pass_id = NEW.pass_id;
    SELECT count(*)
      INTO inventory_members
      FROM review_pass_produced_finding
     WHERE pass_id = NEW.pass_id;
    IF canonical_kind IS DISTINCT FROM 'produced_findings'
       OR NEW.finding_count IS DISTINCT FROM canonical_findings
       OR NEW.finding_count IS DISTINCT FROM inventory_members
    THEN
        RAISE EXCEPTION
            'review pass finding inventory seal differs from canonical findings'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: guard_review_run_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_review_run_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    canonical_pass_state text;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.state_kind IS DISTINCT FROM 'queued'
           OR NEW.state_pass_id IS NOT NULL
        THEN
            RAISE EXCEPTION 'review run must begin queued'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF (NEW.run_id, NEW.target_id, NEW.workflow_kind, NEW.policy_version,
        NEW.minimum_judge_confidence, NEW.minimum_publication_confidence)
       IS DISTINCT FROM
       (OLD.run_id, OLD.target_id, OLD.workflow_kind, OLD.policy_version,
        OLD.minimum_judge_confidence, OLD.minimum_publication_confidence)
    THEN
        RAISE EXCEPTION 'review run immutable facts cannot change'
            USING ERRCODE = '23514';
    END IF;

    IF (NEW.state_kind, NEW.state_pass_id)
       IS NOT DISTINCT FROM
       (OLD.state_kind, OLD.state_pass_id)
    THEN
        RETURN NEW;
    END IF;

    IF OLD.state_kind = 'queued' THEN
        IF NOT (
            NEW.state_kind = 'running'
            OR NEW.state_kind = 'cancelled'
        ) THEN
            RAISE EXCEPTION 'invalid queued review run transition'
                USING ERRCODE = '23514';
        END IF;
    ELSIF OLD.state_kind = 'running' THEN
        IF NOT (
            NEW.state_kind IN (
                'succeeded',
                'failed',
                'blocked',
                'cancelled'
            )
            AND NEW.state_pass_id IS NOT DISTINCT FROM OLD.state_pass_id
        ) THEN
            RAISE EXCEPTION 'invalid running review run transition'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        RAISE EXCEPTION 'terminal review run cannot transition'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.state_pass_id IS NOT NULL THEN
        SELECT state_kind
          INTO canonical_pass_state
          FROM review_pass
         WHERE pass_id = NEW.state_pass_id
           AND run_id = NEW.run_id
           AND target_id = NEW.target_id;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'review run referenced pass is missing'
                USING ERRCODE = '23514';
        END IF;
        IF canonical_pass_state IS DISTINCT FROM NEW.state_kind THEN
            RAISE EXCEPTION 'review run state contradicts canonical pass'
                USING ERRCODE = '23514';
        END IF;
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: guard_review_target_parent(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_review_target_parent() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    parent_provider text;
    parent_repository text;
    parent_head text;
BEGIN
    IF NEW.stack_parent_target_id IS NULL THEN
        RETURN NEW;
    END IF;
    SELECT provider_key, repository_key, head_revision
      INTO parent_provider, parent_repository, parent_head
      FROM review_target
     WHERE target_id = NEW.stack_parent_target_id;
    IF NOT FOUND
       OR parent_provider IS DISTINCT FROM NEW.provider_key
       OR parent_repository IS DISTINCT FROM NEW.repository_key
       OR parent_head IS DISTINCT FROM NEW.base_revision
    THEN
        RAISE EXCEPTION
            'review target stack parent does not match repository and base'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.subject_kind = 'change_request'
       AND EXISTS (
           WITH RECURSIVE ancestors AS (
               SELECT target_id, stack_parent_target_id,
                      provider_key, repository_key,
                      subject_kind, change_request_number
                 FROM review_target
                WHERE target_id = NEW.stack_parent_target_id
               UNION ALL
               SELECT parent.target_id, parent.stack_parent_target_id,
                      parent.provider_key, parent.repository_key,
                      parent.subject_kind, parent.change_request_number
                 FROM review_target AS parent
                 JOIN ancestors
                   ON parent.target_id = ancestors.stack_parent_target_id
           )
           SELECT 1
             FROM ancestors
            WHERE provider_key = NEW.provider_key
              AND repository_key = NEW.repository_key
              AND subject_kind = 'change_request'
              AND change_request_number = NEW.change_request_number
       )
    THEN
        RAISE EXCEPTION
            'review target stack repeats a logical change request'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: reject_review_orchestration_intent_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_review_orchestration_intent_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'UPDATE' THEN
        RAISE EXCEPTION 'review orchestration command intent is immutable'
            USING ERRCODE = '23514';
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM review_orchestration_command WHERE command_id = OLD.command_id
    ) THEN
        RAISE EXCEPTION 'review orchestration command intent requires its replacement receipt'
            USING ERRCODE = '23503';
    END IF;
    RETURN OLD;
END;
$$;


--
-- Name: reject_review_workflow_command_truncate(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_review_workflow_command_truncate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION 'review workflow command receipts are append-only'
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: reject_review_workflow_truncate(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_review_workflow_truncate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION 'review workflow tables cannot be truncated'
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: reject_sealed_review_finding_inventory_expansion(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_sealed_review_finding_inventory_expansion() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_pass uuid;
BEGIN
    checked_pass := COALESCE(
        (to_jsonb(NEW) ->> 'pass_id')::uuid,
        (to_jsonb(NEW) ->> 'producing_pass_id')::uuid
    );
    IF EXISTS (
        SELECT 1
          FROM review_pass_finding_inventory_seal
         WHERE pass_id = checked_pass
    )
    THEN
        RAISE EXCEPTION 'sealed review finding inventory cannot expand'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: require_review_attachment_posted_event(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_review_attachment_posted_event() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    canonical_event_kind text;
    canonical_finding uuid;
    canonical_ordinal bigint;
BEGIN
    SELECT result_event_kind, result_finding_id, result_event_ordinal
      INTO canonical_event_kind, canonical_finding, canonical_ordinal
      FROM review_pass
     WHERE pass_id = NEW.pass_id
       AND run_id = NEW.pass_run_id
       AND target_id = NEW.target_id;
    IF canonical_event_kind = 'posted'
       AND NOT EXISTS (
           SELECT 1
             FROM review_finding_event
            WHERE finding_id = canonical_finding
              AND event_ordinal = canonical_ordinal
              AND target_id = NEW.target_id
              AND event_pass_id = NEW.pass_id
              AND event_pass_run_id = NEW.pass_run_id
              AND event_kind = 'posted'
              AND external_link_id = NEW.external_link_id
       )
    THEN
        RAISE EXCEPTION
            'external attachment omitted its exact posted finding event'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_review_external_identity_attachment(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_review_external_identity_attachment() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM review_external_link_attachment AS attachment
          JOIN review_target AS attached_target
            ON attached_target.target_id = attachment.target_id
         JOIN review_target AS logical_target
            ON logical_target.target_id = NEW.logical_target_id
         WHERE attachment.identity_digest = NEW.identity_digest
           AND attachment.provider_key = NEW.provider_key
           AND attachment.object_kind = NEW.object_kind
           AND attachment.external_object_key = NEW.external_object_key
           AND (
               attachment.target_id = NEW.logical_target_id
               OR (
                   attached_target.subject_kind = 'change_request'
                   AND logical_target.subject_kind = 'change_request'
                   AND attached_target.provider_key =
                       logical_target.provider_key
                   AND attached_target.repository_key =
                       logical_target.repository_key
                   AND attached_target.change_request_number =
                       logical_target.change_request_number
               )
           )
    )
    THEN
        RAISE EXCEPTION
            'external object identity lacks an establishing attachment'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_review_external_observation_sequence(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_review_external_observation_sequence() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    expected_ordinal bigint;
    latest_state text;
    canonical_pass_kind text;
    canonical_pass_state text;
    canonical_result_kind text;
    canonical_result_link uuid;
    canonical_result_ordinal bigint;
    canonical_result_state text;
BEGIN
    PERFORM 1
      FROM review_external_link
     WHERE external_link_id = NEW.external_link_id
     FOR NO KEY UPDATE;

    SELECT pass_kind, state_kind, result_kind,
           result_external_link_id, result_event_ordinal,
           result_observation_state
      INTO canonical_pass_kind, canonical_pass_state,
           canonical_result_kind, canonical_result_link,
           canonical_result_ordinal, canonical_result_state
      FROM review_pass
     WHERE pass_id = NEW.pass_id
       AND run_id = NEW.pass_run_id
       AND target_id = NEW.target_id;
    IF canonical_pass_kind IS DISTINCT FROM 'import_external_context'
       OR canonical_pass_state IS DISTINCT FROM 'succeeded'
       OR canonical_result_kind
            IS DISTINCT FROM 'external_link_observation'
       OR canonical_result_link
            IS DISTINCT FROM NEW.external_link_id
       OR canonical_result_ordinal
            IS DISTINCT FROM NEW.observation_ordinal
       OR canonical_result_state
            IS DISTINCT FROM NEW.object_state
    THEN
        RAISE EXCEPTION
            'external observation requires a succeeded import pass'
            USING ERRCODE = '23514';
    END IF;

    SELECT observation_ordinal + 1, object_state
      INTO expected_ordinal, latest_state
      FROM review_external_link_observation
     WHERE external_link_id = NEW.external_link_id
     ORDER BY observation_ordinal DESC
     LIMIT 1;
    expected_ordinal := COALESCE(expected_ordinal, 1);

    IF latest_state IS NOT NULL
       AND latest_state IS NOT DISTINCT FROM NEW.object_state
    THEN
        RAISE EXCEPTION
            'unchanged external observation is not durable evidence'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.observation_ordinal <> expected_ordinal THEN
        RAISE EXCEPTION
            'external observation ordinal %, expected %',
            NEW.observation_ordinal,
            expected_ordinal
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: require_review_finding_event_head_complete(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_review_finding_event_head_complete() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM assert_review_finding_event_head_complete(
        COALESCE(NEW.finding_id, OLD.finding_id)
    );
    RETURN NULL;
END;
$$;


--
-- Name: require_review_finding_event_sequence(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_review_finding_event_sequence() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    event_pass_kind text;
    event_pass_state text;
    event_pass_result_kind text;
    event_pass_result_finding uuid;
    event_pass_result_run uuid;
    event_pass_result_pass uuid;
    event_pass_result_ordinal bigint;
    event_pass_result_event_kind text;
    event_pass_result_reason text;
    event_pass_result_referenced_finding uuid;
    event_pass_result_referenced_run uuid;
    event_pass_result_referenced_target uuid;
    event_pass_result_referenced_pass uuid;
    event_pass_result_referenced_status text;
    event_pass_result_external_link uuid;
    event_policy_version bigint;
    event_judge_confidence integer;
    event_publication_confidence integer;
    finding_policy_version bigint;
    finding_judge_confidence integer;
    finding_publication_confidence integer;
    finding_is_real_confidence integer;
    finding_producing_pass uuid;
    referenced_pass_kind text;
    referenced_pass_state text;
    referenced_pass_result_kind text;
    referenced_run_state text;
    referenced_run_state_pass uuid;
    referenced_policy_version bigint;
    referenced_judge_confidence integer;
    referenced_publication_confidence integer;
    referenced_seal_count integer;
BEGIN
    PERFORM finding_id
      FROM review_finding
     WHERE finding_id IN (
         NEW.finding_id,
         NEW.referenced_finding_id
     )
     ORDER BY finding_id
     FOR NO KEY UPDATE;

    SELECT finding.is_real_confidence,
           finding.producing_pass_id,
           producing_run.policy_version,
           producing_run.minimum_judge_confidence,
           producing_run.minimum_publication_confidence
      INTO finding_is_real_confidence,
           finding_producing_pass,
           finding_policy_version,
           finding_judge_confidence,
           finding_publication_confidence
      FROM review_finding AS finding
      JOIN review_run AS producing_run
        ON producing_run.run_id = finding.run_id
       AND producing_run.target_id = finding.target_id
     WHERE finding.finding_id = NEW.finding_id
       AND finding.run_id = NEW.finding_run_id
       AND finding.target_id = NEW.target_id;

    SELECT pass.pass_kind, pass.state_kind,
           pass.result_kind,
           pass.result_finding_id,
           pass.result_finding_run_id,
           pass.result_finding_pass_id,
           pass.result_event_ordinal,
           pass.result_event_kind,
           pass.result_reason,
           pass.result_referenced_finding_id,
           pass.result_referenced_finding_run_id,
           pass.result_referenced_finding_target_id,
           pass.result_referenced_finding_pass_id,
           pass.result_referenced_finding_status,
           pass.result_external_link_id,
           event_run.policy_version,
           event_run.minimum_judge_confidence,
           event_run.minimum_publication_confidence
      INTO event_pass_kind, event_pass_state,
           event_pass_result_kind,
           event_pass_result_finding,
           event_pass_result_run,
           event_pass_result_pass,
           event_pass_result_ordinal,
           event_pass_result_event_kind,
           event_pass_result_reason,
           event_pass_result_referenced_finding,
           event_pass_result_referenced_run,
           event_pass_result_referenced_target,
           event_pass_result_referenced_pass,
           event_pass_result_referenced_status,
           event_pass_result_external_link,
           event_policy_version,
           event_judge_confidence,
           event_publication_confidence
      FROM review_pass AS pass
      JOIN review_run AS event_run
        ON event_run.run_id = pass.run_id
       AND event_run.target_id = pass.target_id
     WHERE pass.pass_id = NEW.event_pass_id
       AND pass.run_id = NEW.event_pass_run_id
       AND pass.target_id = NEW.target_id;

    IF event_policy_version IS DISTINCT FROM finding_policy_version
       OR event_judge_confidence IS DISTINCT FROM finding_judge_confidence
       OR event_publication_confidence
            IS DISTINCT FROM finding_publication_confidence
    THEN
        RAISE EXCEPTION
            'finding event pass policy differs from finding policy'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.referenced_finding_id IS NOT NULL THEN
        SELECT referenced_pass.pass_kind,
               referenced_pass.state_kind,
               referenced_pass.result_kind,
               referenced_run.state_kind,
               referenced_run.state_pass_id,
               referenced_run.policy_version,
               referenced_run.minimum_judge_confidence,
               referenced_run.minimum_publication_confidence,
               seal.finding_count
          INTO referenced_pass_kind,
               referenced_pass_state,
               referenced_pass_result_kind,
               referenced_run_state,
               referenced_run_state_pass,
               referenced_policy_version,
               referenced_judge_confidence,
               referenced_publication_confidence,
               referenced_seal_count
          FROM review_finding AS referenced
          JOIN review_pass AS referenced_pass
            ON referenced_pass.pass_id =
                referenced.producing_pass_id
           AND referenced_pass.run_id = referenced.run_id
           AND referenced_pass.target_id = referenced.target_id
          JOIN review_run AS referenced_run
            ON referenced_run.run_id = referenced.run_id
           AND referenced_run.target_id = referenced.target_id
          LEFT JOIN review_pass_finding_inventory_seal AS seal
            ON seal.pass_id = referenced.producing_pass_id
         WHERE referenced.finding_id = NEW.referenced_finding_id
           AND referenced.run_id = NEW.referenced_finding_run_id
           AND referenced.target_id = NEW.referenced_finding_target_id
           AND referenced.producing_pass_id =
                NEW.referenced_finding_pass_id;

        IF NEW.referenced_finding_target_id
                IS DISTINCT FROM NEW.target_id
           OR referenced_pass_kind
                IS DISTINCT FROM 'read_only_review'
           OR referenced_pass_state IS DISTINCT FROM 'succeeded'
           OR referenced_pass_result_kind
                IS DISTINCT FROM 'produced_findings'
           OR referenced_run_state IS DISTINCT FROM 'succeeded'
           OR referenced_run_state_pass
                IS DISTINCT FROM NEW.referenced_finding_pass_id
           OR referenced_seal_count IS NULL
           OR NOT EXISTS (
               SELECT 1
                 FROM review_pass_produced_finding
                WHERE finding_id = NEW.referenced_finding_id
                  AND finding_run_id =
                        NEW.referenced_finding_run_id
                  AND target_id =
                        NEW.referenced_finding_target_id
                  AND finding_pass_id =
                        NEW.referenced_finding_pass_id
                  AND pass_id =
                        NEW.referenced_finding_pass_id
           )
        THEN
            RAISE EXCEPTION
                'referenced finding producer or sealed inventory is invalid'
                USING ERRCODE = '23514';
        END IF;

        IF referenced_policy_version
                IS DISTINCT FROM finding_policy_version
           OR referenced_judge_confidence
                IS DISTINCT FROM finding_judge_confidence
           OR referenced_publication_confidence
                IS DISTINCT FROM finding_publication_confidence
        THEN
            RAISE EXCEPTION
                'referenced finding policy differs from finding policy'
                USING ERRCODE = '23514';
        END IF;
    END IF;

    IF NEW.event_kind = 'accepted'
       AND finding_is_real_confidence < finding_judge_confidence
    THEN
        RAISE EXCEPTION
            'finding is-real confidence is below the judge threshold'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.event_kind = 'posted'
       AND finding_is_real_confidence < finding_publication_confidence
    THEN
        RAISE EXCEPTION
            'finding is-real confidence is below the publication threshold'
            USING ERRCODE = '23514';
    END IF;

    IF event_pass_kind IS NULL
       OR NOT (
           (
               NEW.event_kind IN ('accepted', 'rejected', 'stale')
               AND event_pass_kind = 'judge'
           )
           OR (
               NEW.event_kind IN ('duplicate', 'superseded')
               AND event_pass_kind = 'dedupe'
           )
           OR (
               NEW.event_kind = 'posted'
               AND event_pass_kind IN (
                   'publish',
                   'import_external_context'
               )
           )
           OR (
               NEW.event_kind = 'fixed'
               AND event_pass_kind = 'fix'
           )
           OR (
               NEW.event_kind = 'blocked_with_reason'
               AND (
                   (
                       event_pass_kind = 'publish'
                       AND NEW.external_link_id IS NOT NULL
                   )
                   OR (
                       event_pass_kind = 'fix'
                       AND NEW.external_link_id IS NULL
                   )
               )
           )
       )
    THEN
        RAISE EXCEPTION
            'finding event % is incompatible with pass kind %',
            NEW.event_kind,
            event_pass_kind
            USING ERRCODE = '23514';
    END IF;

    IF (
        NEW.event_kind = 'blocked_with_reason'
        AND event_pass_state IS DISTINCT FROM 'blocked'
    ) OR (
        NEW.event_kind <> 'blocked_with_reason'
        AND event_pass_state IS DISTINCT FROM 'succeeded'
    )
    THEN
        RAISE EXCEPTION
            'finding event % is incompatible with pass state %',
            NEW.event_kind,
            event_pass_state
            USING ERRCODE = '23514';
    END IF;

    IF event_pass_result_kind IS DISTINCT FROM (
           CASE NEW.event_kind
               WHEN 'posted' THEN 'external_link_attachment'
               ELSE 'finding_event'
           END
       )
       OR event_pass_result_finding IS DISTINCT FROM NEW.finding_id
       OR event_pass_result_run IS DISTINCT FROM NEW.finding_run_id
       OR event_pass_result_pass IS DISTINCT FROM (
           SELECT producing_pass_id
             FROM review_finding
            WHERE finding_id = NEW.finding_id
              AND run_id = NEW.finding_run_id
              AND target_id = NEW.target_id
       )
       OR event_pass_result_ordinal IS DISTINCT FROM NEW.event_ordinal
       OR event_pass_result_event_kind IS DISTINCT FROM NEW.event_kind
       OR event_pass_result_reason IS DISTINCT FROM NEW.reason
       OR event_pass_result_referenced_finding
            IS DISTINCT FROM NEW.referenced_finding_id
       OR event_pass_result_referenced_run
            IS DISTINCT FROM NEW.referenced_finding_run_id
       OR event_pass_result_referenced_target
            IS DISTINCT FROM NEW.referenced_finding_target_id
       OR event_pass_result_referenced_pass
            IS DISTINCT FROM NEW.referenced_finding_pass_id
       OR event_pass_result_referenced_status
            IS DISTINCT FROM NEW.referenced_finding_status
       OR event_pass_result_external_link
            IS DISTINCT FROM NEW.external_link_id
    THEN
        RAISE EXCEPTION
            'finding event is not the exact result committed by its pass'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.referenced_finding_id IS NOT NULL THEN
        IF EXISTS (
            WITH RECURSIVE referenced_ancestry(
                finding_id,
                run_id,
                target_id,
                pass_id
            ) AS (
                SELECT NEW.referenced_finding_id,
                       NEW.referenced_finding_run_id,
                       NEW.referenced_finding_target_id,
                       NEW.referenced_finding_pass_id
                UNION
                SELECT latest.referenced_finding_id,
                       latest.referenced_finding_run_id,
                       latest.referenced_finding_target_id,
                       latest.referenced_finding_pass_id
                  FROM referenced_ancestry AS ancestry
                  JOIN LATERAL (
                      SELECT referenced_finding_id,
                             referenced_finding_run_id,
                             referenced_finding_target_id,
                             referenced_finding_pass_id
                        FROM review_finding_event
                       WHERE finding_id = ancestry.finding_id
                         AND finding_run_id = ancestry.run_id
                         AND target_id = ancestry.target_id
                         AND event_kind IN (
                             'duplicate',
                             'superseded'
                         )
                       ORDER BY event_ordinal DESC
                       LIMIT 1
                  ) AS latest
                    ON latest.referenced_finding_id IS NOT NULL
            )
            SELECT 1
              FROM referenced_ancestry
             WHERE finding_id = NEW.finding_id
               AND run_id = NEW.finding_run_id
               AND target_id = NEW.target_id
               AND pass_id = finding_producing_pass
        )
        THEN
            RAISE EXCEPTION
                'finding reference would create a cycle'
                USING ERRCODE = '23514';
        END IF;
    END IF;

    IF NEW.event_kind = 'blocked_with_reason'
       AND NEW.external_link_id IS NOT NULL
       AND EXISTS (
           SELECT 1
             FROM review_external_link_attachment
            WHERE external_link_id = NEW.external_link_id
              AND target_id = NEW.target_id
       )
    THEN
        RAISE EXCEPTION
            'publication block requires a pending reservation'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.event_kind = 'posted'
       AND EXISTS (
           SELECT 1
             FROM review_finding_event
            WHERE finding_id = NEW.finding_id
              AND event_kind = 'posted'
              AND external_link_id = NEW.external_link_id
       )
    THEN
        RAISE EXCEPTION
            'posted event reused consumed attachment evidence'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.event_kind = 'posted'
       AND NOT EXISTS (
           SELECT 1
             FROM review_external_link_attachment AS attachment
             JOIN review_external_link AS link
               ON link.external_link_id = attachment.external_link_id
              AND link.target_id = attachment.target_id
            WHERE attachment.external_link_id = NEW.external_link_id
              AND attachment.target_id = NEW.target_id
              AND attachment.pass_run_id = NEW.event_pass_run_id
              AND attachment.pass_id = NEW.event_pass_id
              AND link.object_kind IN (
                  'review',
                  'review_thread',
                  'review_comment',
                  'change_request_comment'
              )
       )
    THEN
        RAISE EXCEPTION
            'posted event pass did not produce its attachment'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: require_review_pass_external_result(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_review_pass_external_result() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.result_kind = 'finding_event'
       AND NOT EXISTS (
           SELECT 1
             FROM review_finding_event AS event
             JOIN review_finding AS finding
               ON finding.finding_id = event.finding_id
              AND finding.run_id = event.finding_run_id
              AND finding.target_id = event.target_id
            WHERE event.finding_id = NEW.result_finding_id
              AND event.finding_run_id = NEW.result_finding_run_id
              AND finding.producing_pass_id =
                    NEW.result_finding_pass_id
              AND event.target_id = NEW.target_id
              AND event.event_pass_id = NEW.pass_id
              AND event.event_pass_run_id = NEW.run_id
              AND event.event_ordinal = NEW.result_event_ordinal
              AND event.event_kind = NEW.result_event_kind
              AND event.reason IS NOT DISTINCT FROM NEW.result_reason
              AND event.referenced_finding_id IS NOT DISTINCT FROM
                    NEW.result_referenced_finding_id
              AND event.referenced_finding_run_id IS NOT DISTINCT FROM
                    NEW.result_referenced_finding_run_id
              AND event.referenced_finding_target_id IS NOT DISTINCT FROM
                    NEW.result_referenced_finding_target_id
              AND event.referenced_finding_pass_id IS NOT DISTINCT FROM
                    NEW.result_referenced_finding_pass_id
              AND event.referenced_finding_status IS NOT DISTINCT FROM
                    NEW.result_referenced_finding_status
              AND event.external_link_id IS NOT DISTINCT FROM
                    NEW.result_external_link_id
              AND event.external_link_association_kind IS NOT DISTINCT FROM
                    CASE
                        WHEN NEW.result_external_link_id IS NULL THEN NULL
                        ELSE 'finding'
                    END
       )
    THEN
        RAISE EXCEPTION
            'finding-event result omitted its exact child row'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.result_kind = 'external_link_attachment'
       AND NOT EXISTS (
           SELECT 1
             FROM review_external_link_attachment
            WHERE external_link_id = NEW.result_external_link_id
              AND target_id = NEW.target_id
              AND pass_run_id = NEW.run_id
              AND pass_id = NEW.pass_id
              AND external_object_key =
                    NEW.result_external_object_key
       )
    THEN
        RAISE EXCEPTION
            'external attachment result omitted its exact child row'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.result_kind = 'external_link_observation'
       AND NOT EXISTS (
           SELECT 1
             FROM review_external_link_observation
            WHERE external_link_id = NEW.result_external_link_id
              AND observation_ordinal = NEW.result_event_ordinal
              AND target_id = NEW.target_id
              AND pass_run_id = NEW.run_id
              AND pass_id = NEW.pass_id
              AND object_state =
                    NEW.result_observation_state
       )
    THEN
        RAISE EXCEPTION
            'external observation result omitted its exact child row'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.result_kind = 'external_link_no_change'
       AND (
           NOT EXISTS (
               SELECT 1
                 FROM review_external_link_attachment
                WHERE external_link_id = NEW.result_external_link_id
                  AND target_id = NEW.target_id
           )
           OR NEW.result_observation_state IS DISTINCT FROM (
               SELECT object_state
                 FROM review_external_link_observation
                WHERE external_link_id = NEW.result_external_link_id
                ORDER BY observation_ordinal DESC
                LIMIT 1
           )
           OR NEW.result_event_ordinal IS DISTINCT FROM (
               SELECT observation_ordinal
                 FROM review_external_link_observation
                WHERE external_link_id = NEW.result_external_link_id
                ORDER BY observation_ordinal DESC
                LIMIT 1
           )
       )
    THEN
        RAISE EXCEPTION
            'unchanged external result differs from latest durable state'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.result_kind = 'external_link_publication_blocked' THEN
        PERFORM 1
          FROM review_external_link
         WHERE external_link_id = NEW.result_external_link_id
         FOR NO KEY UPDATE;
    END IF;
    IF NEW.result_kind = 'external_link_publication_blocked'
       AND EXISTS (
           SELECT 1
             FROM review_external_link_attachment
            WHERE external_link_id = NEW.result_external_link_id
              AND target_id = NEW.target_id
       )
    THEN
        RAISE EXCEPTION
            'blocked publication result requires a pending reservation'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_review_pass_finding_inventory(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_review_pass_finding_inventory() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_pass uuid;
    canonical_kind text;
    sealed_count integer;
BEGIN
    checked_pass := COALESCE(
        (to_jsonb(NEW) ->> 'pass_id')::uuid,
        (to_jsonb(NEW) ->> 'producing_pass_id')::uuid
    );

    SELECT result_kind
      INTO canonical_kind
      FROM review_pass
     WHERE pass_id = checked_pass;
    SELECT finding_count
      INTO sealed_count
      FROM review_pass_finding_inventory_seal
     WHERE pass_id = checked_pass;

    IF canonical_kind IS DISTINCT FROM 'produced_findings'
       AND (
           EXISTS (
               SELECT 1
                 FROM review_finding
                WHERE producing_pass_id = checked_pass
           )
           OR EXISTS (
               SELECT 1
                 FROM review_pass_produced_finding
                WHERE pass_id = checked_pass
           )
           OR sealed_count IS NOT NULL
       )
    THEN
        RAISE EXCEPTION
            'review finding producer has no produced-findings result'
            USING ERRCODE = '23514';
    END IF;

    IF canonical_kind = 'produced_findings'
       AND (
           sealed_count IS NULL
           OR sealed_count IS DISTINCT FROM (
               SELECT count(*)
                 FROM review_finding
                WHERE producing_pass_id = checked_pass
           )
           OR sealed_count IS DISTINCT FROM (
               SELECT count(*)
                 FROM review_pass_produced_finding
                WHERE pass_id = checked_pass
           )
           OR
           EXISTS (
               SELECT finding_id, run_id, target_id, producing_pass_id
                 FROM review_finding
                WHERE producing_pass_id = checked_pass
               EXCEPT
               SELECT finding_id, finding_run_id, target_id, finding_pass_id
                 FROM review_pass_produced_finding
                WHERE pass_id = checked_pass
           )
           OR EXISTS (
               SELECT finding_id, finding_run_id, target_id, finding_pass_id
                 FROM review_pass_produced_finding
                WHERE pass_id = checked_pass
               EXCEPT
               SELECT finding_id, run_id, target_id, producing_pass_id
                 FROM review_finding
                WHERE producing_pass_id = checked_pass
           )
           OR EXISTS (
               SELECT 1
                 FROM (
                     SELECT result_ordinal,
                            row_number() OVER (
                                ORDER BY
                                    target_id,
                                    finding_run_id,
                                    finding_pass_id,
                                    finding_id
                            ) AS canonical_ordinal
                       FROM review_pass_produced_finding
                      WHERE pass_id = checked_pass
                 ) AS inventory
                WHERE result_ordinal <> canonical_ordinal
           )
       )
    THEN
        RAISE EXCEPTION
            'review pass finding inventory differs from canonical findings'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_review_pass_run_projection(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_review_pass_run_projection() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    canonical_run_state text;
    canonical_run_pass uuid;
BEGIN
    SELECT state_kind, state_pass_id
      INTO canonical_run_state, canonical_run_pass
      FROM review_run
     WHERE run_id = NEW.run_id
       AND target_id = NEW.target_id;

    IF NEW.state_kind = 'queued' THEN
        IF canonical_run_state IS DISTINCT FROM 'queued'
           OR canonical_run_pass IS NOT NULL
        THEN
            RAISE EXCEPTION 'queued pass is not under a queued run'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.state_kind = 'cancelled' AND NEW.turn_id IS NULL THEN
        IF canonical_run_state IS DISTINCT FROM 'cancelled'
           OR canonical_run_pass IS DISTINCT FROM NEW.pass_id
        THEN
            RAISE EXCEPTION
                'pre-start cancelled pass contradicts its run projection'
                USING ERRCODE = '23514';
        END IF;
    ELSIF canonical_run_pass IS DISTINCT FROM NEW.pass_id
          OR canonical_run_state IS DISTINCT FROM NEW.state_kind
    THEN
        RAISE EXCEPTION
            'review pass state contradicts its run projection'
            USING ERRCODE = '23514';
    END IF;

    RETURN NULL;
END;
$$;


--
-- Name: require_review_run_pass_projection(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_review_run_pass_projection() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    canonical_pass_id uuid;
    canonical_pass_state text;
    canonical_pass_turn uuid;
BEGIN
    SELECT pass_id, state_kind, turn_id
      INTO canonical_pass_id, canonical_pass_state, canonical_pass_turn
      FROM review_pass
     WHERE run_id = NEW.run_id
       AND target_id = NEW.target_id;

    IF NEW.state_kind = 'queued' THEN
        IF canonical_pass_id IS NOT NULL
           AND canonical_pass_state IS DISTINCT FROM 'queued'
        THEN
            RAISE EXCEPTION 'queued run has a non-queued pass'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.state_kind = 'cancelled' AND NEW.state_pass_id IS NULL THEN
        IF canonical_pass_id IS NOT NULL THEN
            RAISE EXCEPTION
                'cancelled run without pass projection has a recorded pass'
                USING ERRCODE = '23514';
        END IF;
    ELSIF canonical_pass_id IS DISTINCT FROM NEW.state_pass_id
          OR canonical_pass_state IS DISTINCT FROM NEW.state_kind
    THEN
        RAISE EXCEPTION
            'review run state contradicts its pass projection'
            USING ERRCODE = '23514';
    END IF;

    RETURN NULL;
END;
$$;


--
-- Tables.
--

--
-- Name: review_external_link; Type: TABLE; Schema: public
--

CREATE TABLE review_external_link (
    external_link_id uuid NOT NULL,
    target_id uuid NOT NULL,
    association_kind text NOT NULL,
    run_id uuid,
    finding_id uuid,
    finding_producing_pass_id uuid,
    provider_key text NOT NULL,
    object_kind text NOT NULL,
    CONSTRAINT review_external_link_association_closed CHECK ((association_kind = ANY (ARRAY['target'::text, 'run'::text, 'finding'::text]))),
    CONSTRAINT review_external_link_association_shape CHECK ((((association_kind = 'target'::text) AND (run_id IS NULL) AND (finding_id IS NULL) AND (finding_producing_pass_id IS NULL)) OR ((association_kind = 'run'::text) AND (run_id IS NOT NULL) AND (finding_id IS NULL) AND (finding_producing_pass_id IS NULL)) OR ((association_kind = 'finding'::text) AND (run_id IS NOT NULL) AND (finding_id IS NOT NULL) AND (finding_producing_pass_id IS NOT NULL)))),
    CONSTRAINT review_external_link_object_kind_closed CHECK ((object_kind = ANY (ARRAY['change_request'::text, 'commit'::text, 'review'::text, 'review_thread'::text, 'review_comment'::text, 'change_request_comment'::text]))),
    CONSTRAINT review_external_link_provider_bound CHECK (((octet_length(provider_key) >= 1) AND (octet_length(provider_key) <= 1024)))
);


--
-- Name: review_external_link_attachment; Type: TABLE; Schema: public
--

CREATE TABLE review_external_link_attachment (
    external_link_id uuid NOT NULL,
    target_id uuid NOT NULL,
    pass_run_id uuid NOT NULL,
    pass_id uuid NOT NULL,
    provider_key text NOT NULL,
    object_kind text NOT NULL,
    external_object_key text NOT NULL,
    identity_digest text GENERATED ALWAYS AS (md5(((((provider_key || chr(31)) || object_kind) || chr(31)) || external_object_key))) STORED,
    CONSTRAINT review_external_link_attachment_object_bound CHECK (((octet_length(external_object_key) >= 1) AND (octet_length(external_object_key) <= 1024)))
);


--
-- Name: review_external_link_observation; Type: TABLE; Schema: public
--

CREATE TABLE review_external_link_observation (
    external_link_id uuid NOT NULL,
    observation_ordinal bigint NOT NULL,
    target_id uuid NOT NULL,
    pass_run_id uuid NOT NULL,
    pass_id uuid NOT NULL,
    object_state text NOT NULL,
    CONSTRAINT review_external_link_observation_ordinal_positive_u32 CHECK (((observation_ordinal >= 1) AND (observation_ordinal <= '4294967295'::bigint))),
    CONSTRAINT review_external_link_observation_state_closed CHECK ((object_state = ANY (ARRAY['current'::text, 'outdated'::text, 'resolved'::text])))
);


--
-- Name: review_external_object_identity; Type: TABLE; Schema: public
--

CREATE TABLE review_external_object_identity (
    provider_key text NOT NULL,
    object_kind text NOT NULL,
    external_object_key text NOT NULL,
    logical_target_id uuid NOT NULL,
    identity_digest text GENERATED ALWAYS AS (md5(((((provider_key || chr(31)) || object_kind) || chr(31)) || external_object_key))) STORED,
    CONSTRAINT review_external_object_identity_key_bound CHECK (((octet_length(external_object_key) >= 1) AND (octet_length(external_object_key) <= 1024))),
    CONSTRAINT review_external_object_identity_kind_closed CHECK ((object_kind = ANY (ARRAY['change_request'::text, 'commit'::text, 'review'::text, 'review_thread'::text, 'review_comment'::text, 'change_request_comment'::text]))),
    CONSTRAINT review_external_object_identity_provider_bound CHECK (((octet_length(provider_key) >= 1) AND (octet_length(provider_key) <= 1024)))
);


--
-- Name: review_finding; Type: TABLE; Schema: public
--

CREATE TABLE review_finding (
    finding_id uuid NOT NULL,
    run_id uuid NOT NULL,
    target_id uuid NOT NULL,
    producing_pass_id uuid NOT NULL,
    file_path text NOT NULL,
    line_start bigint,
    line_end bigint,
    diff_side text,
    title text NOT NULL,
    body text NOT NULL,
    severity text NOT NULL,
    is_real_confidence integer CONSTRAINT review_finding_confidence_not_null NOT NULL,
    category text NOT NULL,
    recommended_fix text,
    severity_label_confidence integer NOT NULL,
    CONSTRAINT review_finding_diff_side_closed CHECK (((diff_side IS NULL) OR (diff_side = ANY (ARRAY['left'::text, 'right'::text])))),
    CONSTRAINT review_finding_is_real_confidence_bounds CHECK (((is_real_confidence >= 0) AND (is_real_confidence <= 10000))),
    CONSTRAINT review_finding_key_bounds CHECK (((octet_length(file_path) BETWEEN 1 AND 1024) AND (octet_length(category) BETWEEN 1 AND 1024))),
    CONSTRAINT review_finding_line_shape CHECK ((((line_start IS NULL) AND (line_end IS NULL)) OR ((line_start IS NOT NULL) AND (line_end IS NOT NULL) AND ((line_start >= 1) AND (line_start <= '4294967295'::bigint)) AND ((line_end >= line_start) AND (line_end <= '4294967295'::bigint))))),
    CONSTRAINT review_finding_severity_closed CHECK ((severity = ANY (ARRAY['info'::text, 'low'::text, 'medium'::text, 'high'::text, 'critical'::text]))),
    CONSTRAINT review_finding_severity_label_confidence_bounds CHECK (((severity_label_confidence >= 0) AND (severity_label_confidence <= 10000))),
    CONSTRAINT review_finding_text_bounds CHECK (((octet_length(title) BETWEEN 1 AND 65536) AND (octet_length(body) BETWEEN 1 AND 65536) AND ((recommended_fix IS NULL) OR (octet_length(recommended_fix) BETWEEN 1 AND 65536))))
);


--
-- Name: review_finding_event; Type: TABLE; Schema: public
--

CREATE TABLE review_finding_event (
    finding_id uuid NOT NULL,
    event_ordinal bigint NOT NULL,
    finding_run_id uuid NOT NULL,
    target_id uuid NOT NULL,
    event_pass_id uuid NOT NULL,
    event_pass_run_id uuid NOT NULL,
    event_kind text NOT NULL,
    reason text,
    referenced_finding_id uuid,
    referenced_finding_status text,
    external_link_id uuid,
    external_link_association_kind text,
    referenced_finding_run_id uuid,
    referenced_finding_target_id uuid,
    referenced_finding_pass_id uuid,
    CONSTRAINT review_finding_event_kind_closed CHECK ((event_kind = ANY (ARRAY['accepted'::text, 'rejected'::text, 'duplicate'::text, 'superseded'::text, 'stale'::text, 'posted'::text, 'fixed'::text, 'blocked_with_reason'::text]))),
    CONSTRAINT review_finding_event_ordinal_positive_u32 CHECK (((event_ordinal >= 1) AND (event_ordinal <= '4294967295'::bigint))),
    CONSTRAINT review_finding_event_referenced_ancestry_shape CHECK ((((referenced_finding_id IS NULL) AND (referenced_finding_run_id IS NULL) AND (referenced_finding_target_id IS NULL) AND (referenced_finding_pass_id IS NULL)) OR ((referenced_finding_id IS NOT NULL) AND (referenced_finding_run_id IS NOT NULL) AND (referenced_finding_target_id IS NOT NULL) AND (referenced_finding_pass_id IS NOT NULL) AND (referenced_finding_target_id = target_id)))),
    CONSTRAINT review_finding_event_shape CHECK ((((event_kind = ANY (ARRAY['accepted'::text, 'stale'::text, 'fixed'::text])) AND (reason IS NULL) AND (referenced_finding_id IS NULL) AND (referenced_finding_status IS NULL) AND (external_link_id IS NULL) AND (external_link_association_kind IS NULL)) OR ((event_kind = 'rejected'::text) AND (reason IS NOT NULL) AND ((octet_length(reason) >= 1) AND (octet_length(reason) <= 65536)) AND (referenced_finding_id IS NULL) AND (referenced_finding_status IS NULL) AND (external_link_id IS NULL) AND (external_link_association_kind IS NULL)) OR ((event_kind = 'blocked_with_reason'::text) AND (reason IS NOT NULL) AND ((octet_length(reason) >= 1) AND (octet_length(reason) <= 65536)) AND (referenced_finding_id IS NULL) AND (referenced_finding_status IS NULL) AND (((external_link_id IS NULL) AND (external_link_association_kind IS NULL)) OR ((external_link_id IS NOT NULL) AND (external_link_association_kind IS NOT NULL) AND (external_link_association_kind = 'finding'::text)))) OR ((event_kind = ANY (ARRAY['duplicate'::text, 'superseded'::text])) AND (reason IS NULL) AND (referenced_finding_id IS NOT NULL) AND (referenced_finding_status = ANY (ARRAY['open'::text, 'accepted'::text])) AND (referenced_finding_id <> finding_id) AND (external_link_id IS NULL) AND (external_link_association_kind IS NULL)) OR ((event_kind = 'posted'::text) AND (reason IS NULL) AND (referenced_finding_id IS NULL) AND (referenced_finding_status IS NULL) AND (external_link_id IS NOT NULL) AND (external_link_association_kind IS NOT NULL) AND (external_link_association_kind = 'finding'::text))))
);


--
-- Name: review_finding_event_head; Type: TABLE; Schema: public
--

CREATE TABLE review_finding_event_head (
    finding_id uuid NOT NULL,
    event_ordinal bigint,
    status text NOT NULL,
    event_pass_kind text,
    external_link_id uuid,
    CONSTRAINT review_finding_event_head_shape CHECK ((((event_ordinal IS NULL) AND (status = 'open'::text) AND (event_pass_kind IS NULL) AND (external_link_id IS NULL)) OR ((event_ordinal IS NOT NULL) AND ((event_ordinal >= 1) AND (event_ordinal <= '4294967295'::bigint)) AND (status <> 'open'::text) AND (event_pass_kind IS NOT NULL)))),
    CONSTRAINT review_finding_event_head_status_closed CHECK ((status = ANY (ARRAY['open'::text, 'accepted'::text, 'rejected'::text, 'duplicate'::text, 'superseded'::text, 'stale'::text, 'posted'::text, 'fixed'::text, 'blocked_with_reason'::text])))
);


--
-- Name: review_orchestration_attempt; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_attempt (
    attempt_id uuid NOT NULL,
    target_id uuid NOT NULL,
    policy_version bigint NOT NULL,
    minimum_judge_confidence integer NOT NULL,
    minimum_publication_confidence integer CONSTRAINT review_orchestration_attemp_minimum_publication_confid_not_null NOT NULL,
    concern_set_version text NOT NULL,
    import_template_digest bytea NOT NULL,
    judgment_template_digest bytea NOT NULL,
    repair_template_digest bytea NOT NULL,
    publication_template_digest bytea CONSTRAINT review_orchestration_attemp_publication_template_diges_not_null NOT NULL,
    CONSTRAINT review_orchestration_attempt_concern_set_version_check CHECK (((octet_length(concern_set_version) >= 1) AND (octet_length(concern_set_version) <= 1024))),
    CONSTRAINT review_orchestration_attempt_import_template_digest_check CHECK ((octet_length(import_template_digest) = 32)),
    CONSTRAINT review_orchestration_attempt_judgment_template_digest_check CHECK ((octet_length(judgment_template_digest) = 32)),
    CONSTRAINT review_orchestration_attempt_minimum_judge_confidence_check CHECK (((minimum_judge_confidence >= 0) AND (minimum_judge_confidence <= 10000))),
    CONSTRAINT review_orchestration_attempt_minimum_publication_confiden_check CHECK (((minimum_publication_confidence >= 0) AND (minimum_publication_confidence <= 10000))),
    CONSTRAINT review_orchestration_attempt_policy_version_check CHECK (((policy_version >= 1) AND (policy_version <= '4294967295'::bigint))),
    CONSTRAINT review_orchestration_attempt_publication_template_digest_check CHECK ((octet_length(publication_template_digest) = 32)),
    CONSTRAINT review_orchestration_attempt_repair_template_digest_check CHECK ((octet_length(repair_template_digest) = 32))
);


--
-- Name: review_orchestration_command; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_command (
    command_id uuid NOT NULL,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    semantic_digest bytea NOT NULL,
    attempt_id uuid NOT NULL,
    operation_kind text NOT NULL,
    result_stage text NOT NULL,
    import_recorded boolean NOT NULL,
    concern_claim_count bigint NOT NULL,
    fanout_sealed boolean NOT NULL,
    judgment_plan_sealed boolean NOT NULL,
    judgment_effect_count bigint NOT NULL,
    repair_inventory_count bigint,
    repair_outcomes_recorded boolean NOT NULL,
    publication_inventory_count bigint,
    publication_outcomes_recorded boolean CONSTRAINT review_orchestration_comman_publication_outcomes_recor_not_null NOT NULL,
    CONSTRAINT review_orchestration_command_command_kind_check CHECK ((command_kind = 'review_orchestration'::text)),
    CONSTRAINT review_orchestration_command_concern_claim_count_check CHECK ((concern_claim_count >= 0)),
    CONSTRAINT review_orchestration_command_judgment_effect_count_check CHECK ((judgment_effect_count >= 0)),
    CONSTRAINT review_orchestration_command_operation_kind_check CHECK ((operation_kind = ANY (ARRAY['start'::text, 'import'::text, 'concern'::text, 'judgment_plan'::text, 'judgment_effect'::text, 'repair'::text, 'publication'::text]))),
    CONSTRAINT review_orchestration_command_operation_result CHECK ((((operation_kind = 'start'::text) AND (result_stage = 'started'::text)) OR ((operation_kind = 'import'::text) AND (result_stage = ANY (ARRAY['import_incomplete'::text, 'awaiting_concerns'::text]))) OR ((operation_kind = 'concern'::text) AND (result_stage = ANY (ARRAY['awaiting_concerns'::text, 'fanout_incomplete'::text, 'awaiting_judgment'::text]))) OR ((operation_kind = 'judgment_plan'::text) AND (result_stage = ANY (ARRAY['awaiting_judgment_effects'::text, 'awaiting_repair'::text]))) OR ((operation_kind = 'judgment_effect'::text) AND (result_stage = ANY (ARRAY['awaiting_judgment_effects'::text, 'judgment_incomplete'::text, 'awaiting_repair'::text]))) OR ((operation_kind = 'repair'::text) AND (result_stage = ANY (ARRAY['repair_incomplete'::text, 'awaiting_publication'::text]))) OR ((operation_kind = 'publication'::text) AND (result_stage = ANY (ARRAY['publication_incomplete'::text, 'complete'::text]))))),
    CONSTRAINT review_orchestration_command_result_progress_shape CHECK ((((result_stage = ANY (ARRAY['started'::text, 'awaiting_import'::text])) AND (NOT import_recorded) AND (concern_claim_count = 0) AND (NOT fanout_sealed) AND (NOT judgment_plan_sealed) AND (judgment_effect_count = 0) AND (repair_inventory_count IS NULL) AND (NOT repair_outcomes_recorded) AND (publication_inventory_count IS NULL) AND (NOT publication_outcomes_recorded)) OR ((result_stage = 'import_incomplete'::text) AND import_recorded AND (concern_claim_count = 0) AND (NOT fanout_sealed) AND (NOT judgment_plan_sealed) AND (judgment_effect_count = 0) AND (repair_inventory_count IS NULL) AND (NOT repair_outcomes_recorded) AND (publication_inventory_count IS NULL) AND (NOT publication_outcomes_recorded)) OR ((result_stage = 'awaiting_concerns'::text) AND import_recorded AND (NOT fanout_sealed) AND (NOT judgment_plan_sealed) AND (judgment_effect_count = 0) AND (repair_inventory_count IS NULL) AND (NOT repair_outcomes_recorded) AND (publication_inventory_count IS NULL) AND (NOT publication_outcomes_recorded)) OR ((result_stage = 'fanout_incomplete'::text) AND import_recorded AND (concern_claim_count > 0) AND (NOT fanout_sealed) AND (NOT judgment_plan_sealed) AND (judgment_effect_count = 0) AND (repair_inventory_count IS NULL) AND (NOT repair_outcomes_recorded) AND (publication_inventory_count IS NULL) AND (NOT publication_outcomes_recorded)) OR ((result_stage = 'awaiting_judgment'::text) AND import_recorded AND (concern_claim_count > 0) AND fanout_sealed AND (NOT judgment_plan_sealed) AND (judgment_effect_count = 0) AND (repair_inventory_count IS NULL) AND (NOT repair_outcomes_recorded) AND (publication_inventory_count IS NULL) AND (NOT publication_outcomes_recorded)) OR ((result_stage = ANY (ARRAY['awaiting_judgment_effects'::text, 'judgment_incomplete'::text])) AND import_recorded AND (concern_claim_count > 0) AND fanout_sealed AND judgment_plan_sealed AND (repair_inventory_count IS NULL) AND (NOT repair_outcomes_recorded) AND (publication_inventory_count IS NULL) AND (NOT publication_outcomes_recorded)) OR ((result_stage = 'awaiting_repair'::text) AND import_recorded AND (concern_claim_count > 0) AND fanout_sealed AND judgment_plan_sealed AND (repair_inventory_count IS NOT NULL) AND (repair_inventory_count >= 0) AND (NOT repair_outcomes_recorded) AND (publication_inventory_count IS NULL) AND (NOT publication_outcomes_recorded)) OR ((result_stage = 'repair_incomplete'::text) AND import_recorded AND (concern_claim_count > 0) AND fanout_sealed AND judgment_plan_sealed AND (repair_inventory_count IS NOT NULL) AND (repair_inventory_count >= 0) AND repair_outcomes_recorded AND (publication_inventory_count IS NULL) AND (NOT publication_outcomes_recorded)) OR ((result_stage = 'awaiting_publication'::text) AND import_recorded AND (concern_claim_count > 0) AND fanout_sealed AND judgment_plan_sealed AND (repair_inventory_count IS NOT NULL) AND (repair_inventory_count >= 0) AND repair_outcomes_recorded AND (publication_inventory_count IS NOT NULL) AND (publication_inventory_count >= 0) AND (NOT publication_outcomes_recorded)) OR ((result_stage = ANY (ARRAY['publication_incomplete'::text, 'complete'::text])) AND import_recorded AND (concern_claim_count > 0) AND fanout_sealed AND judgment_plan_sealed AND (repair_inventory_count IS NOT NULL) AND (repair_inventory_count >= 0) AND repair_outcomes_recorded AND (publication_inventory_count IS NOT NULL) AND (publication_inventory_count >= 0) AND publication_outcomes_recorded))),
    CONSTRAINT review_orchestration_command_result_stage_check CHECK ((result_stage = ANY (ARRAY['started'::text, 'awaiting_import'::text, 'import_incomplete'::text, 'awaiting_concerns'::text, 'fanout_incomplete'::text, 'awaiting_judgment'::text, 'awaiting_judgment_effects'::text, 'judgment_incomplete'::text, 'awaiting_repair'::text, 'repair_incomplete'::text, 'awaiting_publication'::text, 'publication_incomplete'::text, 'complete'::text]))),
    CONSTRAINT review_orchestration_command_semantic_digest_check CHECK ((octet_length(semantic_digest) = 32)),
    CONSTRAINT review_orchestration_command_storage_version_check CHECK ((storage_version = 1))
);


--
-- Name: review_orchestration_command_effect; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_command_effect (
    command_id uuid NOT NULL,
    attempt_id uuid NOT NULL,
    operation_kind text NOT NULL,
    concern_effect_sequence bigint,
    CONSTRAINT review_orchestration_command_effect_check CHECK (((operation_kind = 'concern'::text) = (concern_effect_sequence IS NOT NULL))),
    CONSTRAINT review_orchestration_command_effect_operation_kind_check CHECK ((operation_kind = ANY (ARRAY['start'::text, 'import'::text, 'concern'::text, 'judgment_plan'::text, 'judgment_effect'::text, 'repair'::text, 'publication'::text])))
);


--
-- Name: review_orchestration_command_intent; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_command_intent (
    command_id uuid NOT NULL,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    semantic_digest bytea NOT NULL,
    attempt_id uuid NOT NULL,
    operation_kind text NOT NULL,
    CONSTRAINT review_orchestration_command_intent_command_kind_check CHECK ((command_kind = 'review_orchestration'::text)),
    CONSTRAINT review_orchestration_command_intent_operation_kind_check CHECK ((operation_kind = ANY (ARRAY['start'::text, 'import'::text, 'concern'::text, 'judgment_plan'::text, 'judgment_effect'::text, 'repair'::text, 'publication'::text]))),
    CONSTRAINT review_orchestration_command_intent_semantic_digest_check CHECK ((octet_length(semantic_digest) = 32)),
    CONSTRAINT review_orchestration_command_intent_storage_version_check CHECK ((storage_version = 1))
);


--
-- Name: review_orchestration_command_recovery; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_command_recovery (
    command_id uuid NOT NULL,
    semantic_digest bytea NOT NULL,
    attempt_id uuid NOT NULL,
    operation_kind text NOT NULL,
    result_stage text NOT NULL,
    import_recorded boolean NOT NULL,
    concern_claim_count bigint CONSTRAINT review_orchestration_command_recov_concern_claim_count_not_null NOT NULL,
    fanout_sealed boolean NOT NULL,
    judgment_plan_sealed boolean CONSTRAINT review_orchestration_command_reco_judgment_plan_sealed_not_null NOT NULL,
    judgment_effect_count bigint CONSTRAINT review_orchestration_command_rec_judgment_effect_count_not_null NOT NULL,
    repair_inventory_count bigint,
    repair_outcomes_recorded boolean CONSTRAINT review_orchestration_command__repair_outcomes_recorded_not_null NOT NULL,
    publication_inventory_count bigint,
    publication_outcomes_recorded boolean CONSTRAINT review_orchestration_comma_publication_outcomes_recor_not_null1 NOT NULL,
    CONSTRAINT review_orchestration_command_recove_judgment_effect_count_check CHECK ((judgment_effect_count >= 0)),
    CONSTRAINT review_orchestration_command_recovery_concern_claim_count_check CHECK ((concern_claim_count >= 0)),
    CONSTRAINT review_orchestration_command_recovery_operation_kind_check CHECK ((operation_kind = ANY (ARRAY['start'::text, 'import'::text, 'concern'::text, 'judgment_plan'::text, 'judgment_effect'::text, 'repair'::text, 'publication'::text]))),
    CONSTRAINT review_orchestration_command_recovery_operation_result CHECK ((((operation_kind = 'start'::text) AND (result_stage = 'started'::text)) OR ((operation_kind = 'import'::text) AND (result_stage = ANY (ARRAY['import_incomplete'::text, 'awaiting_concerns'::text]))) OR ((operation_kind = 'concern'::text) AND (result_stage = ANY (ARRAY['awaiting_concerns'::text, 'fanout_incomplete'::text, 'awaiting_judgment'::text]))) OR ((operation_kind = 'judgment_plan'::text) AND (result_stage = ANY (ARRAY['awaiting_judgment_effects'::text, 'awaiting_repair'::text]))) OR ((operation_kind = 'judgment_effect'::text) AND (result_stage = ANY (ARRAY['awaiting_judgment_effects'::text, 'judgment_incomplete'::text, 'awaiting_repair'::text]))) OR ((operation_kind = 'repair'::text) AND (result_stage = ANY (ARRAY['repair_incomplete'::text, 'awaiting_publication'::text]))) OR ((operation_kind = 'publication'::text) AND (result_stage = ANY (ARRAY['publication_incomplete'::text, 'complete'::text]))))),
    CONSTRAINT review_orchestration_command_recovery_result_progress_shape CHECK ((((result_stage = ANY (ARRAY['started'::text, 'awaiting_import'::text])) AND (NOT import_recorded) AND (concern_claim_count = 0) AND (NOT fanout_sealed) AND (NOT judgment_plan_sealed) AND (judgment_effect_count = 0) AND (repair_inventory_count IS NULL) AND (NOT repair_outcomes_recorded) AND (publication_inventory_count IS NULL) AND (NOT publication_outcomes_recorded)) OR ((result_stage = 'import_incomplete'::text) AND import_recorded AND (concern_claim_count = 0) AND (NOT fanout_sealed) AND (NOT judgment_plan_sealed) AND (judgment_effect_count = 0) AND (repair_inventory_count IS NULL) AND (NOT repair_outcomes_recorded) AND (publication_inventory_count IS NULL) AND (NOT publication_outcomes_recorded)) OR ((result_stage = 'awaiting_concerns'::text) AND import_recorded AND (NOT fanout_sealed) AND (NOT judgment_plan_sealed) AND (judgment_effect_count = 0) AND (repair_inventory_count IS NULL) AND (NOT repair_outcomes_recorded) AND (publication_inventory_count IS NULL) AND (NOT publication_outcomes_recorded)) OR ((result_stage = 'fanout_incomplete'::text) AND import_recorded AND (concern_claim_count > 0) AND (NOT fanout_sealed) AND (NOT judgment_plan_sealed) AND (judgment_effect_count = 0) AND (repair_inventory_count IS NULL) AND (NOT repair_outcomes_recorded) AND (publication_inventory_count IS NULL) AND (NOT publication_outcomes_recorded)) OR ((result_stage = 'awaiting_judgment'::text) AND import_recorded AND (concern_claim_count > 0) AND fanout_sealed AND (NOT judgment_plan_sealed) AND (judgment_effect_count = 0) AND (repair_inventory_count IS NULL) AND (NOT repair_outcomes_recorded) AND (publication_inventory_count IS NULL) AND (NOT publication_outcomes_recorded)) OR ((result_stage = ANY (ARRAY['awaiting_judgment_effects'::text, 'judgment_incomplete'::text])) AND import_recorded AND (concern_claim_count > 0) AND fanout_sealed AND judgment_plan_sealed AND (repair_inventory_count IS NULL) AND (NOT repair_outcomes_recorded) AND (publication_inventory_count IS NULL) AND (NOT publication_outcomes_recorded)) OR ((result_stage = 'awaiting_repair'::text) AND import_recorded AND (concern_claim_count > 0) AND fanout_sealed AND judgment_plan_sealed AND (repair_inventory_count IS NOT NULL) AND (repair_inventory_count >= 0) AND (NOT repair_outcomes_recorded) AND (publication_inventory_count IS NULL) AND (NOT publication_outcomes_recorded)) OR ((result_stage = 'repair_incomplete'::text) AND import_recorded AND (concern_claim_count > 0) AND fanout_sealed AND judgment_plan_sealed AND (repair_inventory_count IS NOT NULL) AND (repair_inventory_count >= 0) AND repair_outcomes_recorded AND (publication_inventory_count IS NULL) AND (NOT publication_outcomes_recorded)) OR ((result_stage = 'awaiting_publication'::text) AND import_recorded AND (concern_claim_count > 0) AND fanout_sealed AND judgment_plan_sealed AND (repair_inventory_count IS NOT NULL) AND (repair_inventory_count >= 0) AND repair_outcomes_recorded AND (publication_inventory_count IS NOT NULL) AND (publication_inventory_count >= 0) AND (NOT publication_outcomes_recorded)) OR ((result_stage = ANY (ARRAY['publication_incomplete'::text, 'complete'::text])) AND import_recorded AND (concern_claim_count > 0) AND fanout_sealed AND judgment_plan_sealed AND (repair_inventory_count IS NOT NULL) AND (repair_inventory_count >= 0) AND repair_outcomes_recorded AND (publication_inventory_count IS NOT NULL) AND (publication_inventory_count >= 0) AND publication_outcomes_recorded))),
    CONSTRAINT review_orchestration_command_recovery_result_stage_check CHECK ((result_stage = ANY (ARRAY['started'::text, 'awaiting_import'::text, 'import_incomplete'::text, 'awaiting_concerns'::text, 'fanout_incomplete'::text, 'awaiting_judgment'::text, 'awaiting_judgment_effects'::text, 'judgment_incomplete'::text, 'awaiting_repair'::text, 'repair_incomplete'::text, 'awaiting_publication'::text, 'publication_incomplete'::text, 'complete'::text]))),
    CONSTRAINT review_orchestration_command_recovery_semantic_digest_check CHECK ((octet_length(semantic_digest) = 32))
);


--
-- Name: review_orchestration_concern; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_concern (
    attempt_id uuid NOT NULL,
    concern_ordinal integer NOT NULL,
    concern_key text NOT NULL,
    template_digest bytea NOT NULL,
    CONSTRAINT review_orchestration_concern_concern_key_check CHECK (((octet_length(concern_key) >= 1) AND (octet_length(concern_key) <= 1024))),
    CONSTRAINT review_orchestration_concern_concern_ordinal_check CHECK ((concern_ordinal >= 0)),
    CONSTRAINT review_orchestration_concern_template_digest_check CHECK ((octet_length(template_digest) = 32))
);


--
-- Name: review_orchestration_concern_claim; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_concern_claim (
    attempt_id uuid NOT NULL,
    concern_key text NOT NULL,
    claim_ordinal integer NOT NULL,
    template_digest bytea NOT NULL,
    outcome_kind text NOT NULL,
    pass_id uuid,
    effect_sequence bigint NOT NULL,
    CONSTRAINT review_orchestration_concern_claim_check CHECK (((outcome_kind = 'cancelled'::text) OR (pass_id IS NOT NULL))),
    CONSTRAINT review_orchestration_concern_claim_claim_ordinal_check CHECK ((claim_ordinal >= 0)),
    CONSTRAINT review_orchestration_concern_claim_outcome_kind_check CHECK ((outcome_kind = ANY (ARRAY['succeeded'::text, 'failed'::text, 'blocked'::text, 'cancelled'::text, 'superseded'::text]))),
    CONSTRAINT review_orchestration_concern_claim_template_digest_check CHECK ((octet_length(template_digest) = 32))
);


--
-- Name: review_orchestration_concern_claim_effect_sequence_seq; Type: SEQUENCE; Schema: public
--

ALTER TABLE review_orchestration_concern_claim ALTER COLUMN effect_sequence ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME review_orchestration_concern_claim_effect_sequence_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: review_orchestration_concern_finding; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_concern_finding (
    attempt_id uuid NOT NULL,
    concern_key text NOT NULL,
    claim_ordinal integer NOT NULL,
    finding_ordinal integer NOT NULL,
    finding_id uuid NOT NULL,
    CONSTRAINT review_orchestration_concern_finding_finding_ordinal_check CHECK ((finding_ordinal >= 0))
);


--
-- Name: review_orchestration_fanout_member; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_fanout_member (
    attempt_id uuid NOT NULL,
    member_ordinal integer NOT NULL,
    concern_key text NOT NULL,
    claim_ordinal integer NOT NULL,
    CONSTRAINT review_orchestration_fanout_member_member_ordinal_check CHECK ((member_ordinal >= 0))
);


--
-- Name: review_orchestration_fanout_seal; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_fanout_seal (
    attempt_id uuid NOT NULL
);


--
-- Name: review_orchestration_import; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_import (
    attempt_id uuid NOT NULL,
    outcome_kind text NOT NULL,
    pass_id uuid,
    external_link_id uuid,
    template_digest bytea NOT NULL,
    context_digest bytea,
    CONSTRAINT review_orchestration_import_check CHECK ((((outcome_kind = 'succeeded'::text) AND (pass_id IS NOT NULL) AND (context_digest IS NOT NULL)) OR ((outcome_kind = ANY (ARRAY['failed'::text, 'blocked'::text])) AND (pass_id IS NOT NULL) AND (external_link_id IS NULL) AND (context_digest IS NULL)) OR ((outcome_kind = 'cancelled'::text) AND (external_link_id IS NULL) AND (context_digest IS NULL)))),
    CONSTRAINT review_orchestration_import_context_digest_check CHECK (((context_digest IS NULL) OR (octet_length(context_digest) = 32))),
    CONSTRAINT review_orchestration_import_outcome_kind_check CHECK ((outcome_kind = ANY (ARRAY['succeeded'::text, 'failed'::text, 'blocked'::text, 'cancelled'::text]))),
    CONSTRAINT review_orchestration_import_template_digest_check CHECK ((octet_length(template_digest) = 32))
);


--
-- Name: review_orchestration_judgment_effect; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_judgment_effect (
    attempt_id uuid NOT NULL,
    effect_ordinal integer NOT NULL,
    finding_id uuid NOT NULL,
    CONSTRAINT review_orchestration_judgment_effect_effect_ordinal_check CHECK ((effect_ordinal >= 0))
);


--
-- Name: review_orchestration_judgment_member; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_judgment_member (
    attempt_id uuid NOT NULL,
    member_ordinal integer NOT NULL,
    finding_id uuid NOT NULL,
    finding_run_id uuid NOT NULL,
    finding_pass_id uuid NOT NULL,
    disposition_kind text NOT NULL,
    reason text,
    referenced_finding_id uuid,
    referenced_run_id uuid,
    referenced_pass_id uuid,
    CONSTRAINT review_orchestration_judgment_member_check CHECK ((((disposition_kind = 'rejected'::text) AND (reason IS NOT NULL) AND (referenced_finding_id IS NULL) AND (referenced_run_id IS NULL) AND (referenced_pass_id IS NULL)) OR ((disposition_kind = ANY (ARRAY['duplicate'::text, 'superseded'::text])) AND (reason IS NULL) AND (referenced_finding_id IS NOT NULL) AND (referenced_run_id IS NOT NULL) AND (referenced_pass_id IS NOT NULL)) OR ((disposition_kind = ANY (ARRAY['accepted'::text, 'stale'::text])) AND (reason IS NULL) AND (referenced_finding_id IS NULL) AND (referenced_run_id IS NULL) AND (referenced_pass_id IS NULL)))),
    CONSTRAINT review_orchestration_judgment_member_disposition_kind_check CHECK ((disposition_kind = ANY (ARRAY['accepted'::text, 'rejected'::text, 'duplicate'::text, 'superseded'::text, 'stale'::text]))),
    CONSTRAINT review_orchestration_judgment_member_member_ordinal_check CHECK ((member_ordinal >= 0))
);


--
-- Name: review_orchestration_judgment_plan; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_judgment_plan (
    attempt_id uuid NOT NULL,
    analysis_pass_id uuid NOT NULL,
    template_digest bytea NOT NULL,
    CONSTRAINT review_orchestration_judgment_plan_template_digest_check CHECK ((octet_length(template_digest) = 32))
);


--
-- Name: review_orchestration_publication_inventory; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_publication_inventory (
    attempt_id uuid NOT NULL,
    member_ordinal integer CONSTRAINT review_orchestration_publication_invent_member_ordinal_not_null NOT NULL,
    finding_id uuid NOT NULL,
    finding_run_id uuid CONSTRAINT review_orchestration_publication_invent_finding_run_id_not_null NOT NULL,
    finding_pass_id uuid CONSTRAINT review_orchestration_publication_inven_finding_pass_id_not_null NOT NULL,
    CONSTRAINT review_orchestration_publication_inventory_member_ordinal_check CHECK ((member_ordinal >= 0))
);


--
-- Name: review_orchestration_publication_inventory_seal; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_publication_inventory_seal (
    attempt_id uuid CONSTRAINT review_orchestration_publication_inventory__attempt_id_not_null NOT NULL
);


--
-- Name: review_orchestration_publication_outcome; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_publication_outcome (
    attempt_id uuid NOT NULL,
    member_ordinal integer CONSTRAINT review_orchestration_publication_outcom_member_ordinal_not_null NOT NULL,
    finding_id uuid NOT NULL,
    outcome_kind text NOT NULL,
    external_link_id uuid,
    template_digest bytea,
    CONSTRAINT review_orchestration_publication_outcome_check CHECK ((((outcome_kind = 'published'::text) AND (external_link_id IS NOT NULL) AND (template_digest IS NOT NULL) AND (octet_length(template_digest) = 32)) OR ((outcome_kind <> 'published'::text) AND (external_link_id IS NULL) AND (template_digest IS NULL)))),
    CONSTRAINT review_orchestration_publication_outcome_member_ordinal_check CHECK ((member_ordinal >= 0)),
    CONSTRAINT review_orchestration_publication_outcome_outcome_kind_check CHECK ((outcome_kind = ANY (ARRAY['published'::text, 'failed'::text, 'blocked'::text, 'cancelled'::text])))
);


--
-- Name: review_orchestration_publication_outcome_seal; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_publication_outcome_seal (
    attempt_id uuid CONSTRAINT review_orchestration_publication_outcome_se_attempt_id_not_null NOT NULL
);


--
-- Name: review_orchestration_repair_inventory; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_repair_inventory (
    attempt_id uuid NOT NULL,
    member_ordinal integer NOT NULL,
    finding_id uuid NOT NULL,
    finding_run_id uuid NOT NULL,
    finding_pass_id uuid NOT NULL,
    CONSTRAINT review_orchestration_repair_inventory_member_ordinal_check CHECK ((member_ordinal >= 0))
);


--
-- Name: review_orchestration_repair_inventory_seal; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_repair_inventory_seal (
    attempt_id uuid NOT NULL
);


--
-- Name: review_orchestration_repair_outcome; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_repair_outcome (
    attempt_id uuid NOT NULL,
    member_ordinal integer NOT NULL,
    finding_id uuid NOT NULL,
    outcome_kind text NOT NULL,
    event_ordinal bigint,
    template_digest bytea,
    CONSTRAINT review_orchestration_repair_outcome_check CHECK ((((outcome_kind = 'fixed'::text) AND (event_ordinal IS NOT NULL) AND (template_digest IS NOT NULL) AND (octet_length(template_digest) = 32)) OR ((outcome_kind <> 'fixed'::text) AND (event_ordinal IS NULL) AND (template_digest IS NULL)))),
    CONSTRAINT review_orchestration_repair_outcome_event_ordinal_check CHECK (((event_ordinal IS NULL) OR ((event_ordinal >= 1) AND (event_ordinal <= '4294967295'::bigint)))),
    CONSTRAINT review_orchestration_repair_outcome_member_ordinal_check CHECK ((member_ordinal >= 0)),
    CONSTRAINT review_orchestration_repair_outcome_outcome_kind_check CHECK ((outcome_kind = ANY (ARRAY['fixed'::text, 'failed'::text, 'blocked'::text, 'cancelled'::text])))
);


--
-- Name: review_orchestration_repair_outcome_seal; Type: TABLE; Schema: public
--

CREATE TABLE review_orchestration_repair_outcome_seal (
    attempt_id uuid NOT NULL
);


--
-- Name: review_pass; Type: TABLE; Schema: public
--

CREATE TABLE review_pass (
    pass_id uuid NOT NULL,
    run_id uuid NOT NULL,
    target_id uuid NOT NULL,
    pass_kind text NOT NULL,
    session_id uuid NOT NULL,
    accepted_input_id uuid NOT NULL,
    origin_turn_id uuid NOT NULL,
    state_kind text NOT NULL,
    turn_id uuid,
    output_frontier_id uuid,
    result_kind text,
    result_finding_id uuid,
    result_finding_run_id uuid,
    result_finding_pass_id uuid,
    result_event_ordinal bigint,
    result_event_kind text,
    result_reason text,
    result_referenced_finding_id uuid,
    result_referenced_finding_run_id uuid,
    result_referenced_finding_pass_id uuid,
    result_referenced_finding_status text,
    result_external_link_id uuid,
    result_external_object_key text,
    result_observation_state text,
    result_referenced_finding_target_id uuid,
    CONSTRAINT review_pass_kind_closed CHECK ((pass_kind = ANY (ARRAY['import_external_context'::text, 'read_only_review'::text, 'judge'::text, 'dedupe'::text, 'publish'::text, 'fix'::text, 'propagate_stack'::text]))),
    CONSTRAINT review_pass_result_event_kind_closed CHECK (((result_event_kind IS NULL) OR (result_event_kind = ANY (ARRAY['accepted'::text, 'rejected'::text, 'duplicate'::text, 'superseded'::text, 'stale'::text, 'posted'::text, 'fixed'::text, 'blocked_with_reason'::text])))),
    CONSTRAINT review_pass_result_event_ordinal_positive_u32 CHECK (((result_event_ordinal IS NULL) OR ((result_event_ordinal >= 1) AND (result_event_ordinal <= '4294967295'::bigint)))),
    CONSTRAINT review_pass_result_kind_closed CHECK (((result_kind IS NULL) OR (result_kind = ANY (ARRAY['produced_findings'::text, 'finding_event'::text, 'external_link_attachment'::text, 'external_link_observation'::text, 'external_link_no_change'::text, 'external_link_publication_blocked'::text])))),
    CONSTRAINT review_pass_result_observation_state_closed CHECK (((result_observation_state IS NULL) OR (result_observation_state = ANY (ARRAY['current'::text, 'outdated'::text, 'resolved'::text])))),
    CONSTRAINT review_pass_result_reference_status_closed CHECK (((result_referenced_finding_status IS NULL) OR (result_referenced_finding_status = ANY (ARRAY['open'::text, 'accepted'::text])))),
    CONSTRAINT review_pass_result_referenced_target_shape CHECK ((((result_referenced_finding_id IS NULL) AND (result_referenced_finding_target_id IS NULL)) OR ((result_referenced_finding_id IS NOT NULL) AND (result_referenced_finding_target_id IS NOT NULL) AND (result_referenced_finding_target_id = target_id)))),
    CONSTRAINT review_pass_result_shape CHECK ((((result_kind IS NULL) AND (result_finding_id IS NULL) AND (result_finding_run_id IS NULL) AND (result_finding_pass_id IS NULL) AND (result_event_ordinal IS NULL) AND (result_event_kind IS NULL) AND (result_reason IS NULL) AND (result_referenced_finding_id IS NULL) AND (result_referenced_finding_run_id IS NULL) AND (result_referenced_finding_pass_id IS NULL) AND (result_referenced_finding_status IS NULL) AND (result_external_link_id IS NULL) AND (result_external_object_key IS NULL) AND (result_observation_state IS NULL)) OR ((result_kind = 'produced_findings'::text) AND (result_finding_id IS NULL) AND (result_finding_run_id IS NULL) AND (result_finding_pass_id IS NULL) AND (result_event_ordinal IS NULL) AND (result_event_kind IS NULL) AND (result_reason IS NULL) AND (result_referenced_finding_id IS NULL) AND (result_referenced_finding_run_id IS NULL) AND (result_referenced_finding_pass_id IS NULL) AND (result_referenced_finding_status IS NULL) AND (result_external_link_id IS NULL) AND (result_external_object_key IS NULL) AND (result_observation_state IS NULL)) OR ((result_kind = 'finding_event'::text) AND (result_finding_id IS NOT NULL) AND (result_finding_run_id IS NOT NULL) AND (result_finding_pass_id IS NOT NULL) AND (result_event_ordinal IS NOT NULL) AND (result_event_kind IS NOT NULL) AND (result_event_kind <> 'posted'::text) AND (result_external_object_key IS NULL) AND (result_observation_state IS NULL) AND (((result_event_kind = ANY (ARRAY['accepted'::text, 'stale'::text, 'fixed'::text])) AND (result_reason IS NULL) AND (result_referenced_finding_id IS NULL) AND (result_referenced_finding_run_id IS NULL) AND (result_referenced_finding_pass_id IS NULL) AND (result_referenced_finding_status IS NULL) AND (result_external_link_id IS NULL)) OR ((result_event_kind = 'rejected'::text) AND (result_reason IS NOT NULL) AND (result_referenced_finding_id IS NULL) AND (result_referenced_finding_run_id IS NULL) AND (result_referenced_finding_pass_id IS NULL) AND (result_referenced_finding_status IS NULL) AND (result_external_link_id IS NULL)) OR ((result_event_kind = 'blocked_with_reason'::text) AND (result_reason IS NOT NULL) AND (result_referenced_finding_id IS NULL) AND (result_referenced_finding_run_id IS NULL) AND (result_referenced_finding_pass_id IS NULL) AND (result_referenced_finding_status IS NULL)) OR ((result_event_kind = ANY (ARRAY['duplicate'::text, 'superseded'::text])) AND (result_reason IS NULL) AND (result_referenced_finding_id IS NOT NULL) AND (result_referenced_finding_run_id IS NOT NULL) AND (result_referenced_finding_pass_id IS NOT NULL) AND (result_referenced_finding_status IS NOT NULL) AND (result_referenced_finding_status = ANY (ARRAY['open'::text, 'accepted'::text])) AND (result_external_link_id IS NULL)))) OR ((result_kind = 'external_link_attachment'::text) AND (result_external_link_id IS NOT NULL) AND (result_external_object_key IS NOT NULL) AND (result_observation_state IS NULL) AND (((result_finding_id IS NULL) AND (result_finding_run_id IS NULL) AND (result_finding_pass_id IS NULL) AND (result_event_ordinal IS NULL) AND (result_event_kind IS NULL) AND (result_reason IS NULL) AND (result_referenced_finding_id IS NULL) AND (result_referenced_finding_run_id IS NULL) AND (result_referenced_finding_pass_id IS NULL) AND (result_referenced_finding_status IS NULL)) OR ((result_finding_id IS NOT NULL) AND (result_finding_run_id IS NOT NULL) AND (result_finding_pass_id IS NOT NULL) AND (result_event_ordinal IS NOT NULL) AND (result_event_kind IS NOT NULL) AND (result_event_kind = 'posted'::text) AND (result_reason IS NULL) AND (result_referenced_finding_id IS NULL) AND (result_referenced_finding_run_id IS NULL) AND (result_referenced_finding_pass_id IS NULL) AND (result_referenced_finding_status IS NULL)))) OR ((result_kind = 'external_link_observation'::text) AND (result_finding_id IS NULL) AND (result_finding_run_id IS NULL) AND (result_finding_pass_id IS NULL) AND (result_event_ordinal IS NOT NULL) AND (result_event_kind IS NULL) AND (result_reason IS NULL) AND (result_referenced_finding_id IS NULL) AND (result_referenced_finding_run_id IS NULL) AND (result_referenced_finding_pass_id IS NULL) AND (result_referenced_finding_status IS NULL) AND (result_external_link_id IS NOT NULL) AND (result_external_object_key IS NULL) AND (result_observation_state IS NOT NULL)) OR ((result_kind = 'external_link_no_change'::text) AND (result_finding_id IS NULL) AND (result_finding_run_id IS NULL) AND (result_finding_pass_id IS NULL) AND (result_event_ordinal IS NOT NULL) AND (result_event_kind IS NULL) AND (result_reason IS NULL) AND (result_referenced_finding_id IS NULL) AND (result_referenced_finding_run_id IS NULL) AND (result_referenced_finding_pass_id IS NULL) AND (result_referenced_finding_status IS NULL) AND (result_external_link_id IS NOT NULL) AND (result_external_object_key IS NULL) AND (result_observation_state IS NOT NULL)) OR ((result_kind = 'external_link_publication_blocked'::text) AND (result_finding_id IS NULL) AND (result_finding_run_id IS NULL) AND (result_finding_pass_id IS NULL) AND (result_event_ordinal IS NULL) AND (result_event_kind IS NULL) AND (result_reason IS NOT NULL) AND (result_referenced_finding_id IS NULL) AND (result_referenced_finding_run_id IS NULL) AND (result_referenced_finding_pass_id IS NULL) AND (result_referenced_finding_status IS NULL) AND (result_external_link_id IS NOT NULL) AND (result_external_object_key IS NULL) AND (result_observation_state IS NULL)))),
    CONSTRAINT review_pass_result_text_bounds CHECK ((((result_reason IS NULL) OR ((octet_length(result_reason) >= 1) AND (octet_length(result_reason) <= 65536))) AND ((result_external_object_key IS NULL) OR ((octet_length(result_external_object_key) >= 1) AND (octet_length(result_external_object_key) <= 1024))))),
    CONSTRAINT review_pass_state_closed CHECK ((state_kind = ANY (ARRAY['queued'::text, 'running'::text, 'succeeded'::text, 'failed'::text, 'blocked'::text, 'cancelled'::text]))),
    CONSTRAINT review_pass_state_shape CHECK ((((state_kind = 'queued'::text) AND (turn_id IS NULL) AND (output_frontier_id IS NULL) AND (result_kind IS NULL)) OR ((state_kind = ANY (ARRAY['running'::text, 'failed'::text, 'blocked'::text])) AND (turn_id IS NOT NULL) AND (output_frontier_id IS NULL) AND ((state_kind = 'blocked'::text) OR (result_kind IS NULL))) OR ((state_kind = 'succeeded'::text) AND (turn_id IS NOT NULL) AND (output_frontier_id IS NOT NULL)) OR ((state_kind = 'cancelled'::text) AND (output_frontier_id IS NULL) AND (result_kind IS NULL))))
);


--
-- Name: review_pass_finding_inventory_seal; Type: TABLE; Schema: public
--

CREATE TABLE review_pass_finding_inventory_seal (
    pass_id uuid NOT NULL,
    finding_count integer NOT NULL,
    CONSTRAINT review_pass_finding_inventory_seal_count CHECK (((finding_count >= 0) AND (finding_count <= 32)))
);


--
-- Name: review_pass_produced_finding; Type: TABLE; Schema: public
--

CREATE TABLE review_pass_produced_finding (
    pass_id uuid NOT NULL,
    result_ordinal bigint NOT NULL,
    finding_id uuid NOT NULL,
    finding_run_id uuid NOT NULL,
    target_id uuid NOT NULL,
    finding_pass_id uuid NOT NULL,
    CONSTRAINT review_pass_produced_finding_ordinal_bounds CHECK (((result_ordinal >= 1) AND (result_ordinal <= 32))),
    CONSTRAINT review_pass_produced_finding_owner CHECK ((finding_pass_id = pass_id))
);


--
-- Name: review_run; Type: TABLE; Schema: public
--

CREATE TABLE review_run (
    run_id uuid NOT NULL,
    target_id uuid NOT NULL,
    workflow_kind text NOT NULL,
    policy_version bigint NOT NULL,
    minimum_judge_confidence integer NOT NULL,
    minimum_publication_confidence integer NOT NULL,
    state_kind text NOT NULL,
    state_pass_id uuid,
    CONSTRAINT review_run_confidence_bounds CHECK (((policy_version = 1) AND (minimum_judge_confidence = 7000) AND (minimum_publication_confidence = 8000))),
    CONSTRAINT review_run_policy_version_positive_u32 CHECK (((policy_version >= 1) AND (policy_version <= '4294967295'::bigint))),
    CONSTRAINT review_run_state_closed CHECK ((state_kind = ANY (ARRAY['queued'::text, 'running'::text, 'succeeded'::text, 'failed'::text, 'blocked'::text, 'cancelled'::text]))),
    CONSTRAINT review_run_state_shape CHECK ((((state_kind = 'queued'::text) AND (state_pass_id IS NULL)) OR ((state_kind = ANY (ARRAY['running'::text, 'succeeded'::text, 'failed'::text, 'blocked'::text])) AND (state_pass_id IS NOT NULL)) OR (state_kind = 'cancelled'::text))),
    CONSTRAINT review_run_workflow_closed CHECK ((workflow_kind = ANY (ARRAY['import_external_context'::text, 'read_only_review'::text, 'judge_findings'::text, 'dedupe_findings'::text, 'publish_review'::text, 'fix_findings'::text, 'propagate_stack'::text])))
);


--
-- Name: review_target; Type: TABLE; Schema: public
--

CREATE TABLE review_target (
    target_id uuid NOT NULL,
    provider_key text NOT NULL,
    repository_key text NOT NULL,
    subject_kind text NOT NULL,
    change_request_number numeric(20,0),
    head_revision text NOT NULL,
    base_revision text,
    stack_parent_target_id uuid,
    CONSTRAINT review_target_key_bounds CHECK (((octet_length(provider_key) BETWEEN 1 AND 1024) AND (octet_length(repository_key) BETWEEN 1 AND 1024) AND (octet_length(head_revision) BETWEEN 1 AND 1024) AND ((base_revision IS NULL) OR (octet_length(base_revision) BETWEEN 1 AND 1024)))),
    CONSTRAINT review_target_not_self_parent CHECK (((stack_parent_target_id IS NULL) OR (stack_parent_target_id <> target_id))),
    CONSTRAINT review_target_parent_has_base CHECK (((stack_parent_target_id IS NULL) OR (base_revision IS NOT NULL))),
    CONSTRAINT review_target_subject_closed CHECK ((subject_kind = ANY (ARRAY['change_request'::text, 'commit'::text]))),
    CONSTRAINT review_target_subject_shape CHECK ((((subject_kind = 'change_request'::text) AND (change_request_number IS NOT NULL) AND ((change_request_number >= (1)::numeric) AND (change_request_number <= '18446744073709551615'::numeric)) AND (base_revision IS NOT NULL)) OR ((subject_kind = 'commit'::text) AND (change_request_number IS NULL))))
);


--
-- Name: review_workflow_command; Type: TABLE; Schema: public
--

CREATE TABLE review_workflow_command (
    command_id uuid NOT NULL,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    semantic_digest bytea NOT NULL,
    operation_kind text NOT NULL,
    result_kind text NOT NULL,
    result_target_id uuid,
    result_run_id uuid,
    result_pass_id uuid,
    result_finding_id uuid,
    result_external_link_id uuid,
    result_finding_count bigint,
    result_finding_status text,
    result_external_object_key text,
    CONSTRAINT review_workflow_command_digest_size CHECK ((octet_length(semantic_digest) = 32)),
    CONSTRAINT review_workflow_command_external_object_bound CHECK (((result_external_object_key IS NULL) OR ((octet_length(result_external_object_key) >= 1) AND (octet_length(result_external_object_key) <= 1024)))),
    CONSTRAINT review_workflow_command_finding_status_closed CHECK (((result_finding_status IS NULL) OR (result_finding_status = ANY (ARRAY['open'::text, 'accepted'::text, 'rejected'::text, 'duplicate'::text, 'superseded'::text, 'stale'::text, 'posted'::text, 'fixed'::text, 'blocked_with_reason'::text, 'succeeded'::text, 'failed'::text, 'blocked'::text, 'cancelled'::text])))),
    CONSTRAINT review_workflow_command_kind_closed CHECK ((command_kind = 'review_workflow'::text)),
    CONSTRAINT review_workflow_command_operation_closed CHECK ((operation_kind = ANY (ARRAY['create_target'::text, 'start_run'::text, 'activate_pass'::text, 'complete_pass'::text, 'record_findings'::text, 'record_finding_event'::text, 'reserve_external_link'::text, 'attach_external_link'::text]))),
    CONSTRAINT review_workflow_command_operation_result CHECK ((((operation_kind = 'create_target'::text) AND (result_kind = 'target_created'::text)) OR ((operation_kind = 'start_run'::text) AND (result_kind = 'run_started'::text)) OR ((operation_kind = 'activate_pass'::text) AND (result_kind = 'pass_activated'::text)) OR ((operation_kind = 'complete_pass'::text) AND (result_kind = 'pass_completed'::text)) OR ((operation_kind = 'record_findings'::text) AND (result_kind = 'findings_recorded'::text)) OR ((operation_kind = 'record_finding_event'::text) AND (result_kind = 'finding_event_recorded'::text)) OR ((operation_kind = 'reserve_external_link'::text) AND (result_kind = 'external_link_reserved'::text)) OR ((operation_kind = 'attach_external_link'::text) AND (result_kind = 'external_link_attached'::text)))),
    CONSTRAINT review_workflow_command_result_closed CHECK ((result_kind = ANY (ARRAY['target_created'::text, 'run_started'::text, 'pass_activated'::text, 'pass_completed'::text, 'findings_recorded'::text, 'finding_event_recorded'::text, 'external_link_reserved'::text, 'external_link_attached'::text]))),
    CONSTRAINT review_workflow_command_result_shape CHECK ((((result_kind = 'target_created'::text) AND (result_target_id IS NOT NULL) AND (result_run_id IS NULL) AND (result_pass_id IS NULL) AND (result_finding_id IS NULL) AND (result_external_link_id IS NULL) AND (result_finding_count IS NULL) AND (result_finding_status IS NULL) AND (result_external_object_key IS NULL)) OR ((result_kind = ANY (ARRAY['run_started'::text, 'pass_activated'::text])) AND (result_target_id IS NULL) AND (result_run_id IS NOT NULL) AND (result_pass_id IS NOT NULL) AND (result_finding_id IS NULL) AND (result_external_link_id IS NULL) AND (result_finding_count IS NULL) AND (result_finding_status IS NULL) AND (result_external_object_key IS NULL)) OR ((result_kind = 'pass_completed'::text) AND (result_target_id IS NULL) AND (result_run_id IS NOT NULL) AND (result_pass_id IS NOT NULL) AND (result_finding_id IS NULL) AND (result_external_link_id IS NULL) AND (result_finding_count IS NULL) AND (result_finding_status = ANY (ARRAY['succeeded'::text, 'failed'::text, 'blocked'::text, 'cancelled'::text])) AND (result_external_object_key IS NULL)) OR ((result_kind = 'findings_recorded'::text) AND (result_target_id IS NULL) AND (result_run_id IS NOT NULL) AND (result_pass_id IS NOT NULL) AND (result_finding_id IS NULL) AND (result_external_link_id IS NULL) AND (result_finding_count IS NOT NULL) AND (result_finding_count >= 0) AND (result_finding_status IS NULL) AND (result_external_object_key IS NULL)) OR ((result_kind = 'finding_event_recorded'::text) AND (result_target_id IS NULL) AND (result_run_id IS NULL) AND (result_pass_id IS NULL) AND (result_finding_id IS NOT NULL) AND (result_external_link_id IS NULL) AND (result_finding_count IS NULL) AND (result_finding_status = ANY (ARRAY['open'::text, 'accepted'::text, 'rejected'::text, 'duplicate'::text, 'superseded'::text, 'stale'::text, 'posted'::text, 'fixed'::text, 'blocked_with_reason'::text])) AND (result_external_object_key IS NULL)) OR ((result_kind = 'external_link_reserved'::text) AND (result_target_id IS NULL) AND (result_run_id IS NULL) AND (result_pass_id IS NULL) AND (result_finding_id IS NULL) AND (result_external_link_id IS NOT NULL) AND (result_finding_count IS NULL) AND (result_finding_status IS NULL) AND (result_external_object_key IS NULL)) OR ((result_kind = 'external_link_attached'::text) AND (result_target_id IS NULL) AND (result_run_id IS NULL) AND (result_pass_id IS NULL) AND (result_finding_id IS NULL) AND (result_external_link_id IS NOT NULL) AND (result_finding_count IS NULL) AND (result_finding_status IS NULL) AND (result_external_object_key IS NOT NULL)))),
    CONSTRAINT review_workflow_command_storage_version_supported CHECK ((storage_version = 1))
);


--
-- Constraints.
--

--
-- Name: review_external_link review_external_link_attachment_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_external_link
    ADD CONSTRAINT review_external_link_attachment_key UNIQUE (external_link_id, target_id, provider_key, object_kind);


--
-- Name: review_external_link_attachment review_external_link_attachment_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_external_link_attachment
    ADD CONSTRAINT review_external_link_attachment_pkey PRIMARY KEY (external_link_id);


--
-- Name: review_external_link_attachment review_external_link_attachment_target_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_external_link_attachment
    ADD CONSTRAINT review_external_link_attachment_target_key UNIQUE (external_link_id, target_id);


--
-- Name: review_external_link_observation review_external_link_observation_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_external_link_observation
    ADD CONSTRAINT review_external_link_observation_pk PRIMARY KEY (external_link_id, observation_ordinal);


--
-- Name: review_external_link review_external_link_payload_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_external_link
    ADD CONSTRAINT review_external_link_payload_key UNIQUE (external_link_id, target_id, run_id, finding_id, association_kind);


--
-- Name: review_external_link review_external_link_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_external_link
    ADD CONSTRAINT review_external_link_pkey PRIMARY KEY (external_link_id);


--
-- Name: review_external_link review_external_link_target_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_external_link
    ADD CONSTRAINT review_external_link_target_key UNIQUE (external_link_id, target_id);


--
-- Name: review_finding review_finding_ancestry_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_finding
    ADD CONSTRAINT review_finding_ancestry_key UNIQUE (finding_id, run_id, target_id);


--
-- Name: review_finding review_finding_complete_ancestry_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_finding
    ADD CONSTRAINT review_finding_complete_ancestry_key UNIQUE (finding_id, run_id, target_id, producing_pass_id);


--
-- Name: review_finding_event_head review_finding_event_head_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_finding_event_head
    ADD CONSTRAINT review_finding_event_head_pkey PRIMARY KEY (finding_id);


--
-- Name: review_finding_event review_finding_event_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_finding_event
    ADD CONSTRAINT review_finding_event_pk PRIMARY KEY (finding_id, event_ordinal);


--
-- Name: review_finding review_finding_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_finding
    ADD CONSTRAINT review_finding_pkey PRIMARY KEY (finding_id);


--
-- Name: review_orchestration_attempt review_orchestration_attempt_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_attempt
    ADD CONSTRAINT review_orchestration_attempt_pkey PRIMARY KEY (attempt_id);


--
-- Name: review_orchestration_command_effect review_orchestration_command_effect_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_command_effect
    ADD CONSTRAINT review_orchestration_command_effect_pkey PRIMARY KEY (command_id);


--
-- Name: review_orchestration_command_intent review_orchestration_command_intent_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_command_intent
    ADD CONSTRAINT review_orchestration_command_intent_pkey PRIMARY KEY (command_id);


--
-- Name: review_orchestration_command review_orchestration_command_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_command
    ADD CONSTRAINT review_orchestration_command_pkey PRIMARY KEY (command_id);


--
-- Name: review_orchestration_command_recovery review_orchestration_command_recovery_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_command_recovery
    ADD CONSTRAINT review_orchestration_command_recovery_pkey PRIMARY KEY (command_id);


--
-- Name: review_orchestration_concern_finding review_orchestration_concern__attempt_id_concern_key_claim__key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_concern_finding
    ADD CONSTRAINT review_orchestration_concern__attempt_id_concern_key_claim__key UNIQUE (attempt_id, concern_key, claim_ordinal, finding_id);


--
-- Name: review_orchestration_concern review_orchestration_concern_attempt_id_concern_key_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_concern
    ADD CONSTRAINT review_orchestration_concern_attempt_id_concern_key_key UNIQUE (attempt_id, concern_key);


--
-- Name: review_orchestration_concern_claim review_orchestration_concern_claim_effect_attempt_unique; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_concern_claim
    ADD CONSTRAINT review_orchestration_concern_claim_effect_attempt_unique UNIQUE (effect_sequence, attempt_id);


--
-- Name: review_orchestration_concern_claim review_orchestration_concern_claim_effect_sequence_unique; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_concern_claim
    ADD CONSTRAINT review_orchestration_concern_claim_effect_sequence_unique UNIQUE (effect_sequence);


--
-- Name: review_orchestration_concern_claim review_orchestration_concern_claim_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_concern_claim
    ADD CONSTRAINT review_orchestration_concern_claim_pkey PRIMARY KEY (attempt_id, concern_key, claim_ordinal);


--
-- Name: review_orchestration_concern_finding review_orchestration_concern_finding_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_concern_finding
    ADD CONSTRAINT review_orchestration_concern_finding_pkey PRIMARY KEY (attempt_id, concern_key, claim_ordinal, finding_ordinal);


--
-- Name: review_orchestration_concern review_orchestration_concern_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_concern
    ADD CONSTRAINT review_orchestration_concern_pkey PRIMARY KEY (attempt_id, concern_ordinal);


--
-- Name: review_orchestration_fanout_member review_orchestration_fanout_member_attempt_id_concern_key_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_fanout_member
    ADD CONSTRAINT review_orchestration_fanout_member_attempt_id_concern_key_key UNIQUE (attempt_id, concern_key);


--
-- Name: review_orchestration_fanout_member review_orchestration_fanout_member_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_fanout_member
    ADD CONSTRAINT review_orchestration_fanout_member_pkey PRIMARY KEY (attempt_id, member_ordinal);


--
-- Name: review_orchestration_fanout_seal review_orchestration_fanout_seal_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_fanout_seal
    ADD CONSTRAINT review_orchestration_fanout_seal_pkey PRIMARY KEY (attempt_id);


--
-- Name: review_orchestration_import review_orchestration_import_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_import
    ADD CONSTRAINT review_orchestration_import_pkey PRIMARY KEY (attempt_id);


--
-- Name: review_orchestration_judgment_member review_orchestration_judgment_attempt_id_member_ordinal_fin_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_judgment_member
    ADD CONSTRAINT review_orchestration_judgment_attempt_id_member_ordinal_fin_key UNIQUE (attempt_id, member_ordinal, finding_id);


--
-- Name: review_orchestration_judgment_effect review_orchestration_judgment_effect_attempt_id_finding_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_judgment_effect
    ADD CONSTRAINT review_orchestration_judgment_effect_attempt_id_finding_id_key UNIQUE (attempt_id, finding_id);


--
-- Name: review_orchestration_judgment_effect review_orchestration_judgment_effect_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_judgment_effect
    ADD CONSTRAINT review_orchestration_judgment_effect_pkey PRIMARY KEY (attempt_id, effect_ordinal);


--
-- Name: review_orchestration_judgment_member review_orchestration_judgment_member_attempt_id_finding_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_judgment_member
    ADD CONSTRAINT review_orchestration_judgment_member_attempt_id_finding_id_key UNIQUE (attempt_id, finding_id);


--
-- Name: review_orchestration_judgment_member review_orchestration_judgment_member_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_judgment_member
    ADD CONSTRAINT review_orchestration_judgment_member_pkey PRIMARY KEY (attempt_id, member_ordinal);


--
-- Name: review_orchestration_judgment_plan review_orchestration_judgment_plan_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_judgment_plan
    ADD CONSTRAINT review_orchestration_judgment_plan_pkey PRIMARY KEY (attempt_id);


--
-- Name: review_orchestration_publication_inventory review_orchestration_publication_inve_attempt_id_finding_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_publication_inventory
    ADD CONSTRAINT review_orchestration_publication_inve_attempt_id_finding_id_key UNIQUE (attempt_id, finding_id);


--
-- Name: review_orchestration_publication_inventory review_orchestration_publication_inventory_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_publication_inventory
    ADD CONSTRAINT review_orchestration_publication_inventory_pkey PRIMARY KEY (attempt_id, member_ordinal);


--
-- Name: review_orchestration_publication_inventory_seal review_orchestration_publication_inventory_seal_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_publication_inventory_seal
    ADD CONSTRAINT review_orchestration_publication_inventory_seal_pkey PRIMARY KEY (attempt_id);


--
-- Name: review_orchestration_publication_outcome review_orchestration_publication_outc_attempt_id_finding_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_publication_outcome
    ADD CONSTRAINT review_orchestration_publication_outc_attempt_id_finding_id_key UNIQUE (attempt_id, finding_id);


--
-- Name: review_orchestration_publication_outcome review_orchestration_publication_outcome_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_publication_outcome
    ADD CONSTRAINT review_orchestration_publication_outcome_pkey PRIMARY KEY (attempt_id, member_ordinal);


--
-- Name: review_orchestration_publication_outcome_seal review_orchestration_publication_outcome_seal_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_publication_outcome_seal
    ADD CONSTRAINT review_orchestration_publication_outcome_seal_pkey PRIMARY KEY (attempt_id);


--
-- Name: review_orchestration_repair_inventory review_orchestration_repair_inventory_attempt_id_finding_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_repair_inventory
    ADD CONSTRAINT review_orchestration_repair_inventory_attempt_id_finding_id_key UNIQUE (attempt_id, finding_id);


--
-- Name: review_orchestration_repair_inventory review_orchestration_repair_inventory_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_repair_inventory
    ADD CONSTRAINT review_orchestration_repair_inventory_pkey PRIMARY KEY (attempt_id, member_ordinal);


--
-- Name: review_orchestration_repair_inventory_seal review_orchestration_repair_inventory_seal_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_repair_inventory_seal
    ADD CONSTRAINT review_orchestration_repair_inventory_seal_pkey PRIMARY KEY (attempt_id);


--
-- Name: review_orchestration_repair_outcome review_orchestration_repair_outcome_attempt_id_finding_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_repair_outcome
    ADD CONSTRAINT review_orchestration_repair_outcome_attempt_id_finding_id_key UNIQUE (attempt_id, finding_id);


--
-- Name: review_orchestration_repair_outcome review_orchestration_repair_outcome_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_repair_outcome
    ADD CONSTRAINT review_orchestration_repair_outcome_pkey PRIMARY KEY (attempt_id, member_ordinal);


--
-- Name: review_orchestration_repair_outcome_seal review_orchestration_repair_outcome_seal_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_repair_outcome_seal
    ADD CONSTRAINT review_orchestration_repair_outcome_seal_pkey PRIMARY KEY (attempt_id);


--
-- Name: review_pass review_pass_accepted_input_unique; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass
    ADD CONSTRAINT review_pass_accepted_input_unique UNIQUE (accepted_input_id);


--
-- Name: review_pass review_pass_ancestry_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass
    ADD CONSTRAINT review_pass_ancestry_key UNIQUE (pass_id, run_id, target_id);


--
-- Name: review_pass_finding_inventory_seal review_pass_finding_inventory_seal_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass_finding_inventory_seal
    ADD CONSTRAINT review_pass_finding_inventory_seal_pkey PRIMARY KEY (pass_id);


--
-- Name: review_pass review_pass_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass
    ADD CONSTRAINT review_pass_pkey PRIMARY KEY (pass_id);


--
-- Name: review_pass_produced_finding review_pass_produced_finding_complete_identity_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass_produced_finding
    ADD CONSTRAINT review_pass_produced_finding_complete_identity_key UNIQUE (finding_id, finding_run_id, target_id, finding_pass_id);


--
-- Name: review_pass_produced_finding review_pass_produced_finding_identity_unique; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass_produced_finding
    ADD CONSTRAINT review_pass_produced_finding_identity_unique UNIQUE (finding_id);


--
-- Name: review_pass_produced_finding review_pass_produced_finding_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass_produced_finding
    ADD CONSTRAINT review_pass_produced_finding_pk PRIMARY KEY (pass_id, result_ordinal);


--
-- Name: review_pass review_pass_run_unique; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass
    ADD CONSTRAINT review_pass_run_unique UNIQUE (run_id, target_id);


--
-- Name: review_run review_run_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_run
    ADD CONSTRAINT review_run_pkey PRIMARY KEY (run_id);


--
-- Name: review_run review_run_target_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_run
    ADD CONSTRAINT review_run_target_key UNIQUE (run_id, target_id);


--
-- Name: review_target review_target_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_target
    ADD CONSTRAINT review_target_pkey PRIMARY KEY (target_id);


--
-- Name: review_workflow_command review_workflow_command_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_workflow_command
    ADD CONSTRAINT review_workflow_command_pkey PRIMARY KEY (command_id);


--
-- Indexes.
--

--
-- Name: review_external_link_association_index; Type: INDEX; Schema: public
--

CREATE INDEX review_external_link_association_index ON review_external_link USING btree (target_id, association_kind, run_id, finding_id, finding_producing_pass_id);


--
-- Name: review_external_link_attachment_identity_index; Type: INDEX; Schema: public
--

CREATE INDEX review_external_link_attachment_identity_index ON review_external_link_attachment USING btree (identity_digest, target_id);


--
-- Name: review_external_object_identity_digest_index; Type: INDEX; Schema: public
--

CREATE INDEX review_external_object_identity_digest_index ON review_external_object_identity USING btree (identity_digest);


--
-- Name: review_finding_event_blocked_link_index; Type: INDEX; Schema: public
--

CREATE INDEX review_finding_event_blocked_link_index ON review_finding_event USING btree (external_link_id) WHERE ((event_kind = 'blocked_with_reason'::text) AND (external_link_id IS NOT NULL));


--
-- Name: review_finding_event_pass_index; Type: INDEX; Schema: public
--

CREATE INDEX review_finding_event_pass_index ON review_finding_event USING btree (event_pass_run_id, target_id, event_pass_id);


--
-- Name: review_finding_event_publication_link_once; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX review_finding_event_publication_link_once ON review_finding_event USING btree (finding_id, external_link_id) WHERE (event_kind = 'posted'::text);


--
-- Name: review_finding_producing_pass_index; Type: INDEX; Schema: public
--

CREATE INDEX review_finding_producing_pass_index ON review_finding USING btree (producing_pass_id, target_id, run_id, finding_id);


--
-- Name: review_finding_run_index; Type: INDEX; Schema: public
--

CREATE INDEX review_finding_run_index ON review_finding USING btree (run_id, target_id, finding_id);


--
-- Name: review_pass_external_link_result_index; Type: INDEX; Schema: public
--

CREATE INDEX review_pass_external_link_result_index ON review_pass USING btree (result_external_link_id, pass_id) WHERE (result_external_link_id IS NOT NULL);


--
-- Name: review_pass_run_index; Type: INDEX; Schema: public
--

CREATE INDEX review_pass_run_index ON review_pass USING btree (run_id, target_id, pass_id);


--
-- Name: review_run_target_index; Type: INDEX; Schema: public
--

CREATE INDEX review_run_target_index ON review_run USING btree (target_id, run_id);


--
-- Name: review_target_repository_subject_index; Type: INDEX; Schema: public
--

CREATE INDEX review_target_repository_subject_index ON review_target USING btree (provider_key, repository_key, subject_kind);


--
-- Triggers.
--

--
-- Name: review_external_link_attachment review_attachment_posted_event_is_required; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER review_attachment_posted_event_is_required AFTER INSERT ON review_external_link_attachment DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_review_attachment_posted_event();


--
-- Name: review_external_object_identity review_external_identity_attachment_is_required; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER review_external_identity_attachment_is_required AFTER INSERT ON review_external_object_identity DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_review_external_identity_attachment();


--
-- Name: review_external_link_attachment review_external_link_attachment_insert_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_external_link_attachment_insert_is_guarded BEFORE INSERT ON review_external_link_attachment FOR EACH ROW EXECUTE FUNCTION guard_review_external_link_attachment_insert();


--
-- Name: review_external_link_attachment review_external_link_attachment_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_external_link_attachment_is_append_only BEFORE DELETE OR UPDATE ON review_external_link_attachment FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_external_link_attachment review_external_link_attachment_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_external_link_attachment_reject_truncate BEFORE TRUNCATE ON review_external_link_attachment FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_external_link review_external_link_insert_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_external_link_insert_is_guarded BEFORE INSERT ON review_external_link FOR EACH ROW EXECUTE FUNCTION guard_review_external_link_insert();


--
-- Name: review_external_link review_external_link_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_external_link_is_append_only BEFORE DELETE OR UPDATE ON review_external_link FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_external_link_observation review_external_link_observation_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_external_link_observation_is_append_only BEFORE DELETE OR UPDATE ON review_external_link_observation FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_external_link_observation review_external_link_observation_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_external_link_observation_reject_truncate BEFORE TRUNCATE ON review_external_link_observation FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_external_link_observation review_external_link_observation_sequence_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_external_link_observation_sequence_is_guarded BEFORE INSERT ON review_external_link_observation FOR EACH ROW EXECUTE FUNCTION require_review_external_observation_sequence();


--
-- Name: review_external_link review_external_link_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_external_link_reject_truncate BEFORE TRUNCATE ON review_external_link FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_external_object_identity review_external_object_identity_insert_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_external_object_identity_insert_is_guarded BEFORE INSERT ON review_external_object_identity FOR EACH ROW EXECUTE FUNCTION guard_review_external_object_identity_insert();


--
-- Name: review_external_object_identity review_external_object_identity_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_external_object_identity_is_append_only BEFORE DELETE OR UPDATE ON review_external_object_identity FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_external_object_identity review_external_object_identity_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_external_object_identity_reject_truncate BEFORE TRUNCATE ON review_external_object_identity FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_finding_event_head review_finding_event_head_change_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_finding_event_head_change_is_guarded BEFORE INSERT OR DELETE OR UPDATE ON review_finding_event_head FOR EACH ROW EXECUTE FUNCTION guard_review_finding_event_head_change();


--
-- Name: review_finding review_finding_event_head_is_created; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_finding_event_head_is_created AFTER INSERT ON review_finding FOR EACH ROW EXECUTE FUNCTION create_review_finding_event_head();


--
-- Name: review_finding_event_head review_finding_event_head_rechecks_finding; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER review_finding_event_head_rechecks_finding AFTER INSERT OR DELETE OR UPDATE ON review_finding_event_head DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_review_finding_event_head_complete();


--
-- Name: review_finding_event_head review_finding_event_head_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_finding_event_head_reject_truncate BEFORE TRUNCATE ON review_finding_event_head FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_finding_event review_finding_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_finding_event_is_append_only BEFORE DELETE OR UPDATE ON review_finding_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_finding_event review_finding_event_rechecks_head; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER review_finding_event_rechecks_head AFTER INSERT OR DELETE OR UPDATE ON review_finding_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_review_finding_event_head_complete();


--
-- Name: review_finding_event review_finding_event_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_finding_event_reject_truncate BEFORE TRUNCATE ON review_finding_event FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_finding_event review_finding_event_sequence_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_finding_event_sequence_is_guarded BEFORE INSERT ON review_finding_event FOR EACH ROW EXECUTE FUNCTION require_review_finding_event_sequence();


--
-- Name: review_finding_event review_finding_event_transition_head_is_advanced; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_finding_event_transition_head_is_advanced AFTER INSERT ON review_finding_event FOR EACH ROW EXECUTE FUNCTION advance_review_finding_event_head();


--
-- Name: review_finding_event review_finding_event_transition_head_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_finding_event_transition_head_is_guarded BEFORE INSERT ON review_finding_event FOR EACH ROW EXECUTE FUNCTION authenticate_review_finding_event_head();


--
-- Name: review_finding review_finding_insert_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_finding_insert_is_guarded BEFORE INSERT ON review_finding FOR EACH ROW EXECUTE FUNCTION guard_review_finding_insert();


--
-- Name: review_finding review_finding_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_finding_is_append_only BEFORE DELETE OR UPDATE ON review_finding FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_finding review_finding_reject_sealed_expansion; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_finding_reject_sealed_expansion BEFORE INSERT ON review_finding FOR EACH ROW EXECUTE FUNCTION reject_sealed_review_finding_inventory_expansion();


--
-- Name: review_finding review_finding_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_finding_reject_truncate BEFORE TRUNCATE ON review_finding FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_finding review_finding_requires_event_head; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER review_finding_requires_event_head AFTER INSERT ON review_finding DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_review_finding_event_head_complete();


--
-- Name: review_orchestration_attempt review_orchestration_attempt_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_attempt_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_attempt FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_attempt review_orchestration_attempt_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_attempt_reject_truncate BEFORE TRUNCATE ON review_orchestration_attempt FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_command_effect review_orchestration_command_effect_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_command_effect_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_command_effect FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_command_effect review_orchestration_command_effect_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_command_effect_reject_truncate BEFORE TRUNCATE ON review_orchestration_command_effect FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_command_intent review_orchestration_command_intent_change_guard; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_command_intent_change_guard BEFORE DELETE OR UPDATE ON review_orchestration_command_intent FOR EACH ROW EXECUTE FUNCTION reject_review_orchestration_intent_change();


--
-- Name: review_orchestration_command_intent review_orchestration_command_intent_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_command_intent_reject_truncate BEFORE TRUNCATE ON review_orchestration_command_intent FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_command review_orchestration_command_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_command_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_command FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_command_recovery review_orchestration_command_recovery_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_command_recovery_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_command_recovery FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_command_recovery review_orchestration_command_recovery_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_command_recovery_reject_truncate BEFORE TRUNCATE ON review_orchestration_command_recovery FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_command review_orchestration_command_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_command_reject_truncate BEFORE TRUNCATE ON review_orchestration_command FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_concern_claim review_orchestration_concern_claim_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_concern_claim_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_concern_claim FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_concern_claim review_orchestration_concern_claim_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_concern_claim_reject_truncate BEFORE TRUNCATE ON review_orchestration_concern_claim FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_concern_finding review_orchestration_concern_finding_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_concern_finding_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_concern_finding FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_concern_finding review_orchestration_concern_finding_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_concern_finding_reject_truncate BEFORE TRUNCATE ON review_orchestration_concern_finding FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_concern review_orchestration_concern_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_concern_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_concern FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_concern review_orchestration_concern_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_concern_reject_truncate BEFORE TRUNCATE ON review_orchestration_concern FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_fanout_member review_orchestration_fanout_member_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_fanout_member_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_fanout_member FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_fanout_member review_orchestration_fanout_member_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_fanout_member_reject_truncate BEFORE TRUNCATE ON review_orchestration_fanout_member FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_fanout_seal review_orchestration_fanout_seal_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_fanout_seal_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_fanout_seal FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_fanout_seal review_orchestration_fanout_seal_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_fanout_seal_reject_truncate BEFORE TRUNCATE ON review_orchestration_fanout_seal FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_import review_orchestration_import_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_import_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_import FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_import review_orchestration_import_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_import_reject_truncate BEFORE TRUNCATE ON review_orchestration_import FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_judgment_effect review_orchestration_judgment_effect_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_judgment_effect_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_judgment_effect FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_judgment_effect review_orchestration_judgment_effect_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_judgment_effect_reject_truncate BEFORE TRUNCATE ON review_orchestration_judgment_effect FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_judgment_member review_orchestration_judgment_member_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_judgment_member_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_judgment_member FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_judgment_member review_orchestration_judgment_member_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_judgment_member_reject_truncate BEFORE TRUNCATE ON review_orchestration_judgment_member FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_judgment_plan review_orchestration_judgment_plan_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_judgment_plan_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_judgment_plan FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_judgment_plan review_orchestration_judgment_plan_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_judgment_plan_reject_truncate BEFORE TRUNCATE ON review_orchestration_judgment_plan FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_publication_inventory review_orchestration_publication_inventory_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_publication_inventory_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_publication_inventory FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_publication_inventory review_orchestration_publication_inventory_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_publication_inventory_reject_truncate BEFORE TRUNCATE ON review_orchestration_publication_inventory FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_publication_inventory_seal review_orchestration_publication_inventory_seal_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_publication_inventory_seal_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_publication_inventory_seal FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_publication_inventory_seal review_orchestration_publication_inventory_seal_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_publication_inventory_seal_reject_truncate BEFORE TRUNCATE ON review_orchestration_publication_inventory_seal FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_publication_outcome review_orchestration_publication_outcome_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_publication_outcome_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_publication_outcome FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_publication_outcome review_orchestration_publication_outcome_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_publication_outcome_reject_truncate BEFORE TRUNCATE ON review_orchestration_publication_outcome FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_publication_outcome_seal review_orchestration_publication_outcome_seal_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_publication_outcome_seal_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_publication_outcome_seal FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_publication_outcome_seal review_orchestration_publication_outcome_seal_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_publication_outcome_seal_reject_truncate BEFORE TRUNCATE ON review_orchestration_publication_outcome_seal FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_repair_inventory review_orchestration_repair_inventory_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_repair_inventory_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_repair_inventory FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_repair_inventory review_orchestration_repair_inventory_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_repair_inventory_reject_truncate BEFORE TRUNCATE ON review_orchestration_repair_inventory FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_repair_inventory_seal review_orchestration_repair_inventory_seal_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_repair_inventory_seal_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_repair_inventory_seal FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_repair_inventory_seal review_orchestration_repair_inventory_seal_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_repair_inventory_seal_reject_truncate BEFORE TRUNCATE ON review_orchestration_repair_inventory_seal FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_repair_outcome review_orchestration_repair_outcome_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_repair_outcome_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_repair_outcome FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_repair_outcome review_orchestration_repair_outcome_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_repair_outcome_reject_truncate BEFORE TRUNCATE ON review_orchestration_repair_outcome FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_orchestration_repair_outcome_seal review_orchestration_repair_outcome_seal_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_repair_outcome_seal_is_append_only BEFORE DELETE OR UPDATE ON review_orchestration_repair_outcome_seal FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_orchestration_repair_outcome_seal review_orchestration_repair_outcome_seal_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_orchestration_repair_outcome_seal_reject_truncate BEFORE TRUNCATE ON review_orchestration_repair_outcome_seal FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_pass review_pass_bound_referenced_target_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_pass_bound_referenced_target_is_guarded BEFORE UPDATE OF result_referenced_finding_target_id ON review_pass FOR EACH ROW EXECUTE FUNCTION guard_bound_review_pass_referenced_target();


--
-- Name: review_pass review_pass_change_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_pass_change_is_guarded BEFORE INSERT OR UPDATE ON review_pass FOR EACH ROW EXECUTE FUNCTION guard_review_pass_change();


--
-- Name: review_pass review_pass_external_result_is_guarded; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER review_pass_external_result_is_guarded AFTER UPDATE OF result_kind ON review_pass DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_review_pass_external_result();


--
-- Name: review_finding review_pass_finding_inventory_from_finding; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER review_pass_finding_inventory_from_finding AFTER INSERT ON review_finding DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_review_pass_finding_inventory();


--
-- Name: review_pass_produced_finding review_pass_finding_inventory_from_member; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER review_pass_finding_inventory_from_member AFTER INSERT ON review_pass_produced_finding DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_review_pass_finding_inventory();


--
-- Name: review_pass review_pass_finding_inventory_from_pass; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER review_pass_finding_inventory_from_pass AFTER UPDATE OF result_kind ON review_pass DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_review_pass_finding_inventory();


--
-- Name: review_pass_finding_inventory_seal review_pass_finding_inventory_from_seal; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER review_pass_finding_inventory_from_seal AFTER INSERT ON review_pass_finding_inventory_seal DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_review_pass_finding_inventory();


--
-- Name: review_pass_finding_inventory_seal review_pass_finding_inventory_seal_insert_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_pass_finding_inventory_seal_insert_is_guarded BEFORE INSERT ON review_pass_finding_inventory_seal FOR EACH ROW EXECUTE FUNCTION guard_review_pass_finding_inventory_seal();


--
-- Name: review_pass_finding_inventory_seal review_pass_finding_inventory_seal_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_pass_finding_inventory_seal_is_append_only BEFORE DELETE OR UPDATE ON review_pass_finding_inventory_seal FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_pass_finding_inventory_seal review_pass_finding_inventory_seal_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_pass_finding_inventory_seal_reject_truncate BEFORE TRUNCATE ON review_pass_finding_inventory_seal FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_pass_produced_finding review_pass_produced_finding_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_pass_produced_finding_is_append_only BEFORE DELETE OR UPDATE ON review_pass_produced_finding FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_pass_produced_finding review_pass_produced_finding_reject_sealed_expansion; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_pass_produced_finding_reject_sealed_expansion BEFORE INSERT ON review_pass_produced_finding FOR EACH ROW EXECUTE FUNCTION reject_sealed_review_finding_inventory_expansion();


--
-- Name: review_pass_produced_finding review_pass_produced_finding_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_pass_produced_finding_reject_truncate BEFORE TRUNCATE ON review_pass_produced_finding FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_pass review_pass_reject_delete; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_pass_reject_delete BEFORE DELETE ON review_pass FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_pass review_pass_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_pass_reject_truncate BEFORE TRUNCATE ON review_pass FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_pass review_pass_run_projection_is_guarded; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER review_pass_run_projection_is_guarded AFTER INSERT OR UPDATE ON review_pass DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_review_pass_run_projection();


--
-- Name: review_run review_run_change_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_run_change_is_guarded BEFORE INSERT OR UPDATE ON review_run FOR EACH ROW EXECUTE FUNCTION guard_review_run_change();


--
-- Name: review_run review_run_pass_projection_is_guarded; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER review_run_pass_projection_is_guarded AFTER INSERT OR UPDATE ON review_run DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_review_run_pass_projection();


--
-- Name: review_run review_run_reject_delete; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_run_reject_delete BEFORE DELETE ON review_run FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_run review_run_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_run_reject_truncate BEFORE TRUNCATE ON review_run FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_target review_target_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_target_is_append_only BEFORE DELETE OR UPDATE ON review_target FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_target review_target_parent_is_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_target_parent_is_guarded BEFORE INSERT ON review_target FOR EACH ROW EXECUTE FUNCTION guard_review_target_parent();


--
-- Name: review_target review_target_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_target_reject_truncate BEFORE TRUNCATE ON review_target FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();


--
-- Name: review_workflow_command review_workflow_command_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_workflow_command_is_append_only BEFORE DELETE OR UPDATE ON review_workflow_command FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: review_workflow_command review_workflow_command_truncate_is_rejected; Type: TRIGGER; Schema: public
--

CREATE TRIGGER review_workflow_command_truncate_is_rejected BEFORE TRUNCATE ON review_workflow_command FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_command_truncate();


--
-- Foreign keys.
--

--
-- Name: review_external_link_attachment review_external_link_attachment_pass_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_external_link_attachment
    ADD CONSTRAINT review_external_link_attachment_pass_fk FOREIGN KEY (pass_id, pass_run_id, target_id) REFERENCES review_pass(pass_id, run_id, target_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: review_external_link_attachment review_external_link_attachment_reservation_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_external_link_attachment
    ADD CONSTRAINT review_external_link_attachment_reservation_fk FOREIGN KEY (external_link_id, target_id, provider_key, object_kind) REFERENCES review_external_link(external_link_id, target_id, provider_key, object_kind) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: review_external_link review_external_link_finding_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_external_link
    ADD CONSTRAINT review_external_link_finding_fk FOREIGN KEY (finding_id, run_id, target_id, finding_producing_pass_id) REFERENCES review_finding(finding_id, run_id, target_id, producing_pass_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: review_external_link_observation review_external_link_observation_attachment_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_external_link_observation
    ADD CONSTRAINT review_external_link_observation_attachment_fk FOREIGN KEY (external_link_id, target_id) REFERENCES review_external_link_attachment(external_link_id, target_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: review_external_link_observation review_external_link_observation_pass_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_external_link_observation
    ADD CONSTRAINT review_external_link_observation_pass_fk FOREIGN KEY (pass_id, pass_run_id, target_id) REFERENCES review_pass(pass_id, run_id, target_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: review_external_link review_external_link_run_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_external_link
    ADD CONSTRAINT review_external_link_run_fk FOREIGN KEY (run_id, target_id) REFERENCES review_run(run_id, target_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: review_external_link review_external_link_target_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_external_link
    ADD CONSTRAINT review_external_link_target_fk FOREIGN KEY (target_id) REFERENCES review_target(target_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: review_external_object_identity review_external_object_identity_target_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_external_object_identity
    ADD CONSTRAINT review_external_object_identity_target_fk FOREIGN KEY (logical_target_id) REFERENCES review_target(target_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: review_finding_event review_finding_event_external_link_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_finding_event
    ADD CONSTRAINT review_finding_event_external_link_fk FOREIGN KEY (external_link_id, target_id, finding_run_id, finding_id, external_link_association_kind) REFERENCES review_external_link(external_link_id, target_id, run_id, finding_id, association_kind) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: review_finding_event review_finding_event_finding_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_finding_event
    ADD CONSTRAINT review_finding_event_finding_fk FOREIGN KEY (finding_id, finding_run_id, target_id) REFERENCES review_finding(finding_id, run_id, target_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: review_finding_event_head review_finding_event_head_event_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_finding_event_head
    ADD CONSTRAINT review_finding_event_head_event_fk FOREIGN KEY (finding_id, event_ordinal) REFERENCES review_finding_event(finding_id, event_ordinal) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: review_finding_event_head review_finding_event_head_finding_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_finding_event_head
    ADD CONSTRAINT review_finding_event_head_finding_fk FOREIGN KEY (finding_id) REFERENCES review_finding(finding_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: review_finding_event review_finding_event_pass_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_finding_event
    ADD CONSTRAINT review_finding_event_pass_fk FOREIGN KEY (event_pass_id, event_pass_run_id, target_id) REFERENCES review_pass(pass_id, run_id, target_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: review_finding_event review_finding_event_referenced_finding_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_finding_event
    ADD CONSTRAINT review_finding_event_referenced_finding_fk FOREIGN KEY (referenced_finding_id, referenced_finding_run_id, referenced_finding_target_id, referenced_finding_pass_id) REFERENCES review_finding(finding_id, run_id, target_id, producing_pass_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: review_finding_event review_finding_event_referenced_inventory_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_finding_event
    ADD CONSTRAINT review_finding_event_referenced_inventory_fk FOREIGN KEY (referenced_finding_id, referenced_finding_run_id, referenced_finding_target_id, referenced_finding_pass_id) REFERENCES review_pass_produced_finding(finding_id, finding_run_id, target_id, finding_pass_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: review_finding review_finding_producing_pass_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_finding
    ADD CONSTRAINT review_finding_producing_pass_fk FOREIGN KEY (producing_pass_id, run_id, target_id) REFERENCES review_pass(pass_id, run_id, target_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: review_orchestration_attempt review_orchestration_attempt_target_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_attempt
    ADD CONSTRAINT review_orchestration_attempt_target_id_fkey FOREIGN KEY (target_id) REFERENCES review_target(target_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_command_intent review_orchestration_command__command_id_command_kind_stor_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_command_intent
    ADD CONSTRAINT review_orchestration_command__command_id_command_kind_stor_fkey FOREIGN KEY (command_id, command_kind, storage_version) REFERENCES durable_command(command_id, command_kind, storage_version) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: review_orchestration_command_effect review_orchestration_command__concern_effect_sequence_atte_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_command_effect
    ADD CONSTRAINT review_orchestration_command__concern_effect_sequence_atte_fkey FOREIGN KEY (concern_effect_sequence, attempt_id) REFERENCES review_orchestration_concern_claim(effect_sequence, attempt_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: review_orchestration_command review_orchestration_command_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_command
    ADD CONSTRAINT review_orchestration_command_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES review_orchestration_attempt(attempt_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_command review_orchestration_command_command_id_command_kind_stora_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_command
    ADD CONSTRAINT review_orchestration_command_command_id_command_kind_stora_fkey FOREIGN KEY (command_id, command_kind, storage_version) REFERENCES durable_command(command_id, command_kind, storage_version) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: review_orchestration_command_effect review_orchestration_command_effect_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_command_effect
    ADD CONSTRAINT review_orchestration_command_effect_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES review_orchestration_attempt(attempt_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: review_orchestration_command_effect review_orchestration_command_effect_command_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_command_effect
    ADD CONSTRAINT review_orchestration_command_effect_command_id_fkey FOREIGN KEY (command_id) REFERENCES durable_command(command_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_command_recovery review_orchestration_command_recovery_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_command_recovery
    ADD CONSTRAINT review_orchestration_command_recovery_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES review_orchestration_attempt(attempt_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_concern_finding review_orchestration_concern__attempt_id_concern_key_claim_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_concern_finding
    ADD CONSTRAINT review_orchestration_concern__attempt_id_concern_key_claim_fkey FOREIGN KEY (attempt_id, concern_key, claim_ordinal) REFERENCES review_orchestration_concern_claim(attempt_id, concern_key, claim_ordinal) ON DELETE RESTRICT;


--
-- Name: review_orchestration_concern review_orchestration_concern_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_concern
    ADD CONSTRAINT review_orchestration_concern_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES review_orchestration_attempt(attempt_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_concern_claim review_orchestration_concern_claim_attempt_id_concern_key_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_concern_claim
    ADD CONSTRAINT review_orchestration_concern_claim_attempt_id_concern_key_fkey FOREIGN KEY (attempt_id, concern_key) REFERENCES review_orchestration_concern(attempt_id, concern_key) ON DELETE RESTRICT;


--
-- Name: review_orchestration_concern_claim review_orchestration_concern_claim_pass_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_concern_claim
    ADD CONSTRAINT review_orchestration_concern_claim_pass_id_fkey FOREIGN KEY (pass_id) REFERENCES review_pass(pass_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_concern_finding review_orchestration_concern_finding_finding_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_concern_finding
    ADD CONSTRAINT review_orchestration_concern_finding_finding_id_fkey FOREIGN KEY (finding_id) REFERENCES review_finding(finding_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_fanout_member review_orchestration_fanout_m_attempt_id_concern_key_claim_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_fanout_member
    ADD CONSTRAINT review_orchestration_fanout_m_attempt_id_concern_key_claim_fkey FOREIGN KEY (attempt_id, concern_key, claim_ordinal) REFERENCES review_orchestration_concern_claim(attempt_id, concern_key, claim_ordinal) ON DELETE RESTRICT;


--
-- Name: review_orchestration_fanout_member review_orchestration_fanout_member_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_fanout_member
    ADD CONSTRAINT review_orchestration_fanout_member_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES review_orchestration_fanout_seal(attempt_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_fanout_seal review_orchestration_fanout_seal_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_fanout_seal
    ADD CONSTRAINT review_orchestration_fanout_seal_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES review_orchestration_attempt(attempt_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_import review_orchestration_import_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_import
    ADD CONSTRAINT review_orchestration_import_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES review_orchestration_attempt(attempt_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_import review_orchestration_import_external_link_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_import
    ADD CONSTRAINT review_orchestration_import_external_link_id_fkey FOREIGN KEY (external_link_id) REFERENCES review_external_link(external_link_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_import review_orchestration_import_pass_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_import
    ADD CONSTRAINT review_orchestration_import_pass_id_fkey FOREIGN KEY (pass_id) REFERENCES review_pass(pass_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_judgment_effect review_orchestration_judgment_attempt_id_effect_ordinal_fi_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_judgment_effect
    ADD CONSTRAINT review_orchestration_judgment_attempt_id_effect_ordinal_fi_fkey FOREIGN KEY (attempt_id, effect_ordinal, finding_id) REFERENCES review_orchestration_judgment_member(attempt_id, member_ordinal, finding_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_judgment_effect review_orchestration_judgment_effect_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_judgment_effect
    ADD CONSTRAINT review_orchestration_judgment_effect_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES review_orchestration_judgment_plan(attempt_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_judgment_member review_orchestration_judgment_member_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_judgment_member
    ADD CONSTRAINT review_orchestration_judgment_member_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES review_orchestration_judgment_plan(attempt_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_judgment_member review_orchestration_judgment_member_finding_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_judgment_member
    ADD CONSTRAINT review_orchestration_judgment_member_finding_id_fkey FOREIGN KEY (finding_id) REFERENCES review_finding(finding_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_judgment_member review_orchestration_judgment_member_finding_pass_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_judgment_member
    ADD CONSTRAINT review_orchestration_judgment_member_finding_pass_id_fkey FOREIGN KEY (finding_pass_id) REFERENCES review_pass(pass_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_judgment_member review_orchestration_judgment_member_finding_run_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_judgment_member
    ADD CONSTRAINT review_orchestration_judgment_member_finding_run_id_fkey FOREIGN KEY (finding_run_id) REFERENCES review_run(run_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_judgment_member review_orchestration_judgment_member_referenced_finding_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_judgment_member
    ADD CONSTRAINT review_orchestration_judgment_member_referenced_finding_id_fkey FOREIGN KEY (referenced_finding_id) REFERENCES review_finding(finding_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_judgment_member review_orchestration_judgment_member_referenced_pass_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_judgment_member
    ADD CONSTRAINT review_orchestration_judgment_member_referenced_pass_id_fkey FOREIGN KEY (referenced_pass_id) REFERENCES review_pass(pass_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_judgment_member review_orchestration_judgment_member_referenced_run_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_judgment_member
    ADD CONSTRAINT review_orchestration_judgment_member_referenced_run_id_fkey FOREIGN KEY (referenced_run_id) REFERENCES review_run(run_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_judgment_plan review_orchestration_judgment_plan_analysis_pass_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_judgment_plan
    ADD CONSTRAINT review_orchestration_judgment_plan_analysis_pass_id_fkey FOREIGN KEY (analysis_pass_id) REFERENCES review_pass(pass_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_judgment_plan review_orchestration_judgment_plan_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_judgment_plan
    ADD CONSTRAINT review_orchestration_judgment_plan_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES review_orchestration_fanout_seal(attempt_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_publication_inventory review_orchestration_publication_inventory_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_publication_inventory
    ADD CONSTRAINT review_orchestration_publication_inventory_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES review_orchestration_publication_inventory_seal(attempt_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_publication_inventory review_orchestration_publication_inventory_finding_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_publication_inventory
    ADD CONSTRAINT review_orchestration_publication_inventory_finding_id_fkey FOREIGN KEY (finding_id) REFERENCES review_finding(finding_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_publication_inventory review_orchestration_publication_inventory_finding_pass_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_publication_inventory
    ADD CONSTRAINT review_orchestration_publication_inventory_finding_pass_id_fkey FOREIGN KEY (finding_pass_id) REFERENCES review_pass(pass_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_publication_inventory review_orchestration_publication_inventory_finding_run_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_publication_inventory
    ADD CONSTRAINT review_orchestration_publication_inventory_finding_run_id_fkey FOREIGN KEY (finding_run_id) REFERENCES review_run(run_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_publication_inventory_seal review_orchestration_publication_inventory_seal_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_publication_inventory_seal
    ADD CONSTRAINT review_orchestration_publication_inventory_seal_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES review_orchestration_repair_outcome_seal(attempt_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_publication_outcome review_orchestration_publication_outcome_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_publication_outcome
    ADD CONSTRAINT review_orchestration_publication_outcome_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES review_orchestration_publication_outcome_seal(attempt_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_publication_outcome review_orchestration_publication_outcome_external_link_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_publication_outcome
    ADD CONSTRAINT review_orchestration_publication_outcome_external_link_id_fkey FOREIGN KEY (external_link_id) REFERENCES review_external_link(external_link_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_publication_outcome review_orchestration_publication_outcome_finding_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_publication_outcome
    ADD CONSTRAINT review_orchestration_publication_outcome_finding_id_fkey FOREIGN KEY (finding_id) REFERENCES review_finding(finding_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_publication_outcome_seal review_orchestration_publication_outcome_seal_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_publication_outcome_seal
    ADD CONSTRAINT review_orchestration_publication_outcome_seal_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES review_orchestration_publication_inventory_seal(attempt_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_repair_inventory review_orchestration_repair_inventory_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_repair_inventory
    ADD CONSTRAINT review_orchestration_repair_inventory_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES review_orchestration_repair_inventory_seal(attempt_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_repair_inventory review_orchestration_repair_inventory_finding_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_repair_inventory
    ADD CONSTRAINT review_orchestration_repair_inventory_finding_id_fkey FOREIGN KEY (finding_id) REFERENCES review_finding(finding_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_repair_inventory review_orchestration_repair_inventory_finding_pass_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_repair_inventory
    ADD CONSTRAINT review_orchestration_repair_inventory_finding_pass_id_fkey FOREIGN KEY (finding_pass_id) REFERENCES review_pass(pass_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_repair_inventory review_orchestration_repair_inventory_finding_run_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_repair_inventory
    ADD CONSTRAINT review_orchestration_repair_inventory_finding_run_id_fkey FOREIGN KEY (finding_run_id) REFERENCES review_run(run_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_repair_inventory_seal review_orchestration_repair_inventory_seal_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_repair_inventory_seal
    ADD CONSTRAINT review_orchestration_repair_inventory_seal_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES review_orchestration_judgment_plan(attempt_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_repair_outcome review_orchestration_repair_outcome_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_repair_outcome
    ADD CONSTRAINT review_orchestration_repair_outcome_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES review_orchestration_repair_outcome_seal(attempt_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_repair_outcome review_orchestration_repair_outcome_finding_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_repair_outcome
    ADD CONSTRAINT review_orchestration_repair_outcome_finding_id_fkey FOREIGN KEY (finding_id) REFERENCES review_finding(finding_id) ON DELETE RESTRICT;


--
-- Name: review_orchestration_repair_outcome_seal review_orchestration_repair_outcome_seal_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_orchestration_repair_outcome_seal
    ADD CONSTRAINT review_orchestration_repair_outcome_seal_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES review_orchestration_repair_inventory_seal(attempt_id) ON DELETE RESTRICT;


--
-- Name: review_pass review_pass_accepted_input_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass
    ADD CONSTRAINT review_pass_accepted_input_fk FOREIGN KEY (accepted_input_id, session_id) REFERENCES accepted_input(accepted_input_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: review_pass_finding_inventory_seal review_pass_finding_inventory_seal_pass_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass_finding_inventory_seal
    ADD CONSTRAINT review_pass_finding_inventory_seal_pass_fk FOREIGN KEY (pass_id) REFERENCES review_pass(pass_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: review_pass review_pass_origin_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass
    ADD CONSTRAINT review_pass_origin_turn_fk FOREIGN KEY (origin_turn_id, session_id, accepted_input_id) REFERENCES turn_lifecycle(turn_id, session_id, origin_accepted_input_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: review_pass_produced_finding review_pass_produced_finding_finding_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass_produced_finding
    ADD CONSTRAINT review_pass_produced_finding_finding_fk FOREIGN KEY (finding_id, finding_run_id, target_id, finding_pass_id) REFERENCES review_finding(finding_id, run_id, target_id, producing_pass_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: review_pass_produced_finding review_pass_produced_finding_pass_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass_produced_finding
    ADD CONSTRAINT review_pass_produced_finding_pass_fk FOREIGN KEY (pass_id, finding_run_id, target_id) REFERENCES review_pass(pass_id, run_id, target_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: review_pass review_pass_result_external_link_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass
    ADD CONSTRAINT review_pass_result_external_link_fk FOREIGN KEY (result_external_link_id, target_id) REFERENCES review_external_link(external_link_id, target_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: review_pass review_pass_result_finding_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass
    ADD CONSTRAINT review_pass_result_finding_fk FOREIGN KEY (result_finding_id, result_finding_run_id, target_id, result_finding_pass_id) REFERENCES review_finding(finding_id, run_id, target_id, producing_pass_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: review_pass review_pass_result_referenced_finding_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass
    ADD CONSTRAINT review_pass_result_referenced_finding_fk FOREIGN KEY (result_referenced_finding_id, result_referenced_finding_run_id, result_referenced_finding_target_id, result_referenced_finding_pass_id) REFERENCES review_finding(finding_id, run_id, target_id, producing_pass_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: review_pass review_pass_result_referenced_inventory_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass
    ADD CONSTRAINT review_pass_result_referenced_inventory_fk FOREIGN KEY (result_referenced_finding_id, result_referenced_finding_run_id, result_referenced_finding_target_id, result_referenced_finding_pass_id) REFERENCES review_pass_produced_finding(finding_id, finding_run_id, target_id, finding_pass_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: review_pass review_pass_run_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass
    ADD CONSTRAINT review_pass_run_fk FOREIGN KEY (run_id, target_id) REFERENCES review_run(run_id, target_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: review_pass review_pass_terminal_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass
    ADD CONSTRAINT review_pass_terminal_frontier_fk FOREIGN KEY (turn_id, session_id, accepted_input_id, output_frontier_id) REFERENCES turn_lifecycle(turn_id, session_id, origin_accepted_input_id, terminal_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: review_pass review_pass_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_pass
    ADD CONSTRAINT review_pass_turn_fk FOREIGN KEY (turn_id, session_id, accepted_input_id) REFERENCES turn_lifecycle(turn_id, session_id, origin_accepted_input_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: review_run review_run_state_pass_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_run
    ADD CONSTRAINT review_run_state_pass_fk FOREIGN KEY (state_pass_id, run_id, target_id) REFERENCES review_pass(pass_id, run_id, target_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: review_run review_run_target_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_run
    ADD CONSTRAINT review_run_target_fk FOREIGN KEY (target_id) REFERENCES review_target(target_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: review_target review_target_parent_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_target
    ADD CONSTRAINT review_target_parent_fk FOREIGN KEY (stack_parent_target_id) REFERENCES review_target(target_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: review_workflow_command review_workflow_command_registry_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY review_workflow_command
    ADD CONSTRAINT review_workflow_command_registry_fk FOREIGN KEY (command_id, command_kind, storage_version) REFERENCES durable_command(command_id, command_kind, storage_version) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Search-path pins for this file's constraint-reachable functions.
--
-- The pin has to name the schema the migration selected rather than a
-- literal, so it is applied here through current_schema instead of inline
-- in each CREATE FUNCTION (the full rationale is in 202609010000_core.sql;
-- crates/persistence/tests/search_path_postgres.rs is the guard).
--

DO $search_path_pins$
DECLARE
    signature text;
BEGIN
    -- the canonical restore-safe pin: the migration-selected schema, then pg_catalog, then pg_temp
    FOREACH signature IN ARRAY ARRAY[
        'advance_review_finding_event_head()',
        'assert_review_finding_event_head_complete(uuid)',
        'authenticate_review_finding_event_head()',
        'create_review_finding_event_head()',
        'guard_review_finding_event_head_change()',
        'require_review_finding_event_head_complete()',
        'require_review_finding_event_sequence()'
    ] LOOP
        EXECUTE format('ALTER FUNCTION %s SET search_path TO %I, pg_catalog, pg_temp',
                   signature, current_schema);
    END LOOP;
END
$search_path_pins$;


--
-- Restore body validation for anything applied after this file.
--

RESET check_function_bodies;
