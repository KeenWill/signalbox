-- Repo watch: the poll cursor and event log, webhook receipt and projection,
-- rule activation and evaluation, pull-request state, dispatch obligations
-- through leases, batches, deliveries, and settlements, and commissioned
-- dispatches with their headless approval escalations.

-- Function bodies may read tables a later section or file creates;
-- 202609010000_core.sql explains why validation is deferred.
SET check_function_bodies = false;

--
-- Functions.
--

--
-- Name: adjust_repo_watch_latest_event_obligation_count(uuid, bigint); Type: FUNCTION; Schema: public
--

CREATE FUNCTION adjust_repo_watch_latest_event_obligation_count(counted_event_id uuid, obligation_delta bigint) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    counted_repository text;
    counted_pull_request numeric;
BEGIN
    SELECT repository, pull_request_number
      INTO counted_repository, counted_pull_request
      FROM repo_watch_event
     WHERE event_id = counted_event_id;
    IF counted_pull_request IS NOT NULL THEN
        PERFORM adjust_repo_watch_pull_request_work_count(
            counted_repository, counted_pull_request, 0, obligation_delta
        );
    END IF;
END;
$$;


--
-- Name: adjust_repo_watch_pull_request_work_count(text, numeric, bigint, bigint); Type: FUNCTION; Schema: public
--

CREATE FUNCTION adjust_repo_watch_pull_request_work_count(counted_repository text, counted_pull_request numeric, held_delta bigint, obligation_delta bigint) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    current_held_count bigint;
    current_obligation_count bigint;
    adjusted_held_count bigint;
    adjusted_obligation_count bigint;
BEGIN
    SELECT held_count, obligation_count
      INTO current_held_count, current_obligation_count
      FROM repo_watch_current_pull_request_work_count
     WHERE repository = counted_repository
       AND pull_request_number = counted_pull_request
     FOR UPDATE;

    IF NOT FOUND THEN
        IF held_delta < 0 OR obligation_delta < 0 THEN
            RAISE EXCEPTION
                'current pull-request work count is missing for % #% ',
                counted_repository, counted_pull_request;
        END IF;
        INSERT INTO repo_watch_current_pull_request_work_count (
            repository, pull_request_number, held_count, obligation_count
        ) VALUES (
            counted_repository, counted_pull_request, held_delta, obligation_delta
        );
        RETURN;
    END IF;

    adjusted_held_count := current_held_count + held_delta;
    adjusted_obligation_count := current_obligation_count + obligation_delta;
    IF adjusted_held_count < 0 OR adjusted_obligation_count < 0 THEN
        RAISE EXCEPTION
            'current pull-request work count underflow for % #% ',
            counted_repository, counted_pull_request;
    END IF;

    IF adjusted_held_count = 0 AND adjusted_obligation_count = 0 THEN
        DELETE FROM repo_watch_current_pull_request_work_count
         WHERE repository = counted_repository
           AND pull_request_number = counted_pull_request;
    ELSE
        UPDATE repo_watch_current_pull_request_work_count
           SET held_count = adjusted_held_count,
               obligation_count = adjusted_obligation_count
         WHERE repository = counted_repository
           AND pull_request_number = counted_pull_request;
    END IF;
END;
$$;


--
-- Name: clear_repo_watch_current_held_dispatch(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION clear_repo_watch_current_held_dispatch() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    DELETE FROM repo_watch_current_held_dispatch
     WHERE dispatch_id = NEW.dispatch_id;
    RETURN NULL;
END;
$$;


--
-- Name: decrement_repo_watch_repository_obligation_count(text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION decrement_repo_watch_repository_obligation_count(counted_repository text) RETURNS void
    LANGUAGE plpgsql
    AS $$
BEGIN
    UPDATE repo_watch_current_repository_obligation_count
       SET obligation_count = obligation_count - 1
     WHERE repository = counted_repository
       AND obligation_count > 1;
    IF FOUND THEN
        RETURN;
    END IF;

    DELETE FROM repo_watch_current_repository_obligation_count
     WHERE repository = counted_repository
       AND obligation_count = 1;
    IF NOT FOUND THEN
        RAISE EXCEPTION
            'outstanding repository-watch obligation count is missing for %',
            counted_repository;
    END IF;
END;
$$;


--
-- Name: guard_repo_watch_webhook_pending_mutation(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_repo_watch_webhook_pending_mutation() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'UPDATE' THEN
        RAISE EXCEPTION 'repo_watch_webhook_pending cannot be updated'
            USING ERRCODE = '23514';
    END IF;

    IF TG_OP = 'INSERT' THEN
        IF NOT EXISTS (
            SELECT 1
              FROM repo_watch_webhook_delivery AS delivery
             WHERE delivery.hook_id = NEW.hook_id
               AND delivery.delivery_id = NEW.delivery_id
               AND delivery.repository = NEW.repository
               AND delivery.receipt_sequence = NEW.receipt_sequence
        ) OR EXISTS (
            SELECT 1
              FROM repo_watch_webhook_disposition AS disposition
             WHERE disposition.hook_id = NEW.hook_id
               AND disposition.delivery_id = NEW.delivery_id
        ) THEN
            RAISE EXCEPTION
                'repo-watch webhook pending row requires its exact undispositioned delivery'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM repo_watch_webhook_disposition AS disposition
         WHERE disposition.hook_id = OLD.hook_id
           AND disposition.delivery_id = OLD.delivery_id
    ) THEN
        RAISE EXCEPTION
            'repo-watch webhook pending row retires only with its disposition'
            USING ERRCODE = '23514';
    END IF;
    RETURN OLD;
END;
$$;


--
-- Name: increment_repo_watch_pull_request_session_count(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION increment_repo_watch_pull_request_session_count() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.pull_request_number IS NOT NULL THEN
        INSERT INTO repo_watch_current_pull_request_session_count (
            repository, pull_request_number, session_count
        ) VALUES (
            NEW.repository, NEW.pull_request_number, 1
        )
        ON CONFLICT (repository, pull_request_number) DO UPDATE
            SET session_count =
                repo_watch_current_pull_request_session_count.session_count + 1;
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: increment_repo_watch_repository_obligation_count(text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION increment_repo_watch_repository_obligation_count(counted_repository text) RETURNS void
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO repo_watch_current_repository_obligation_count (
        repository, obligation_count
    ) VALUES (
        counted_repository, 1
    )
    ON CONFLICT (repository) DO UPDATE
        SET obligation_count =
            repo_watch_current_repository_obligation_count.obligation_count + 1;
END;
$$;


--
-- Name: maintain_repo_watch_pull_request_held_count(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION maintain_repo_watch_pull_request_held_count() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'INSERT' AND NEW.pull_request_number IS NOT NULL THEN
        PERFORM adjust_repo_watch_pull_request_work_count(
            NEW.repository, NEW.pull_request_number, 1, 0
        );
    ELSIF TG_OP = 'DELETE' AND OLD.pull_request_number IS NOT NULL THEN
        PERFORM adjust_repo_watch_pull_request_work_count(
            OLD.repository, OLD.pull_request_number, -1, 0
        );
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: maintain_repo_watch_pull_request_obligation_count(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION maintain_repo_watch_pull_request_obligation_count() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.settled_kind IS NULL THEN
            PERFORM adjust_repo_watch_latest_event_obligation_count(NEW.latest_event_id, 1);
        END IF;
        RETURN NULL;
    END IF;

    IF OLD.settled_kind IS NULL
       AND (NEW.settled_kind IS NOT NULL OR OLD.latest_event_id <> NEW.latest_event_id) THEN
        PERFORM adjust_repo_watch_latest_event_obligation_count(OLD.latest_event_id, -1);
    END IF;
    IF NEW.settled_kind IS NULL
       AND (OLD.settled_kind IS NOT NULL OR OLD.latest_event_id <> NEW.latest_event_id) THEN
        PERFORM adjust_repo_watch_latest_event_obligation_count(NEW.latest_event_id, 1);
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: maintain_repo_watch_repository_held_count(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION maintain_repo_watch_repository_held_count() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO repo_watch_current_repository_held_count (
            repository, held_count
        ) VALUES (
            NEW.repository, 1
        )
        ON CONFLICT (repository) DO UPDATE
            SET held_count =
                repo_watch_current_repository_held_count.held_count + 1;
        RETURN NULL;
    END IF;

    UPDATE repo_watch_current_repository_held_count
       SET held_count = held_count - 1
     WHERE repository = OLD.repository
       AND held_count > 1;
    IF FOUND THEN
        RETURN NULL;
    END IF;

    DELETE FROM repo_watch_current_repository_held_count
     WHERE repository = OLD.repository
       AND held_count = 1;
    IF NOT FOUND THEN
        RAISE EXCEPTION
            'current repository-watch held count is missing for %',
            OLD.repository;
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: maintain_repo_watch_repository_obligation_count(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION maintain_repo_watch_repository_obligation_count() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.settled_kind IS NULL THEN
            PERFORM increment_repo_watch_repository_obligation_count(NEW.repository);
        END IF;
        RETURN NULL;
    END IF;

    IF OLD.settled_kind IS NULL
       AND (NEW.settled_kind IS NOT NULL OR OLD.repository <> NEW.repository) THEN
        PERFORM decrement_repo_watch_repository_obligation_count(OLD.repository);
    END IF;
    IF NEW.settled_kind IS NULL
       AND (OLD.settled_kind IS NOT NULL OR OLD.repository <> NEW.repository) THEN
        PERFORM increment_repo_watch_repository_obligation_count(NEW.repository);
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: project_repo_watch_achieved_dispatch_settlement(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION project_repo_watch_achieved_dispatch_settlement() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO repo_watch_achieved_dispatch_settlement (
        dispatch_id, repository, pull_request_number, event_id, released_at
    )
    SELECT batch.dispatch_id, event.repository, event.pull_request_number,
           batch.event_id, NEW.released_at
      FROM repo_watch_dispatch_batch AS batch
      JOIN repo_watch_event AS event ON event.event_id = batch.event_id
     WHERE batch.dispatch_id = NEW.dispatch_id
       AND NOT EXISTS (
            SELECT 1
              FROM repo_watch_dispatch_action AS action
             WHERE action.dispatch_id = batch.dispatch_id
               AND NOT EXISTS (
                    SELECT 1
                      FROM repo_watch_dispatch_delivery AS delivery
                      JOIN goal_turn AS dispatched_turn
                        ON dispatched_turn.session_id = action.session_id
                       AND dispatched_turn.turn_id = delivery.turn_id
                      JOIN goal_event AS goal
                        ON goal.session_id = dispatched_turn.session_id
                       AND goal.generation = dispatched_turn.goal_generation
                       AND goal.event_ordinal = (
                            SELECT max(candidate.event_ordinal)
                              FROM goal_event AS candidate
                             WHERE candidate.session_id = dispatched_turn.session_id
                               AND candidate.generation = dispatched_turn.goal_generation
                       )
                       AND goal.event_kind = 'achieved'
                     WHERE delivery.dispatch_id = action.dispatch_id
                       AND delivery.action_ordinal = action.action_ordinal
               )
       )
    ON CONFLICT DO NOTHING;
    RETURN NULL;
END;
$$;


--
-- Name: project_repo_watch_current_held_dispatch(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION project_repo_watch_current_held_dispatch() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO repo_watch_current_held_dispatch (
        dispatch_id, repository, pull_request_number, rule_id, rule_version,
        singleton_scope, singleton_repository, singleton_pull_request_number,
        singleton_stack_root_pull_request_number, held_since
    ) VALUES (
        NEW.dispatch_id, NEW.repository, NEW.pull_request_number,
        NEW.rule_id, NEW.rule_version, NEW.singleton_scope,
        NEW.singleton_repository, NEW.singleton_pull_request_number,
        NEW.singleton_stack_root_pull_request_number, NEW.admitted_at
    );
    RETURN NULL;
END;
$$;


--
-- Name: project_repo_watch_singleton_cooldown(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION project_repo_watch_singleton_cooldown() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    released_batch repo_watch_dispatch_batch%ROWTYPE;
    projected_eligible_at timestamptz;
BEGIN
    SELECT * INTO STRICT released_batch
      FROM repo_watch_dispatch_batch
     WHERE dispatch_id = NEW.dispatch_id;
    IF released_batch.cooldown_seconds::numeric <= extract(epoch FROM (
        '294276-12-31 23:59:59+00'::timestamptz - NEW.released_at
    )) THEN
        projected_eligible_at := NEW.released_at
            + released_batch.cooldown_seconds * interval '1 second';
    ELSE
        projected_eligible_at := 'infinity'::timestamptz;
    END IF;

    INSERT INTO repo_watch_current_singleton_cooldown (
        rule_id, rule_version, singleton_scope, singleton_repository,
        singleton_pull_request_number, singleton_stack_root_pull_request_number, eligible_at
    ) VALUES (
        released_batch.rule_id, released_batch.rule_version,
        released_batch.singleton_scope, released_batch.singleton_repository,
        released_batch.singleton_pull_request_number,
        released_batch.singleton_stack_root_pull_request_number, projected_eligible_at
    )
    ON CONFLICT (rule_id, rule_version, singleton_scope, singleton_repository,
                 singleton_pull_request_number, singleton_stack_root_pull_request_number)
        DO UPDATE SET eligible_at = GREATEST(
            repo_watch_current_singleton_cooldown.eligible_at, EXCLUDED.eligible_at
        );
    RETURN NULL;
END;
$$;


--
-- Name: register_repo_watch_webhook_pending(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION register_repo_watch_webhook_pending() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO repo_watch_webhook_pending (
        hook_id, delivery_id, repository, receipt_sequence
    ) VALUES (
        NEW.hook_id, NEW.delivery_id, NEW.repository, NEW.receipt_sequence
    );
    RETURN NULL;
END;
$$;


--
-- Name: reject_commissioned_dispatch_table_truncate(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_commissioned_dispatch_table_truncate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION '% cannot be truncated', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: reject_repo_watch_table_truncate(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_repo_watch_table_truncate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION '% cannot be truncated', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: remember_repo_watch_repository_key(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION remember_repo_watch_repository_key() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO repo_watch_repository_key (repository)
    VALUES (NEW.repository)
    ON CONFLICT DO NOTHING;
    RETURN NULL;
END;
$$;


--
-- Name: repo_watch_branch_is_valid(text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_branch_is_valid(candidate text) RETURNS boolean
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    AS $$
    SELECT octet_length(candidate) BETWEEN 1 AND 255
       AND candidate <> '@'
       AND left(candidate, 1) <> '-'
       AND right(candidate, 1) <> '.'
       AND strpos(candidate, '..') = 0
       AND strpos(candidate, '@{') = 0
       AND NOT EXISTS (
            SELECT 1
            FROM generate_series(1, length(candidate)) AS character(position)
            WHERE ascii(substr(candidate, character.position, 1)) <= 32
               OR ascii(substr(candidate, character.position, 1)) = 127
               OR substr(candidate, character.position, 1)
                    = ANY (ARRAY['~', '^', ':', '?', '*', '[', chr(92)])
       )
       AND NOT EXISTS (
            SELECT 1
            FROM unnest(string_to_array(candidate, '/')) AS part(value)
            WHERE part.value = ''
               OR left(part.value, 1) = '.'
               OR right(part.value, 5) = '.lock'
       )
$$;


--
-- Name: repo_watch_dispatch_attempt_budget(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_dispatch_attempt_budget() RETURNS bigint
    LANGUAGE sql IMMUTABLE
    AS $$
    SELECT 6::bigint;
$$;


--
-- Name: repo_watch_dispatch_batch_has_complete_actions(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_dispatch_batch_has_complete_actions() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    candidate_dispatch_id uuid;
    expected_count integer;
    actual_count bigint;
    first_ordinal integer;
    last_ordinal integer;
BEGIN
    candidate_dispatch_id := COALESCE(NEW.dispatch_id, OLD.dispatch_id);
    SELECT batch.action_count
      INTO expected_count
      FROM repo_watch_dispatch_batch AS batch
     WHERE batch.dispatch_id = candidate_dispatch_id;
    IF expected_count IS NULL THEN
        RETURN NULL;
    END IF;
    SELECT count(*), min(action.action_ordinal), max(action.action_ordinal)
      INTO actual_count, first_ordinal, last_ordinal
      FROM repo_watch_dispatch_action AS action
     WHERE action.dispatch_id = candidate_dispatch_id;
    IF actual_count <> expected_count
       OR first_ordinal <> 1
       OR last_ordinal <> expected_count THEN
        RAISE EXCEPTION 'repository-watch dispatch action inventory is incomplete'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: repo_watch_dispatch_singleton_lock_key(text, bigint, text, text, numeric, numeric); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_dispatch_singleton_lock_key(rule_id text, rule_version bigint, singleton_scope text, singleton_repository text, singleton_pull_request_number numeric, singleton_stack_root_pull_request_number numeric) RETURNS text
    LANGUAGE sql IMMUTABLE
    AS $$
    SELECT concat_ws(
        E'\x1f',
        'repo-watch',
        rule_id,
        rule_version::text,
        singleton_scope,
        coalesce(singleton_repository, ''),
        coalesce(singleton_pull_request_number::text, ''),
        coalesce(singleton_stack_root_pull_request_number::text, '')
    );
$$;


--
-- Name: repo_watch_labels_are_valid(text[]); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_labels_are_valid(candidate text[]) RETURNS boolean
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    AS $$
    SELECT COALESCE(array_ndims(candidate), 1) = 1
       AND COALESCE(array_lower(candidate, 1), 1) = 1
       AND candidate = ARRAY(
            SELECT DISTINCT label.value COLLATE "C"
            FROM unnest(candidate) AS label(value)
            ORDER BY label.value COLLATE "C"
       )
       AND NOT EXISTS (
            SELECT 1
            FROM unnest(candidate) AS label(value)
            WHERE value IS NULL
               OR octet_length(value) NOT BETWEEN 1 AND 200
               OR char_length(value) > 50
       )
$$;


--
-- Name: repo_watch_login_is_valid(text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_login_is_valid(candidate text) RETURNS boolean
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    AS $_$
    SELECT candidate = lower(candidate)
       AND octet_length(candidate) BETWEEN 1 AND 44
       AND octet_length(normalized.base) BETWEEN 1 AND 39
       AND normalized.base COLLATE "C" ~ '^[a-z0-9_-]+$'
       AND left(normalized.base, 1) <> '-'
       AND right(normalized.base, 1) <> '-'
       AND strpos(normalized.base, '--') = 0
    FROM (
        SELECT CASE
            WHEN right(candidate, 5) = '[bot]'
                THEN left(candidate, length(candidate) - 5)
            ELSE candidate
        END AS base
    ) AS normalized
$_$;


--
-- Name: repo_watch_owe_dispatch_requeue(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_owe_dispatch_requeue(candidate_dispatch_id uuid, completed_session_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    candidate_repository text;
    candidate_rule_id text;
    candidate_rule_version bigint;
    candidate_singleton_key text;
    terminal_goal_kind text;
    owed_obligation_id uuid;
BEGIN
    -- The terminal event that matters is the one ending the generation this
    -- dispatch commissioned, not whatever the session is doing now. A session
    -- whose sibling action still holds the batch unreleased may legally accept
    -- an unrelated successor goal, and reading that successor's termination
    -- would owe a requeue for work the dispatched generation already converged.
    SELECT current_goal.event_kind
      INTO terminal_goal_kind
      FROM goal_event AS current_goal
     WHERE current_goal.session_id = completed_session_id
       AND current_goal.generation = (
            SELECT dispatched_turn.goal_generation
              FROM repo_watch_dispatch_action AS action
              JOIN repo_watch_dispatch_delivery AS delivery
                ON delivery.dispatch_id = action.dispatch_id
               AND delivery.action_ordinal = action.action_ordinal
              JOIN goal_turn AS dispatched_turn
                ON dispatched_turn.session_id = action.session_id
               AND dispatched_turn.turn_id = delivery.turn_id
             WHERE action.dispatch_id = candidate_dispatch_id
               AND action.session_id = completed_session_id
       )
     ORDER BY current_goal.event_ordinal DESC
     LIMIT 1;

    SELECT origin.repository, batch.rule_id, batch.rule_version,
           repo_watch_dispatch_singleton_lock_key(
                batch.rule_id,
                batch.rule_version,
                batch.singleton_scope,
                batch.singleton_repository,
                batch.singleton_pull_request_number,
                batch.singleton_stack_root_pull_request_number
           )
      INTO candidate_repository, candidate_rule_id, candidate_rule_version,
           candidate_singleton_key
      FROM repo_watch_dispatch_batch AS batch
      JOIN repo_watch_event AS origin ON origin.event_id = batch.event_id
     WHERE batch.dispatch_id = candidate_dispatch_id;

    -- Termination runs inside the transaction that ends the goal, which
    -- already holds that session's row. Lifecycle-cutoff processing takes
    -- the repository advisory key and then waits for the same session row,
    -- so taking the repository key here would invert that order and deadlock
    -- a goal pass against a cutoff attempt. Termination therefore takes only
    -- keys no repository-key holder waits behind a session row for.
    --
    -- Fresh evaluation and obligation admission take the singleton key, so a
    -- match racing this termination waits and then joins the obligation
    -- through its settle-guarded update, rather than aborting on the
    -- active-singleton index. Deactivation is serialized by row: inserting
    -- into repo_watch_rule_deactivation takes a key-share lock on the
    -- activation row this locks exclusively, so the two cannot both pass
    -- their checks against a snapshot predating the other's row.
    PERFORM pg_advisory_xact_lock(hashtextextended(candidate_singleton_key, 0));

    PERFORM 1
      FROM repo_watch_rule_activation AS activation
     WHERE activation.repository = candidate_repository
       AND activation.rule_id = candidate_rule_id
       AND activation.rule_version = candidate_rule_version
       FOR UPDATE;

    -- A terminal session leaves the current dispatch state owed unless it
    -- achieved against state that is still current. The active-singleton
    -- index collapses sibling terminations and preserves any later matching
    -- event already recorded by ordinary evaluation.
    WITH owed AS (
    INSERT INTO repo_watch_dispatch_obligation
        (obligation_id, repository, rule_id, rule_version,
         singleton_scope, singleton_repository, singleton_pull_request_number,
         singleton_stack_root_pull_request_number, first_repository,
         first_event_id, latest_event_id, matched_event_count,
         blocking_dispatch_id, failed_attempts, last_failed_attempt_at,
         counted_dispatch_id)
    SELECT gen_random_uuid(), origin.repository, batch.rule_id,
           batch.rule_version, batch.singleton_scope,
           batch.singleton_repository, batch.singleton_pull_request_number,
           batch.singleton_stack_root_pull_request_number, origin.repository,
           origin.event_id, origin.event_id, 1, batch.dispatch_id,
           -- The settled predecessor carries the lineage count; a dispatch
           -- admitted from a fresh match settled none and starts the lineage.
           coalesce(settled.failed_attempts, 0) + 1,
           clock_timestamp(),
           batch.dispatch_id
      FROM repo_watch_dispatch_batch AS batch
      JOIN repo_watch_event AS origin ON origin.event_id = batch.event_id
      LEFT JOIN repo_watch_event AS delivered
        ON delivered.event_id = batch.delivered_state_event_id
      LEFT JOIN repo_watch_dispatch_obligation AS settled
        ON settled.settled_dispatch_id = batch.dispatch_id
     WHERE batch.dispatch_id = candidate_dispatch_id
       AND EXISTS (
            SELECT 1
              FROM repo_watch_dispatch_action AS action
             WHERE action.dispatch_id = batch.dispatch_id
               AND action.session_id = completed_session_id
       )
       AND terminal_goal_kind IN ('blocked', 'achieved', 'user_stopped')
       -- A branch target carries no durable revision at all: its only event
       -- kind records a workflow conclusion, so achievement is its own seal.
       -- A pull-request target seals only when the state this batch delivered is
       -- known and is still the pull request's latest durable head.
       AND NOT (
            terminal_goal_kind = 'achieved'
            AND (
                origin.target_kind <> 'pull_request'
                OR (
                    batch.delivered_state_event_id IS NOT NULL
                    AND delivered.head_sha = (
                        SELECT current_state.head_sha
                          FROM repo_watch_event AS current_state
                         WHERE current_state.repository = origin.repository
                           AND current_state.pull_request_number
                                = origin.pull_request_number
                         ORDER BY current_state.cursor_generation DESC,
                                  current_state.event_ordinal DESC
                         LIMIT 1
                    )
                )
            )
       )
       AND NOT EXISTS (
            SELECT 1
              FROM repo_watch_rule_deactivation AS deactivation
             WHERE deactivation.repository = origin.repository
               AND deactivation.rule_id = batch.rule_id
               AND deactivation.rule_version = batch.rule_version
       )
       -- A later close or merge makes outstanding work stale. The cutoff event
       -- itself is the fact a rule may match, not work invalidated by that
       -- fact, so a dispatch of the latest cutoff keeps its requeue -- but only
       -- while that cutoff is still latest. A reopen makes the close obsolete,
       -- and requeueing it would run close automation against an open pull
       -- request, so the opened arm admits nonterminal origins only.
       AND (
            origin.target_kind <> 'pull_request'
            OR EXISTS (
                SELECT 1
                  FROM (
                        SELECT lifecycle.event_id, lifecycle.event_kind
                          FROM repo_watch_event AS lifecycle
                         WHERE lifecycle.repository = origin.repository
                           AND lifecycle.pull_request_number
                                = origin.pull_request_number
                           AND lifecycle.event_kind IN (
                                'pull_request_opened',
                                'pull_request_closed',
                                'pull_request_merged'
                           )
                         ORDER BY lifecycle.cursor_generation DESC,
                                  lifecycle.event_ordinal DESC
                         LIMIT 1
                  ) AS latest_lifecycle
                 WHERE (
                        latest_lifecycle.event_kind = 'pull_request_opened'
                        AND origin.event_kind NOT IN (
                            'pull_request_closed',
                            'pull_request_merged'
                        )
                 )
                    OR latest_lifecycle.event_id = origin.event_id
            )
       )
    -- Only an active obligation on this singleton may absorb a termination.
    -- A bare conflict clause would also swallow an identifier collision with
    -- an already settled obligation and silently drop the requeue.
    ON CONFLICT (rule_id, rule_version, singleton_scope, singleton_repository,
                 singleton_pull_request_number,
                 singleton_stack_root_pull_request_number)
        WHERE settled_kind IS NULL
    -- The absorbing obligation carries the lineage count forward. GREATEST
    -- rather than addition because a match that opened the row while the batch
    -- ran contributes a count of zero that must not erase the lineage. The
    -- WHERE is what makes one batch one attempt: the second and later siblings
    -- of the same batch find their own identifier already recorded and change
    -- nothing, so neither the count nor a park release taken since the first
    -- sibling terminated is disturbed.
    DO UPDATE SET
        failed_attempts = GREATEST(
            repo_watch_dispatch_obligation.failed_attempts,
            EXCLUDED.failed_attempts
        ),
        last_failed_attempt_at = clock_timestamp(),
        counted_dispatch_id = EXCLUDED.counted_dispatch_id
    WHERE repo_watch_dispatch_obligation.counted_dispatch_id
           IS DISTINCT FROM EXCLUDED.counted_dispatch_id
    RETURNING obligation_id
    )
    SELECT owed.obligation_id INTO owed_obligation_id FROM owed;

    -- Same transaction as the count that exhausted the budget, so an
    -- obligation is never readable with its budget spent and its parked state
    -- still unwritten.
    IF owed_obligation_id IS NOT NULL THEN
        PERFORM repo_watch_park_exhausted_dispatch_obligation(owed_obligation_id);
    END IF;
END;
$$;


--
-- Name: repo_watch_park_exhausted_dispatch_obligation(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_park_exhausted_dispatch_obligation(subject_obligation_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    parked_attempts bigint;
    progress_event_id uuid;
BEGIN
    -- The state the exhausting attempt was actually given, not the obligation's
    -- latest-event projection: a fresh match arriving while that attempt ran
    -- coalesces into the same row and advances the projection, so parking
    -- against it would park state no attempt has been spent on and would hide
    -- that state from the progress test below. Only a batch admitted before the
    -- delivered state was recorded has none, and there the projection is all
    -- there is.
    UPDATE repo_watch_dispatch_obligation
       SET parked_at = clock_timestamp(),
           parked_state_event_id = coalesce(
                (
                    SELECT batch.delivered_state_event_id
                      FROM repo_watch_dispatch_batch AS batch
                     WHERE batch.dispatch_id = counted_dispatch_id
                ),
                latest_event_id
           )
     WHERE obligation_id = subject_obligation_id
       AND settled_kind IS NULL
       AND parked_at IS NULL
       AND failed_attempts >= repo_watch_dispatch_attempt_budget()
    RETURNING failed_attempts INTO parked_attempts;

    IF NOT FOUND THEN
        RETURN;
    END IF;

    PERFORM repo_watch_record_dispatch_obligation_park_transition(
        subject_obligation_id,
        'parked',
        parked_attempts,
        NULL::text,
        NULL::uuid,
        NULL::text
    );

    -- The pull request may have moved while the exhausting attempt ran, under
    -- an event that reached its evaluation before there was a park to release.
    -- Nothing restates that fact afterwards, so it is read from the durable
    -- record here. Parking first and releasing from that fact, rather than
    -- refusing to park, is what spends it: the lineage keeps its budget, and
    -- the same fact cannot buy a second one at the next exhaustion.
    SELECT later.event_id
      INTO progress_event_id
      FROM repo_watch_dispatch_obligation AS obligation
      JOIN repo_watch_event AS parked_state
        ON parked_state.event_id = obligation.parked_state_event_id
      JOIN repo_watch_event AS later
        ON later.repository = parked_state.repository
       AND later.pull_request_number = parked_state.pull_request_number
     WHERE obligation.obligation_id = subject_obligation_id
       AND parked_state.target_kind = 'pull_request'
       AND later.target_kind = 'pull_request'
       AND (later.cursor_generation, later.event_ordinal)
            > (parked_state.cursor_generation, parked_state.event_ordinal)
       AND (
            later.head_sha IS DISTINCT FROM parked_state.head_sha
            OR later.event_kind IN (
                'review_submitted', 'thread_opened', 'thread_resolved'
            )
       )
       -- Already spent by this lineage, by identity anywhere in it, or by
       -- order within the pull request this fact is about. Several facts can
       -- follow one stalled state, this selection takes the newest, and the
       -- older ones stay unevaluated by rules that lag; without the ordering
       -- arm one of those could release a later park after a newer fact had
       -- already been spent on it.
       --
       -- The ordering arm is confined to one pull request because
       -- (cursor_generation, event_ordinal) numbers a single repository's
       -- stream: repo_watch_cursor is keyed by (repository, generation), so
       -- tuples from two repositories are incomparable, and a rule-scoped
       -- lineage spans them. Comparing across would let a repository that had
       -- reached a high generation hold a lineage parked on a repository whose
       -- own numbering is lower. Identity still spans the whole lineage, since
       -- an event identifier means the same thing everywhere.
       AND NOT EXISTS (
            SELECT 1
              FROM repo_watch_dispatch_obligation_park AS spent
              JOIN repo_watch_dispatch_obligation AS spent_on
                ON spent_on.obligation_id = spent.obligation_id
              JOIN repo_watch_event AS spent_event
                ON spent_event.event_id = spent.release_event_id
             WHERE (
                    spent_event.event_id = later.event_id
                    OR (
                        spent_event.repository = later.repository
                        AND spent_event.pull_request_number
                             = later.pull_request_number
                        AND (
                            spent_event.cursor_generation,
                            spent_event.event_ordinal
                        ) >= (later.cursor_generation, later.event_ordinal)
                    )
               )
               AND spent_on.rule_id = obligation.rule_id
               AND spent_on.rule_version = obligation.rule_version
               AND spent_on.singleton_scope = obligation.singleton_scope
               AND spent_on.singleton_repository
                    IS NOT DISTINCT FROM obligation.singleton_repository
               AND spent_on.singleton_pull_request_number
                    IS NOT DISTINCT FROM obligation.singleton_pull_request_number
               AND spent_on.singleton_stack_root_pull_request_number
                    IS NOT DISTINCT FROM
                        obligation.singleton_stack_root_pull_request_number
       )
     ORDER BY later.cursor_generation DESC, later.event_ordinal DESC
     LIMIT 1;

    IF progress_event_id IS NOT NULL THEN
        PERFORM repo_watch_release_dispatch_obligation_park_for_progress(
            subject_obligation_id,
            progress_event_id
        );
    END IF;
END;
$$;


--
-- Name: repo_watch_record_dispatch_obligation_park_transition(uuid, text, bigint, text, uuid, text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_record_dispatch_obligation_park_transition(subject_obligation_id uuid, kind text, attempts bigint, reason text, progress_event_id uuid, actor text) RETURNS void
    LANGUAGE sql
    AS $$
    INSERT INTO repo_watch_dispatch_obligation_park
        (obligation_id, transition_ordinal, transition_kind, failed_attempts,
         release_reason, release_event_id, release_actor)
    SELECT subject_obligation_id,
           coalesce(
               (SELECT max(park.transition_ordinal)
                  FROM repo_watch_dispatch_obligation_park AS park
                 WHERE park.obligation_id = subject_obligation_id),
               0
           ) + 1,
           kind,
           attempts,
           reason,
           progress_event_id,
           actor;
$$;


--
-- Name: repo_watch_release_completed_dispatch_batches(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_release_completed_dispatch_batches() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.state_kind <> 'terminal' OR OLD.state_kind = 'terminal' THEN
        RETURN NULL;
    END IF;
    PERFORM repo_watch_release_completed_dispatch_batches_for_turn(
        NEW.turn_id,
        NEW.session_id
    );
    RETURN NULL;
END;
$$;


--
-- Name: repo_watch_release_completed_dispatch_batches_for_goal(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_release_completed_dispatch_batches_for_goal() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.event_kind NOT IN ('blocked', 'achieved', 'user_stopped') THEN
        RETURN NULL;
    END IF;
    PERFORM repo_watch_release_completed_dispatch_batches_for_turn(
        NULL::uuid,
        NEW.session_id
    );
    RETURN NULL;
END;
$$;


--
-- Name: repo_watch_release_completed_dispatch_batches_for_turn(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_release_completed_dispatch_batches_for_turn(completed_turn_id uuid, completed_session_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    candidate_dispatch_id uuid;
BEGIN
    FOR candidate_dispatch_id IN
        SELECT DISTINCT action.dispatch_id
          FROM repo_watch_dispatch_action AS action
         WHERE action.session_id = completed_session_id
         ORDER BY action.dispatch_id
    LOOP
        -- A released batch has already accounted for its dispatched work. The
        -- session may afterwards accept an unrelated successor goal, whose own
        -- termination reaches this trigger through the same action link; owing a
        -- requeue for it would redispatch a repository-watch event that already
        -- converged. The release below is written after this call, so a batch
        -- releasing now is still unreleased here.
        IF NOT EXISTS (
            SELECT 1
              FROM repo_watch_dispatch_release AS released
             WHERE released.dispatch_id = candidate_dispatch_id
        ) THEN
            PERFORM repo_watch_owe_dispatch_requeue(
                candidate_dispatch_id,
                completed_session_id
            );
        END IF;

        PERFORM 1
          FROM repo_watch_dispatch_batch AS locked_batch
         WHERE locked_batch.dispatch_id = candidate_dispatch_id
           FOR UPDATE;

        INSERT INTO repo_watch_dispatch_release (dispatch_id, released_at)
        SELECT batch.dispatch_id, clock_timestamp()
          FROM repo_watch_dispatch_batch AS batch
         WHERE batch.dispatch_id = candidate_dispatch_id
           AND NOT EXISTS (
                SELECT 1
                  FROM repo_watch_dispatch_release AS released
                 WHERE released.dispatch_id = batch.dispatch_id
           )
           AND batch.action_count = (
                SELECT count(*)
                  FROM repo_watch_dispatch_action AS action
                  JOIN repo_watch_dispatch_delivery AS delivery
                    ON delivery.dispatch_id = action.dispatch_id
                   AND delivery.action_ordinal = action.action_ordinal
                  JOIN turn_lifecycle AS turn
                    ON turn.turn_id = delivery.turn_id
                   AND (
                        turn.state_kind = 'terminal'
                        OR turn.turn_id = completed_turn_id
                        OR NOT goal_turn_is_runtime_relevant(
                            turn.session_id,
                            turn.turn_id
                        )
                   )
                 WHERE action.dispatch_id = batch.dispatch_id
                   AND NOT EXISTS (
                        SELECT 1
                          FROM turn_lifecycle AS live_turn
                         WHERE live_turn.session_id = action.session_id
                           AND live_turn.state_kind <> 'terminal'
                           AND goal_turn_is_runtime_relevant(
                                live_turn.session_id,
                                live_turn.turn_id
                           )
                           AND (
                                completed_turn_id IS NULL
                                OR live_turn.turn_id <> completed_turn_id
                           )
                   )
                   AND NOT EXISTS (
                        SELECT 1
                          FROM goal_event AS current_goal
                         WHERE current_goal.session_id = action.session_id
                           AND current_goal.event_ordinal = (
                                SELECT max(candidate.event_ordinal)
                                  FROM goal_event AS candidate
                                 WHERE candidate.session_id = action.session_id
                           )
                           AND current_goal.event_kind IN (
                                'commissioned', 'resumed', 'superseded'
                           )
                   )
           )
        ON CONFLICT DO NOTHING;
    END LOOP;
END;
$$;


--
-- Name: repo_watch_release_dispatch_obligation_park_for_progress(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_release_dispatch_obligation_park_for_progress(parked_obligation_id uuid, progress_event_id uuid) RETURNS boolean
    LANGUAGE plpgsql
    AS $$
DECLARE
    parked_attempts bigint;
BEGIN
    SELECT obligation.failed_attempts
      INTO parked_attempts
      FROM repo_watch_dispatch_obligation AS obligation
     WHERE obligation.obligation_id = parked_obligation_id
       AND obligation.settled_kind IS NULL
       AND obligation.parked_at IS NOT NULL
       FOR UPDATE;

    IF NOT FOUND THEN
        RETURN false;
    END IF;

    PERFORM repo_watch_record_dispatch_obligation_park_transition(
        parked_obligation_id,
        'released',
        parked_attempts,
        'pull_request_progress',
        progress_event_id,
        NULL::text
    );

    UPDATE repo_watch_dispatch_obligation
       SET parked_at = NULL,
           parked_state_event_id = NULL,
           failed_attempts = 0,
           last_failed_attempt_at = NULL
     WHERE obligation_id = parked_obligation_id;

    RETURN true;
END;
$$;


--
-- Name: repo_watch_release_dispatch_obligation_parks_for_event(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_release_dispatch_obligation_parks_for_event(progress_event_id uuid) RETURNS bigint
    LANGUAGE plpgsql
    AS $$
DECLARE
    released_count bigint := 0;
    parked_obligation_id uuid;
BEGIN
    FOR parked_obligation_id IN
        SELECT obligation.obligation_id
          FROM repo_watch_dispatch_obligation AS obligation
          JOIN repo_watch_event AS parked_state
            ON parked_state.event_id = obligation.parked_state_event_id
          JOIN repo_watch_event AS incoming
            ON incoming.event_id = progress_event_id
         WHERE obligation.settled_kind IS NULL
           AND obligation.parked_at IS NOT NULL
           AND parked_state.target_kind = 'pull_request'
           AND incoming.target_kind = 'pull_request'
           AND incoming.repository = parked_state.repository
           AND incoming.pull_request_number = parked_state.pull_request_number
           AND (
                parked_state.head_sha IS DISTINCT FROM incoming.head_sha
                OR incoming.event_kind IN (
                    'review_submitted', 'thread_opened', 'thread_resolved'
                )
           )
           -- Progress is what follows the stalled state. One repository event
           -- is evaluated once per active rule, so without this a rule lagging
           -- behind its siblings would eventually replay an older event against
           -- a newer park and hand back a budget the pull request never earned.
           AND (incoming.cursor_generation, incoming.event_ordinal)
                > (parked_state.cursor_generation, parked_state.event_ordinal)
           -- And one event buys one release per lineage. The evaluations of a
           -- single event are spread across rules, so a lineage that parks
           -- again between two of them would otherwise be released twice by the
           -- same fact. The spend is read across the whole lineage rather than
           -- the row holding it, because settlement retires that row and the
           -- requeue opens a successor: a lineage that parked, dispatched, and
           -- exhausted itself again would otherwise take a second full budget
           -- from a fact it had already spent.
           -- Already spent by this lineage: by identity anywhere in it, or by
           -- order within the pull request this fact is about. Identity alone
           -- would let a rule lagging behind its siblings release a later park
           -- with a fact older than one already spent; ordering across
           -- repositories would be meaningless, because
           -- (cursor_generation, event_ordinal) numbers one repository's
           -- stream and a rule-scoped lineage spans several.
           AND NOT EXISTS (
                SELECT 1
                  FROM repo_watch_dispatch_obligation_park AS spent
                  JOIN repo_watch_dispatch_obligation AS spent_on
                    ON spent_on.obligation_id = spent.obligation_id
                  JOIN repo_watch_event AS spent_event
                    ON spent_event.event_id = spent.release_event_id
                 WHERE (
                        spent_event.event_id = incoming.event_id
                        OR (
                            spent_event.repository = incoming.repository
                            AND spent_event.pull_request_number
                                 = incoming.pull_request_number
                            AND (
                                spent_event.cursor_generation,
                                spent_event.event_ordinal
                            ) >= (
                                incoming.cursor_generation,
                                incoming.event_ordinal
                            )
                        )
                   )
                   AND spent_on.rule_id = obligation.rule_id
                   AND spent_on.rule_version = obligation.rule_version
                   AND spent_on.singleton_scope = obligation.singleton_scope
                   AND spent_on.singleton_repository
                        IS NOT DISTINCT FROM obligation.singleton_repository
                   AND spent_on.singleton_pull_request_number
                        IS NOT DISTINCT FROM obligation.singleton_pull_request_number
                   AND spent_on.singleton_stack_root_pull_request_number
                        IS NOT DISTINCT FROM
                            obligation.singleton_stack_root_pull_request_number
           )
         ORDER BY obligation.obligation_id
    LOOP
        IF repo_watch_release_dispatch_obligation_park_for_progress(
               parked_obligation_id,
               progress_event_id
           ) THEN
            released_count := released_count + 1;
        END IF;
    END LOOP;
    RETURN released_count;
END;
$$;


--
-- Name: repo_watch_release_parked_dispatch_obligation(uuid, text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_release_parked_dispatch_obligation(parked_obligation_id uuid, releasing_actor text) RETURNS boolean
    LANGUAGE plpgsql
    AS $$
DECLARE
    parked_attempts bigint;
BEGIN
    SELECT obligation.failed_attempts
      INTO parked_attempts
      FROM repo_watch_dispatch_obligation AS obligation
     WHERE obligation.obligation_id = parked_obligation_id
       AND obligation.settled_kind IS NULL
       AND obligation.parked_at IS NOT NULL
       FOR UPDATE;

    IF NOT FOUND THEN
        RETURN false;
    END IF;

    PERFORM repo_watch_record_dispatch_obligation_park_transition(
        parked_obligation_id,
        'released',
        parked_attempts,
        'operator',
        NULL,
        releasing_actor
    );

    UPDATE repo_watch_dispatch_obligation
       SET parked_at = NULL,
           parked_state_event_id = NULL,
           failed_attempts = 0,
           last_failed_attempt_at = NULL
     WHERE obligation_id = parked_obligation_id;

    RETURN true;
END;
$$;


--
-- Name: repo_watch_repository_is_valid(text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_repository_is_valid(candidate text) RETURNS boolean
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    AS $_$
    SELECT octet_length(candidate) BETWEEN 1 AND 201
       AND candidate = lower(candidate)
       AND candidate COLLATE "C" ~ '^[a-z0-9_.-]+/[a-z0-9_.-]+$'
       AND split_part(candidate, '/', 1) NOT IN ('.', '..')
       AND split_part(candidate, '/', 2) NOT IN ('.', '..')
$_$;


--
-- Name: repo_watch_restart_dispatch_obligation_backoff(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_restart_dispatch_obligation_backoff() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    UPDATE repo_watch_dispatch_obligation AS obligation
       SET last_failed_attempt_at = clock_timestamp()
      FROM repo_watch_dispatch_batch AS batch
     WHERE batch.dispatch_id = NEW.dispatch_id
       AND obligation.rule_id = batch.rule_id
       AND obligation.rule_version = batch.rule_version
       AND obligation.singleton_scope = batch.singleton_scope
       AND obligation.singleton_repository
            IS NOT DISTINCT FROM batch.singleton_repository
       AND obligation.singleton_pull_request_number
            IS NOT DISTINCT FROM batch.singleton_pull_request_number
       AND obligation.singleton_stack_root_pull_request_number
            IS NOT DISTINCT FROM batch.singleton_stack_root_pull_request_number
       AND obligation.settled_kind IS NULL
       AND obligation.failed_attempts > 0;
    RETURN NULL;
END;
$$;


--
-- Name: repo_watch_rule_id_is_valid(text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_rule_id_is_valid(candidate text) RETURNS boolean
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    AS $_$
    SELECT octet_length(candidate) BETWEEN 1 AND 128
       AND candidate COLLATE "C" ~ '^[A-Za-z0-9._-]+$'
$_$;


--
-- Name: repo_watch_settle_deactivated_dispatch_obligations(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_settle_deactivated_dispatch_obligations() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    UPDATE repo_watch_dispatch_obligation
       SET settled_kind = 'deactivated',
           settled_at = clock_timestamp()
     WHERE repository = NEW.repository
       AND rule_id = NEW.rule_id
       AND rule_version = NEW.rule_version
       AND settled_kind IS NULL;
    RETURN NULL;
END;
$$;


--
-- Name: repo_watch_stamp_dispatch_batch_delivered_state(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION repo_watch_stamp_dispatch_batch_delivered_state() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    -- Admission coalesces a fresh match into an outstanding obligation instead
    -- of dispatching it, and settles an obligation only after inserting its
    -- successor batch. An unsettled obligation on this singleton therefore
    -- identifies the successor exactly. The batch table is append-only, so the
    -- delivered state is recorded here rather than stamped after settlement.
    IF NOT EXISTS (
        SELECT 1
          FROM repo_watch_dispatch_obligation AS obligation
         WHERE obligation.rule_id = NEW.rule_id
           AND obligation.rule_version = NEW.rule_version
           AND obligation.singleton_scope = NEW.singleton_scope
           AND obligation.singleton_repository
                IS NOT DISTINCT FROM NEW.singleton_repository
           AND obligation.singleton_pull_request_number
                IS NOT DISTINCT FROM NEW.singleton_pull_request_number
           AND obligation.singleton_stack_root_pull_request_number
                IS NOT DISTINCT FROM NEW.singleton_stack_root_pull_request_number
           AND obligation.settled_kind IS NULL
    ) THEN
        NEW.delivered_state_event_id := NEW.event_id;
        RETURN NEW;
    END IF;
    SELECT (
            SELECT state.event_id
              FROM repo_watch_event AS state
             WHERE state.repository = origin.repository
               AND state.pull_request_number = origin.pull_request_number
             ORDER BY state.cursor_generation DESC,
                      state.event_ordinal DESC
             LIMIT 1
    )
      INTO NEW.delivered_state_event_id
      FROM repo_watch_event AS origin
     WHERE origin.event_id = NEW.event_id;
    RETURN NEW;
END;
$$;


--
-- Name: require_repo_watch_event_cursor_commit(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_repo_watch_event_cursor_commit() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM repo_watch_cursor
         WHERE repository = NEW.repository
           AND generation = NEW.cursor_generation
           AND recording_transaction_id = pg_current_xact_id()
    ) THEN
        RAISE EXCEPTION
            'repository-watch event requires its cursor commit transaction'
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'repo_watch_event_requires_current_cursor_transaction';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: retain_repo_watch_webhook_payload_until_expired(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION retain_repo_watch_webhook_payload_until_expired() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'UPDATE' THEN
        RAISE EXCEPTION 'repo_watch_webhook_payload is append-only'
            USING ERRCODE = '23514';
    END IF;
    IF NOT EXISTS (
        SELECT 1
          FROM repo_watch_webhook_delivery AS delivery
          JOIN repo_watch_webhook_disposition AS disposition
            ON disposition.hook_id = delivery.hook_id
           AND disposition.delivery_id = delivery.delivery_id
         WHERE delivery.hook_id = OLD.hook_id
           AND delivery.delivery_id = OLD.delivery_id
           AND disposition.recorded_at <= statement_timestamp() - interval '7 days'
    ) THEN
        RAISE EXCEPTION
            'repo-watch webhook payload is not terminal and seven days old'
            USING ERRCODE = '23514';
    END IF;
    RETURN OLD;
END;
$$;


--
-- Name: retire_repo_watch_webhook_pending(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION retire_repo_watch_webhook_pending() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    DELETE FROM repo_watch_webhook_pending
     WHERE hook_id = NEW.hook_id
       AND delivery_id = NEW.delivery_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION
            'repo-watch webhook disposition requires its pending row'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: stamp_repo_watch_cursor_transaction(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION stamp_repo_watch_cursor_transaction() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    NEW.recording_transaction_id := pg_current_xact_id();
    RETURN NEW;
END;
$$;


--
-- Name: stamp_repo_watch_dispatch_action_target(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION stamp_repo_watch_dispatch_action_target() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    SELECT batch.repository, batch.pull_request_number
      INTO NEW.repository, NEW.pull_request_number
      FROM repo_watch_dispatch_batch AS batch
     WHERE batch.dispatch_id = NEW.dispatch_id;
    RETURN NEW;
END;
$$;


--
-- Name: stamp_repo_watch_dispatch_batch_target(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION stamp_repo_watch_dispatch_batch_target() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    SELECT event.repository, event.pull_request_number
      INTO NEW.repository, NEW.pull_request_number
      FROM repo_watch_event AS event
     WHERE event.event_id = NEW.event_id;
    RETURN NEW;
END;
$$;


--
-- Name: stamp_repo_watch_webhook_projection_delivery(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION stamp_repo_watch_webhook_projection_delivery() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    SELECT delivery.repository, delivery.received_at
      INTO NEW.repository, NEW.received_at
      FROM repo_watch_webhook_delivery AS delivery
     WHERE delivery.hook_id = NEW.hook_id
       AND delivery.delivery_id = NEW.delivery_id;
    RETURN NEW;
END;
$$;


--
-- Tables.
--

--
-- Name: commissioned_dispatch; Type: TABLE; Schema: public
--

CREATE TABLE commissioned_dispatch (
    dispatch_id uuid NOT NULL,
    session_id uuid NOT NULL,
    create_command_id uuid NOT NULL,
    template_name text NOT NULL,
    template_content_digest bytea NOT NULL,
    initial_content_digest bytea NOT NULL,
    target_kind text NOT NULL,
    repository text NOT NULL,
    pull_request_number numeric(20,0),
    head_sha text,
    head_repository text,
    head_branch text,
    base_branch text,
    branch text,
    recorded_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT commissioned_dispatch_base_branch_check CHECK (((base_branch IS NULL) OR repo_watch_branch_is_valid(base_branch))),
    CONSTRAINT commissioned_dispatch_branch_check CHECK (((branch IS NULL) OR repo_watch_branch_is_valid(branch))),
    CONSTRAINT commissioned_dispatch_check CHECK ((((target_kind = 'pull_request'::text) AND (pull_request_number IS NOT NULL) AND (head_sha IS NOT NULL) AND (head_repository IS NOT NULL) AND (head_branch IS NOT NULL) AND (base_branch IS NOT NULL) AND (branch IS NULL)) OR ((target_kind = 'branch'::text) AND (pull_request_number IS NULL) AND (head_sha IS NULL) AND (head_repository IS NULL) AND (head_branch IS NULL) AND (base_branch IS NULL) AND (branch IS NOT NULL)))),
    CONSTRAINT commissioned_dispatch_head_branch_check CHECK (((head_branch IS NULL) OR repo_watch_branch_is_valid(head_branch))),
    CONSTRAINT commissioned_dispatch_head_repository_check CHECK (((head_repository IS NULL) OR repo_watch_repository_is_valid(head_repository))),
    CONSTRAINT commissioned_dispatch_head_sha_check CHECK (((head_sha IS NULL) OR ((head_sha COLLATE "C") ~ '^[0-9a-f]{40}$'::text))),
    CONSTRAINT commissioned_dispatch_initial_content_digest_check CHECK ((octet_length(initial_content_digest) = 32)),
    CONSTRAINT commissioned_dispatch_pull_request_number_check CHECK (((pull_request_number IS NULL) OR ((pull_request_number > (0)::numeric) AND (pull_request_number <= '18446744073709551615'::numeric)))),
    CONSTRAINT commissioned_dispatch_repository_check CHECK (repo_watch_repository_is_valid(repository)),
    CONSTRAINT commissioned_dispatch_target_kind_check CHECK ((target_kind = ANY (ARRAY['pull_request'::text, 'branch'::text]))),
    CONSTRAINT commissioned_dispatch_template_content_digest_check CHECK ((octet_length(template_content_digest) = 32)),
    CONSTRAINT commissioned_dispatch_template_name_check CHECK (((octet_length(template_name) >= 1) AND (octet_length(template_name) <= 128)))
);


--
-- Name: commissioned_dispatch_headless_approval_escalation; Type: TABLE; Schema: public
--

CREATE TABLE commissioned_dispatch_headless_approval_escalation (
    model_call_id uuid CONSTRAINT commissioned_dispatch_headless_approval__model_call_id_not_null NOT NULL,
    request_id uuid CONSTRAINT commissioned_dispatch_headless_approval_esc_request_id_not_null NOT NULL,
    dispatch_id uuid CONSTRAINT commissioned_dispatch_headless_approval_es_dispatch_id_not_null NOT NULL,
    session_id uuid CONSTRAINT commissioned_dispatch_headless_approval_esc_session_id_not_null NOT NULL,
    turn_id uuid CONSTRAINT commissioned_dispatch_headless_approval_escala_turn_id_not_null NOT NULL,
    terminal_attempt_id uuid CONSTRAINT commissioned_dispatch_headless_app_terminal_attempt_id_not_null NOT NULL,
    failure_entry_id uuid CONSTRAINT commissioned_dispatch_headless_approv_failure_entry_id_not_null NOT NULL,
    terminal_frontier_id uuid CONSTRAINT commissioned_dispatch_headless_ap_terminal_frontier_id_not_null NOT NULL,
    escalated_at timestamp with time zone DEFAULT transaction_timestamp() CONSTRAINT commissioned_dispatch_headless_approval_e_escalated_at_not_null NOT NULL
);


--
-- Name: commissioned_dispatch_headless_approval_escalation_audit; Type: VIEW; Schema: public
--

CREATE VIEW commissioned_dispatch_headless_approval_escalation_audit AS
 SELECT escalation.model_call_id,
    escalation.request_id,
    escalation.dispatch_id,
    escalation.session_id,
    escalation.turn_id,
    escalation.terminal_attempt_id,
    escalation.failure_entry_id,
    escalation.terminal_frontier_id,
    judge.rationale,
    escalation.escalated_at
   FROM (commissioned_dispatch_headless_approval_escalation escalation
     JOIN tool_approval_judge_model_call judge ON ((judge.model_call_id = escalation.model_call_id)));


--
-- Name: repo_watch_achieved_dispatch_settlement; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_achieved_dispatch_settlement (
    dispatch_id uuid NOT NULL,
    repository text NOT NULL,
    pull_request_number numeric(20,0),
    event_id uuid NOT NULL,
    released_at timestamp with time zone NOT NULL,
    CONSTRAINT repo_watch_achieved_dispatch_settleme_pull_request_number_check CHECK (((pull_request_number IS NULL) OR ((pull_request_number > (0)::numeric) AND (pull_request_number <= '18446744073709551615'::numeric)))),
    CONSTRAINT repo_watch_achieved_dispatch_settlement_repository_check CHECK (repo_watch_repository_is_valid(repository))
);


--
-- Name: repo_watch_complete_poll; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_complete_poll (
    repository text NOT NULL,
    completed_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT repo_watch_complete_poll_repository_check CHECK (repo_watch_repository_is_valid(repository))
);


--
-- Name: repo_watch_current_held_dispatch; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_current_held_dispatch (
    dispatch_id uuid NOT NULL,
    repository text NOT NULL,
    pull_request_number numeric(20,0),
    rule_id text NOT NULL,
    rule_version bigint NOT NULL,
    singleton_scope text NOT NULL,
    singleton_repository text,
    singleton_pull_request_number numeric(20,0),
    singleton_stack_root_pull_request_number numeric(20,0),
    held_since timestamp with time zone NOT NULL,
    CONSTRAINT repo_watch_current_held_dispatch_pull_request_number_check CHECK (((pull_request_number IS NULL) OR ((pull_request_number > (0)::numeric) AND (pull_request_number <= '18446744073709551615'::numeric)))),
    CONSTRAINT repo_watch_current_held_dispatch_repository_check CHECK (repo_watch_repository_is_valid(repository)),
    CONSTRAINT repo_watch_current_held_dispatch_rule_id_check CHECK (repo_watch_rule_id_is_valid(rule_id))
);


--
-- Name: repo_watch_current_pull_request; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_current_pull_request (
    repository text NOT NULL,
    pull_request_number numeric(20,0) NOT NULL,
    cursor_generation bigint NOT NULL,
    lifecycle text NOT NULL,
    head_repository text NOT NULL,
    base_branch text NOT NULL,
    head_branch text NOT NULL,
    state_payload jsonb NOT NULL,
    CONSTRAINT repo_watch_current_pull_request_base_branch_check CHECK (repo_watch_branch_is_valid(base_branch)),
    CONSTRAINT repo_watch_current_pull_request_cursor_generation_check CHECK ((cursor_generation > 0)),
    CONSTRAINT repo_watch_current_pull_request_head_branch_check CHECK (repo_watch_branch_is_valid(head_branch)),
    CONSTRAINT repo_watch_current_pull_request_head_repository_check CHECK (repo_watch_repository_is_valid(head_repository)),
    CONSTRAINT repo_watch_current_pull_request_lifecycle_check CHECK ((lifecycle = ANY (ARRAY['open'::text, 'closed'::text, 'merged'::text]))),
    CONSTRAINT repo_watch_current_pull_request_pull_request_number_check CHECK (((pull_request_number > (0)::numeric) AND (pull_request_number <= '18446744073709551615'::numeric))),
    CONSTRAINT repo_watch_current_pull_request_repository_check CHECK (repo_watch_repository_is_valid(repository)),
    CONSTRAINT repo_watch_current_pull_request_state_payload_check CHECK ((jsonb_typeof(state_payload) = 'object'::text))
);


--
-- Name: repo_watch_cursor; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_cursor (
    repository text NOT NULL,
    generation bigint NOT NULL,
    storage_version smallint NOT NULL,
    cursor_payload jsonb NOT NULL,
    recording_transaction_id xid8 NOT NULL,
    recorded_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT repo_watch_cursor_check CHECK ((((cursor_payload ->> 'storage_version'::text))::numeric = (storage_version)::numeric)),
    CONSTRAINT repo_watch_cursor_cursor_payload_check CHECK ((jsonb_typeof(cursor_payload) = 'object'::text)),
    CONSTRAINT repo_watch_cursor_cursor_payload_check1 CHECK ((cursor_payload ? 'storage_version'::text)),
    CONSTRAINT repo_watch_cursor_cursor_payload_check2 CHECK ((jsonb_typeof((cursor_payload -> 'storage_version'::text)) = 'number'::text)),
    CONSTRAINT repo_watch_cursor_cursor_payload_check3 CHECK (((cursor_payload ->> 'storage_version'::text) ~ '^[0-9]+$'::text)),
    CONSTRAINT repo_watch_cursor_generation_check CHECK ((generation > 0)),
    CONSTRAINT repo_watch_cursor_repository_check CHECK (repo_watch_repository_is_valid(repository)),
    CONSTRAINT repo_watch_cursor_storage_version_check CHECK ((storage_version = 4))
);


--
-- Name: repo_watch_current_pull_request_session_count; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_current_pull_request_session_count (
    repository text CONSTRAINT repo_watch_current_pull_request_session_cou_repository_not_null NOT NULL,
    pull_request_number numeric(20,0) CONSTRAINT repo_watch_current_pull_request_se_pull_request_number_not_null NOT NULL,
    session_count bigint CONSTRAINT repo_watch_current_pull_request_session__session_count_not_null NOT NULL,
    CONSTRAINT repo_watch_current_pull_request_sessi_pull_request_number_check CHECK (((pull_request_number > (0)::numeric) AND (pull_request_number <= '18446744073709551615'::numeric))),
    CONSTRAINT repo_watch_current_pull_request_session_cou_session_count_check CHECK ((session_count > 0)),
    CONSTRAINT repo_watch_current_pull_request_session_count_repository_check CHECK (repo_watch_repository_is_valid(repository))
);


--
-- Name: repo_watch_current_pull_request_work_count; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_current_pull_request_work_count (
    repository text NOT NULL,
    pull_request_number numeric(20,0) CONSTRAINT repo_watch_current_pull_request_wo_pull_request_number_not_null NOT NULL,
    held_count bigint DEFAULT 0 NOT NULL,
    obligation_count bigint DEFAULT 0 CONSTRAINT repo_watch_current_pull_request_work__obligation_count_not_null NOT NULL,
    CONSTRAINT repo_watch_current_pull_request_work__pull_request_number_check CHECK (((pull_request_number > (0)::numeric) AND (pull_request_number <= '18446744073709551615'::numeric))),
    CONSTRAINT repo_watch_current_pull_request_work_cou_obligation_count_check CHECK ((obligation_count >= 0)),
    CONSTRAINT repo_watch_current_pull_request_work_count_check CHECK (((held_count > 0) OR (obligation_count > 0))),
    CONSTRAINT repo_watch_current_pull_request_work_count_held_count_check CHECK ((held_count >= 0)),
    CONSTRAINT repo_watch_current_pull_request_work_count_repository_check CHECK (repo_watch_repository_is_valid(repository))
);


--
-- Name: repo_watch_current_repository_held_count; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_current_repository_held_count (
    repository text NOT NULL,
    held_count bigint NOT NULL,
    CONSTRAINT repo_watch_current_repository_held_count_held_count_check CHECK ((held_count > 0)),
    CONSTRAINT repo_watch_current_repository_held_count_repository_check CHECK (repo_watch_repository_is_valid(repository))
);


--
-- Name: repo_watch_current_repository_obligation_count; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_current_repository_obligation_count (
    repository text CONSTRAINT repo_watch_current_repository_obligation_co_repository_not_null NOT NULL,
    obligation_count bigint CONSTRAINT repo_watch_current_repository_obligat_obligation_count_not_null NOT NULL,
    CONSTRAINT repo_watch_current_repository_obligation_count_repository_check CHECK (repo_watch_repository_is_valid(repository)),
    CONSTRAINT repo_watch_current_repository_obligation_obligation_count_check CHECK ((obligation_count > 0))
);


--
-- Name: repo_watch_current_singleton_cooldown; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_current_singleton_cooldown (
    rule_id text NOT NULL,
    rule_version bigint NOT NULL,
    singleton_scope text NOT NULL,
    singleton_repository text,
    singleton_pull_request_number numeric(20,0),
    singleton_stack_root_pull_request_number numeric(20,0),
    eligible_at timestamp with time zone NOT NULL,
    CONSTRAINT repo_watch_current_singleton_cooldown_rule_id_check CHECK (repo_watch_rule_id_is_valid(rule_id)),
    CONSTRAINT repo_watch_current_singleton_cooldown_rule_version_check CHECK ((rule_version > 0))
);


--
-- Name: repo_watch_dispatch_action; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_dispatch_action (
    dispatch_id uuid NOT NULL,
    action_ordinal integer NOT NULL,
    event_id uuid NOT NULL,
    session_id uuid NOT NULL,
    create_command_id uuid NOT NULL,
    template_name text NOT NULL,
    template_content_digest bytea NOT NULL,
    recorded_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    repository text NOT NULL,
    pull_request_number numeric(20,0),
    CONSTRAINT repo_watch_dispatch_action_action_ordinal_check CHECK ((action_ordinal > 0)),
    CONSTRAINT repo_watch_dispatch_action_pull_request_number_check CHECK (((pull_request_number IS NULL) OR ((pull_request_number > (0)::numeric) AND (pull_request_number <= '18446744073709551615'::numeric)))),
    CONSTRAINT repo_watch_dispatch_action_repository_check CHECK (repo_watch_repository_is_valid(repository)),
    CONSTRAINT repo_watch_dispatch_action_template_content_digest_check CHECK ((octet_length(template_content_digest) = 32)),
    CONSTRAINT repo_watch_dispatch_action_template_name_check CHECK (((octet_length(template_name) >= 1) AND (octet_length(template_name) <= 128)))
);


--
-- Name: repo_watch_dispatch_batch; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_dispatch_batch (
    dispatch_id uuid NOT NULL,
    event_id uuid NOT NULL,
    rule_id text NOT NULL,
    rule_version bigint NOT NULL,
    singleton_scope text NOT NULL,
    singleton_repository text,
    singleton_pull_request_number numeric(20,0),
    singleton_stack_root_pull_request_number numeric(20,0),
    cooldown_seconds bigint NOT NULL,
    action_count integer NOT NULL,
    admitted_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    delivered_state_event_id uuid,
    repository text NOT NULL,
    pull_request_number numeric(20,0),
    CONSTRAINT repo_watch_dispatch_batch_action_count_check CHECK (((action_count >= 1) AND (action_count <= 32))),
    CONSTRAINT repo_watch_dispatch_batch_check CHECK ((((singleton_scope = 'pull_request'::text) AND (singleton_repository IS NOT NULL) AND (singleton_pull_request_number IS NOT NULL) AND (singleton_stack_root_pull_request_number IS NULL)) OR ((singleton_scope = 'stack'::text) AND (singleton_repository IS NOT NULL) AND (singleton_pull_request_number IS NULL) AND (singleton_stack_root_pull_request_number IS NOT NULL)) OR ((singleton_scope = 'rule'::text) AND (singleton_repository IS NULL) AND (singleton_pull_request_number IS NULL) AND (singleton_stack_root_pull_request_number IS NULL)) OR ((singleton_scope = 'repo'::text) AND (singleton_repository IS NOT NULL) AND (singleton_pull_request_number IS NULL) AND (singleton_stack_root_pull_request_number IS NULL)))),
    CONSTRAINT repo_watch_dispatch_batch_cooldown_seconds_check CHECK ((cooldown_seconds >= 0)),
    CONSTRAINT repo_watch_dispatch_batch_pull_request_number_check CHECK (((pull_request_number IS NULL) OR ((pull_request_number > (0)::numeric) AND (pull_request_number <= '18446744073709551615'::numeric)))),
    CONSTRAINT repo_watch_dispatch_batch_repository_check CHECK (repo_watch_repository_is_valid(repository)),
    CONSTRAINT repo_watch_dispatch_batch_rule_id_check CHECK (repo_watch_rule_id_is_valid(rule_id)),
    CONSTRAINT repo_watch_dispatch_batch_rule_version_check CHECK ((rule_version > 0)),
    CONSTRAINT repo_watch_dispatch_batch_singleton_pull_request_number_check CHECK (((singleton_pull_request_number IS NULL) OR ((singleton_pull_request_number > (0)::numeric) AND (singleton_pull_request_number <= '18446744073709551615'::numeric)))),
    CONSTRAINT repo_watch_dispatch_batch_singleton_repository_check CHECK (((singleton_repository IS NULL) OR repo_watch_repository_is_valid(singleton_repository))),
    CONSTRAINT repo_watch_dispatch_batch_singleton_scope_check CHECK ((singleton_scope = ANY (ARRAY['pull_request'::text, 'stack'::text, 'rule'::text, 'repo'::text]))),
    CONSTRAINT repo_watch_dispatch_batch_singleton_stack_root_pull_reque_check CHECK (((singleton_stack_root_pull_request_number IS NULL) OR ((singleton_stack_root_pull_request_number > (0)::numeric) AND (singleton_stack_root_pull_request_number <= '18446744073709551615'::numeric))))
);


--
-- Name: repo_watch_dispatch_delivery; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_dispatch_delivery (
    dispatch_id uuid NOT NULL,
    action_ordinal integer NOT NULL,
    submit_command_id uuid NOT NULL,
    accepted_input_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    delivered_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL
);


--
-- Name: repo_watch_dispatch_delivery_intent; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_dispatch_delivery_intent (
    dispatch_id uuid NOT NULL,
    action_ordinal integer NOT NULL,
    submit_command_id uuid NOT NULL,
    accepted_input_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    cancellation_entry_id uuid CONSTRAINT repo_watch_dispatch_delivery_int_cancellation_entry_id_not_null NOT NULL,
    cancellation_frontier_id uuid CONSTRAINT repo_watch_dispatch_delivery__cancellation_frontier_id_not_null NOT NULL,
    prepared_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL
);


--
-- Name: repo_watch_dispatch_obligation; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_dispatch_obligation (
    obligation_id uuid NOT NULL,
    repository text NOT NULL,
    rule_id text NOT NULL,
    rule_version bigint NOT NULL,
    singleton_scope text NOT NULL,
    singleton_repository text,
    singleton_pull_request_number numeric(20,0),
    singleton_stack_root_pull_request_number numeric(20,0),
    first_repository text NOT NULL,
    first_event_id uuid NOT NULL,
    latest_event_id uuid NOT NULL,
    matched_event_count bigint NOT NULL,
    blocking_dispatch_id uuid,
    owed_since timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    latest_match_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    settled_kind text,
    settled_dispatch_id uuid,
    settled_at timestamp with time zone,
    failed_attempts bigint DEFAULT 0 NOT NULL,
    last_failed_attempt_at timestamp with time zone,
    parked_at timestamp with time zone,
    counted_dispatch_id uuid,
    parked_state_event_id uuid,
    external_blocking_session_id uuid,
    CONSTRAINT repo_watch_dispatch_obligati_singleton_pull_request_numbe_check CHECK (((singleton_pull_request_number IS NULL) OR ((singleton_pull_request_number > (0)::numeric) AND (singleton_pull_request_number <= '18446744073709551615'::numeric)))),
    CONSTRAINT repo_watch_dispatch_obligati_singleton_stack_root_pull_re_check CHECK (((singleton_stack_root_pull_request_number IS NULL) OR ((singleton_stack_root_pull_request_number > (0)::numeric) AND (singleton_stack_root_pull_request_number <= '18446744073709551615'::numeric)))),
    CONSTRAINT repo_watch_dispatch_obligation_blocker_shape_check CHECK ((num_nonnulls(blocking_dispatch_id, external_blocking_session_id) = 1)),
    CONSTRAINT repo_watch_dispatch_obligation_check CHECK ((((singleton_scope = 'pull_request'::text) AND (singleton_repository IS NOT NULL) AND (singleton_pull_request_number IS NOT NULL) AND (singleton_stack_root_pull_request_number IS NULL)) OR ((singleton_scope = 'stack'::text) AND (singleton_repository IS NOT NULL) AND (singleton_pull_request_number IS NULL) AND (singleton_stack_root_pull_request_number IS NOT NULL)) OR ((singleton_scope = 'rule'::text) AND (singleton_repository IS NULL) AND (singleton_pull_request_number IS NULL) AND (singleton_stack_root_pull_request_number IS NULL)) OR ((singleton_scope = 'repo'::text) AND (singleton_repository IS NOT NULL) AND (singleton_pull_request_number IS NULL) AND (singleton_stack_root_pull_request_number IS NULL)))),
    CONSTRAINT repo_watch_dispatch_obligation_failed_attempt_time_check CHECK (((last_failed_attempt_at IS NULL) = (failed_attempts = 0))),
    CONSTRAINT repo_watch_dispatch_obligation_failed_attempts_check CHECK ((failed_attempts >= 0)),
    CONSTRAINT repo_watch_dispatch_obligation_first_repository_check CHECK (repo_watch_repository_is_valid(first_repository)),
    CONSTRAINT repo_watch_dispatch_obligation_matched_event_count_check CHECK ((matched_event_count > 0)),
    CONSTRAINT repo_watch_dispatch_obligation_parked_shape_check CHECK (((parked_at IS NULL) OR (failed_attempts > 0))),
    CONSTRAINT repo_watch_dispatch_obligation_parked_state_check CHECK (((parked_state_event_id IS NULL) = (parked_at IS NULL))),
    CONSTRAINT repo_watch_dispatch_obligation_repository_check CHECK (repo_watch_repository_is_valid(repository)),
    CONSTRAINT repo_watch_dispatch_obligation_rule_id_check CHECK (repo_watch_rule_id_is_valid(rule_id)),
    CONSTRAINT repo_watch_dispatch_obligation_rule_version_check CHECK ((rule_version > 0)),
    CONSTRAINT repo_watch_dispatch_obligation_settled_kind_check CHECK (((settled_kind IS NULL) OR (settled_kind = ANY (ARRAY['dispatched'::text, 'deactivated'::text, 'target_closed'::text])))),
    CONSTRAINT repo_watch_dispatch_obligation_settlement_shape_check CHECK ((((settled_kind IS NULL) AND (settled_dispatch_id IS NULL) AND (settled_at IS NULL)) OR ((settled_kind = 'dispatched'::text) AND (settled_dispatch_id IS NOT NULL) AND (settled_at IS NOT NULL)) OR ((settled_kind = ANY (ARRAY['deactivated'::text, 'target_closed'::text])) AND (settled_dispatch_id IS NULL) AND (settled_at IS NOT NULL)))),
    CONSTRAINT repo_watch_dispatch_obligation_singleton_repository_check CHECK (((singleton_repository IS NULL) OR repo_watch_repository_is_valid(singleton_repository))),
    CONSTRAINT repo_watch_dispatch_obligation_singleton_scope_check CHECK ((singleton_scope = ANY (ARRAY['pull_request'::text, 'stack'::text, 'rule'::text, 'repo'::text])))
);


--
-- Name: repo_watch_dispatch_obligation_park; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_dispatch_obligation_park (
    obligation_id uuid NOT NULL,
    transition_ordinal integer NOT NULL,
    transition_kind text NOT NULL,
    failed_attempts bigint NOT NULL,
    release_reason text,
    release_event_id uuid,
    release_actor text,
    recorded_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT repo_watch_dispatch_obligation_park_check CHECK (((transition_kind = 'released'::text) = (release_reason IS NOT NULL))),
    CONSTRAINT repo_watch_dispatch_obligation_park_check1 CHECK (((release_event_id IS NOT NULL) = (NOT (release_reason IS DISTINCT FROM 'pull_request_progress'::text)))),
    CONSTRAINT repo_watch_dispatch_obligation_park_check2 CHECK (((release_actor IS NOT NULL) = (NOT (release_reason IS DISTINCT FROM 'operator'::text)))),
    CONSTRAINT repo_watch_dispatch_obligation_park_failed_attempts_check CHECK ((failed_attempts >= 0)),
    CONSTRAINT repo_watch_dispatch_obligation_park_release_actor_check CHECK (((release_actor IS NULL) OR ((length(release_actor) >= 1) AND (length(release_actor) <= 200)))),
    CONSTRAINT repo_watch_dispatch_obligation_park_release_reason_check CHECK (((release_reason IS NULL) OR (release_reason = ANY (ARRAY['operator'::text, 'pull_request_progress'::text])))),
    CONSTRAINT repo_watch_dispatch_obligation_park_transition_kind_check CHECK ((transition_kind = ANY (ARRAY['parked'::text, 'released'::text]))),
    CONSTRAINT repo_watch_dispatch_obligation_park_transition_ordinal_check CHECK ((transition_ordinal > 0))
);


--
-- Name: repo_watch_dispatch_release; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_dispatch_release (
    dispatch_id uuid NOT NULL,
    released_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL
);


--
-- Name: repo_watch_dispatch_start_lease; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_dispatch_start_lease (
    dispatch_id uuid NOT NULL,
    action_ordinal integer NOT NULL,
    session_id uuid NOT NULL,
    leased_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    CONSTRAINT repo_watch_dispatch_start_lease_check CHECK ((expires_at > leased_at)),
    CONSTRAINT repo_watch_dispatch_start_lease_check1 CHECK ((expires_at <= (leased_at + '00:05:00'::interval)))
);


--
-- Name: repo_watch_dispatch_start_lease_expiration; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_dispatch_start_lease_expiration (
    dispatch_id uuid NOT NULL,
    action_ordinal integer CONSTRAINT repo_watch_dispatch_start_lease_expirat_action_ordinal_not_null NOT NULL,
    session_id uuid NOT NULL,
    goal_command_id uuid,
    expired_at timestamp with time zone DEFAULT clock_timestamp() NOT NULL
);


--
-- Name: repo_watch_dispatch_start_lease_quarantine; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_dispatch_start_lease_quarantine (
    dispatch_id uuid NOT NULL,
    action_ordinal integer CONSTRAINT repo_watch_dispatch_start_lease_quarant_action_ordinal_not_null NOT NULL,
    session_id uuid NOT NULL,
    reason text NOT NULL,
    quarantined_at timestamp with time zone DEFAULT clock_timestamp() CONSTRAINT repo_watch_dispatch_start_lease_quarant_quarantined_at_not_null NOT NULL,
    CONSTRAINT repo_watch_dispatch_start_lease_quarantine_reason_check CHECK (((octet_length(reason) >= 1) AND (octet_length(reason) <= 256)))
);


--
-- Name: repo_watch_event; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_event (
    event_id uuid NOT NULL,
    repository text NOT NULL,
    cursor_generation bigint NOT NULL,
    event_ordinal integer NOT NULL,
    event_version smallint NOT NULL,
    target_kind text NOT NULL,
    event_kind text NOT NULL,
    pull_request_number numeric(20,0),
    head_sha text,
    head_repository text,
    base_branch text,
    head_branch text,
    title text,
    body text,
    labels text[],
    draft boolean,
    author text,
    previous_sha text,
    current_sha text,
    mergeable_state text,
    checks_outcome text,
    check_run_name text,
    conclusion text,
    workflow_branch text,
    workflow_name text,
    review_reviewer text,
    review_state text,
    review_commit text,
    thread_id text,
    label_name text,
    advanced_branch text,
    reaction_subject_kind text,
    reaction_subject_id numeric(20,0),
    reaction_reactor text,
    reaction_content text,
    reaction_change text,
    recorded_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    content_identity_version smallint NOT NULL,
    content_identity bytea NOT NULL,
    producer text NOT NULL,
    CONSTRAINT repo_watch_event_author_check CHECK (((author IS NULL) OR repo_watch_login_is_valid(author))),
    CONSTRAINT repo_watch_event_base_branch_check CHECK (((base_branch IS NULL) OR repo_watch_branch_is_valid(base_branch))),
    CONSTRAINT repo_watch_event_body_check CHECK (((body IS NULL) OR (octet_length(body) <= 262144))),
    CONSTRAINT repo_watch_event_check CHECK ((((target_kind = 'pull_request'::text) AND (event_kind <> 'branch_workflow_run_completed'::text) AND (pull_request_number IS NOT NULL) AND (pull_request_number > (0)::numeric) AND (pull_request_number <= '18446744073709551615'::numeric) AND (head_sha IS NOT NULL) AND (head_repository IS NOT NULL) AND (base_branch IS NOT NULL) AND (head_branch IS NOT NULL) AND (title IS NOT NULL) AND (body IS NOT NULL) AND (labels IS NOT NULL) AND (draft IS NOT NULL)) OR ((target_kind = 'branch'::text) AND (event_kind = 'branch_workflow_run_completed'::text) AND (pull_request_number IS NULL) AND (head_sha IS NULL) AND (head_repository IS NULL) AND (base_branch IS NULL) AND (head_branch IS NULL) AND (title IS NULL) AND (body IS NULL) AND (labels IS NULL) AND (draft IS NULL) AND (author IS NULL)))),
    CONSTRAINT repo_watch_event_check1 CHECK ((((reaction_subject_kind = 'pull_request_body'::text) AND (reaction_subject_id IS NULL)) OR ((reaction_subject_kind = ANY (ARRAY['issue_comment'::text, 'review_comment'::text])) AND (reaction_subject_id IS NOT NULL) AND (reaction_subject_id > (0)::numeric) AND (reaction_subject_id <= '18446744073709551615'::numeric)) OR ((reaction_subject_kind IS NULL) AND (reaction_subject_id IS NULL)))),
    CONSTRAINT repo_watch_event_check10 CHECK (((review_reviewer IS NOT NULL) = (event_kind = 'review_submitted'::text))),
    CONSTRAINT repo_watch_event_check11 CHECK (((review_state IS NOT NULL) = (event_kind = 'review_submitted'::text))),
    CONSTRAINT repo_watch_event_check12 CHECK (((review_commit IS NOT NULL) = (event_kind = 'review_submitted'::text))),
    CONSTRAINT repo_watch_event_check13 CHECK (((thread_id IS NOT NULL) = (event_kind = ANY (ARRAY['thread_opened'::text, 'thread_resolved'::text])))),
    CONSTRAINT repo_watch_event_check14 CHECK (((label_name IS NOT NULL) = (event_kind = ANY (ARRAY['labeled'::text, 'unlabeled'::text])))),
    CONSTRAINT repo_watch_event_check15 CHECK (((advanced_branch IS NOT NULL) = (event_kind = 'base_advanced'::text))),
    CONSTRAINT repo_watch_event_check16 CHECK (((reaction_subject_kind IS NOT NULL) = (event_kind = 'reaction_changed'::text))),
    CONSTRAINT repo_watch_event_check17 CHECK (((reaction_reactor IS NOT NULL) = (event_kind = 'reaction_changed'::text))),
    CONSTRAINT repo_watch_event_check18 CHECK (((reaction_content IS NOT NULL) = (event_kind = 'reaction_changed'::text))),
    CONSTRAINT repo_watch_event_check19 CHECK (((reaction_change IS NOT NULL) = (event_kind = 'reaction_changed'::text))),
    CONSTRAINT repo_watch_event_check2 CHECK (((previous_sha IS NOT NULL) = (event_kind = 'head_changed'::text))),
    CONSTRAINT repo_watch_event_check20 CHECK (((previous_sha IS NULL) OR (previous_sha <> current_sha))),
    CONSTRAINT repo_watch_event_check21 CHECK (((current_sha IS NULL) OR (current_sha = head_sha))),
    CONSTRAINT repo_watch_event_check22 CHECK (((advanced_branch IS NULL) OR (advanced_branch = base_branch))),
    CONSTRAINT repo_watch_event_check23 CHECK (((event_kind <> 'labeled'::text) OR (label_name = ANY (labels)))),
    CONSTRAINT repo_watch_event_check24 CHECK (((event_kind <> 'unlabeled'::text) OR (NOT (label_name = ANY (labels))))),
    CONSTRAINT repo_watch_event_check3 CHECK (((current_sha IS NOT NULL) = (event_kind = 'head_changed'::text))),
    CONSTRAINT repo_watch_event_check4 CHECK (((mergeable_state IS NOT NULL) = (event_kind = 'mergeable_state_changed'::text))),
    CONSTRAINT repo_watch_event_check5 CHECK (((checks_outcome IS NOT NULL) = (event_kind = 'checks_completed'::text))),
    CONSTRAINT repo_watch_event_check6 CHECK (((check_run_name IS NOT NULL) = (event_kind = 'check_run_completed'::text))),
    CONSTRAINT repo_watch_event_check7 CHECK (((conclusion IS NOT NULL) = (event_kind = ANY (ARRAY['check_run_completed'::text, 'branch_workflow_run_completed'::text])))),
    CONSTRAINT repo_watch_event_check8 CHECK (((workflow_branch IS NOT NULL) = (event_kind = 'branch_workflow_run_completed'::text))),
    CONSTRAINT repo_watch_event_check9 CHECK (((workflow_name IS NOT NULL) = (event_kind = 'branch_workflow_run_completed'::text))),
    CONSTRAINT repo_watch_event_check_run_name_check CHECK (((check_run_name IS NULL) OR ((octet_length(check_run_name) >= 1) AND (octet_length(check_run_name) <= 256)))),
    CONSTRAINT repo_watch_event_checks_outcome_check CHECK (((checks_outcome IS NULL) OR (checks_outcome = ANY (ARRAY['success'::text, 'failure'::text])))),
    CONSTRAINT repo_watch_event_conclusion_check CHECK (((conclusion IS NULL) OR (conclusion = ANY (ARRAY['success'::text, 'failure'::text, 'neutral'::text, 'cancelled'::text, 'skipped'::text, 'timed_out'::text, 'action_required'::text, 'stale'::text, 'startup_failure'::text])))),
    CONSTRAINT repo_watch_event_content_identity_length_check CHECK ((octet_length(content_identity) = 32)),
    CONSTRAINT repo_watch_event_content_identity_version_check CHECK ((content_identity_version = 1)),
    CONSTRAINT repo_watch_event_current_sha_check CHECK (((current_sha IS NULL) OR ((current_sha COLLATE "C") ~ '^[0-9a-f]{40}$'::text))),
    CONSTRAINT repo_watch_event_event_kind_check CHECK ((event_kind = ANY (ARRAY['pull_request_opened'::text, 'pull_request_closed'::text, 'pull_request_merged'::text, 'head_changed'::text, 'mergeable_state_changed'::text, 'checks_completed'::text, 'check_run_completed'::text, 'branch_workflow_run_completed'::text, 'review_submitted'::text, 'thread_opened'::text, 'thread_resolved'::text, 'labeled'::text, 'unlabeled'::text, 'base_advanced'::text, 'reaction_changed'::text]))),
    CONSTRAINT repo_watch_event_event_ordinal_check CHECK ((event_ordinal > 0)),
    CONSTRAINT repo_watch_event_event_version_check CHECK ((event_version = 1)),
    CONSTRAINT repo_watch_event_head_branch_check CHECK (((head_branch IS NULL) OR repo_watch_branch_is_valid(head_branch))),
    CONSTRAINT repo_watch_event_head_repository_check CHECK (((head_repository IS NULL) OR repo_watch_repository_is_valid(head_repository))),
    CONSTRAINT repo_watch_event_head_sha_check CHECK (((head_sha IS NULL) OR ((head_sha COLLATE "C") ~ '^[0-9a-f]{40}$'::text))),
    CONSTRAINT repo_watch_event_label_name_check CHECK (((label_name IS NULL) OR ((octet_length(label_name) BETWEEN 1 AND 200) AND (char_length(label_name) <= 50)))),
    CONSTRAINT repo_watch_event_labels_check CHECK (((labels IS NULL) OR repo_watch_labels_are_valid(labels))),
    CONSTRAINT repo_watch_event_mergeable_state_check CHECK (((mergeable_state IS NULL) OR (mergeable_state = ANY (ARRAY['mergeable'::text, 'conflicting'::text, 'unknown'::text])))),
    CONSTRAINT repo_watch_event_previous_sha_check CHECK (((previous_sha IS NULL) OR ((previous_sha COLLATE "C") ~ '^[0-9a-f]{40}$'::text))),
    CONSTRAINT repo_watch_event_producer_check CHECK ((producer = ANY (ARRAY['poll'::text, 'webhook'::text]))),
    CONSTRAINT repo_watch_event_reaction_change_check CHECK (((reaction_change IS NULL) OR (reaction_change = ANY (ARRAY['added'::text, 'removed'::text])))),
    CONSTRAINT repo_watch_event_reaction_content_check CHECK (((reaction_content IS NULL) OR ((octet_length(reaction_content) >= 1) AND (octet_length(reaction_content) <= 64)))),
    CONSTRAINT repo_watch_event_reaction_reactor_check CHECK (((reaction_reactor IS NULL) OR repo_watch_login_is_valid(reaction_reactor))),
    CONSTRAINT repo_watch_event_reaction_subject_kind_check CHECK (((reaction_subject_kind IS NULL) OR (reaction_subject_kind = ANY (ARRAY['pull_request_body'::text, 'issue_comment'::text, 'review_comment'::text])))),
    CONSTRAINT repo_watch_event_repository_check CHECK (repo_watch_repository_is_valid(repository)),
    CONSTRAINT repo_watch_event_review_commit_check CHECK (((review_commit IS NULL) OR ((review_commit COLLATE "C") ~ '^[0-9a-f]{40}$'::text))),
    CONSTRAINT repo_watch_event_review_reviewer_check CHECK (((review_reviewer IS NULL) OR repo_watch_login_is_valid(review_reviewer))),
    CONSTRAINT repo_watch_event_review_state_check CHECK (((review_state IS NULL) OR (review_state = ANY (ARRAY['approved'::text, 'changes_requested'::text, 'commented'::text])))),
    CONSTRAINT repo_watch_event_target_kind_check CHECK ((target_kind = ANY (ARRAY['pull_request'::text, 'branch'::text]))),
    CONSTRAINT repo_watch_event_thread_id_check CHECK (((thread_id IS NULL) OR ((octet_length(thread_id) >= 1) AND (octet_length(thread_id) <= 256)))),
    CONSTRAINT repo_watch_event_title_check CHECK (((title IS NULL) OR ((octet_length(title) >= 1) AND (octet_length(title) <= 1024)))),
    CONSTRAINT repo_watch_event_workflow_branch_check CHECK (((workflow_branch IS NULL) OR repo_watch_branch_is_valid(workflow_branch))),
    CONSTRAINT repo_watch_event_workflow_name_check CHECK (((workflow_name IS NULL) OR ((octet_length(workflow_name) >= 1) AND (octet_length(workflow_name) <= 256))))
);


--
-- Name: repo_watch_headless_approval_escalation; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_headless_approval_escalation (
    model_call_id uuid NOT NULL,
    request_id uuid NOT NULL,
    dispatch_id uuid NOT NULL,
    action_ordinal integer NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    terminal_attempt_id uuid CONSTRAINT repo_watch_headless_approval_escal_terminal_attempt_id_not_null NOT NULL,
    failure_entry_id uuid CONSTRAINT repo_watch_headless_approval_escalati_failure_entry_id_not_null NOT NULL,
    terminal_frontier_id uuid CONSTRAINT repo_watch_headless_approval_esca_terminal_frontier_id_not_null NOT NULL,
    escalated_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL
);


--
-- Name: repo_watch_headless_approval_escalation_audit; Type: VIEW; Schema: public
--

CREATE VIEW repo_watch_headless_approval_escalation_audit AS
 SELECT escalation.model_call_id,
    escalation.request_id,
    escalation.dispatch_id,
    escalation.action_ordinal,
    escalation.session_id,
    escalation.turn_id,
    escalation.terminal_attempt_id,
    escalation.failure_entry_id,
    escalation.terminal_frontier_id,
    judge.rationale,
    escalation.escalated_at,
    released.released_at,
    owed.obligation_id,
    owed.settled_kind AS obligation_settled_kind,
    owed.settled_at AS obligation_settled_at
   FROM (((repo_watch_headless_approval_escalation escalation
     JOIN tool_approval_judge_model_call judge ON ((judge.model_call_id = escalation.model_call_id)))
     LEFT JOIN repo_watch_dispatch_release released ON ((released.dispatch_id = escalation.dispatch_id)))
     LEFT JOIN LATERAL ( SELECT obligation.obligation_id,
            obligation.settled_kind,
            obligation.settled_at
           FROM repo_watch_dispatch_obligation obligation
          WHERE (obligation.blocking_dispatch_id = escalation.dispatch_id)
          ORDER BY obligation.owed_since DESC, obligation.obligation_id DESC
         LIMIT 1) owed ON (true));


--
-- Name: repo_watch_held_dispatch_slot; Type: VIEW; Schema: public
--

CREATE VIEW repo_watch_held_dispatch_slot AS
 SELECT batch.dispatch_id,
    origin.repository,
    origin.pull_request_number,
    batch.rule_id,
    batch.rule_version,
    batch.singleton_scope,
    batch.singleton_repository,
    batch.singleton_pull_request_number,
    batch.singleton_stack_root_pull_request_number,
    batch.admitted_at AS held_since,
    ARRAY( SELECT action.session_id
           FROM repo_watch_dispatch_action action
          WHERE (action.dispatch_id = batch.dispatch_id)
          ORDER BY action.action_ordinal) AS session_ids,
    (batch.action_count = ( SELECT count(*) AS count
           FROM (repo_watch_dispatch_action action
             JOIN repo_watch_dispatch_delivery delivery ON (((delivery.dispatch_id = action.dispatch_id) AND (delivery.action_ordinal = action.action_ordinal))))
          WHERE (action.dispatch_id = batch.dispatch_id))) AS every_action_delivered,
    (batch.action_count = ( SELECT count(*) AS count
           FROM ((repo_watch_dispatch_action action
             JOIN repo_watch_dispatch_delivery delivery ON (((delivery.dispatch_id = action.dispatch_id) AND (delivery.action_ordinal = action.action_ordinal))))
             JOIN turn_lifecycle turn ON (((turn.turn_id = delivery.turn_id) AND ((turn.state_kind = 'terminal'::text) OR (NOT goal_turn_is_runtime_relevant(turn.session_id, turn.turn_id))))))
          WHERE (action.dispatch_id = batch.dispatch_id))) AS every_delivery_turn_releasable,
    (NOT (EXISTS ( SELECT 1
           FROM (repo_watch_dispatch_action action
             JOIN turn_lifecycle live_turn ON ((live_turn.session_id = action.session_id)))
          WHERE ((action.dispatch_id = batch.dispatch_id) AND (live_turn.state_kind <> 'terminal'::text) AND goal_turn_is_runtime_relevant(live_turn.session_id, live_turn.turn_id))))) AS no_live_runtime_turn,
    (NOT (EXISTS ( SELECT 1
           FROM (repo_watch_dispatch_action action
             JOIN goal_event current_goal ON ((current_goal.session_id = action.session_id)))
          WHERE ((action.dispatch_id = batch.dispatch_id) AND (current_goal.event_ordinal = ( SELECT max(candidate.event_ordinal) AS max
                   FROM goal_event candidate
                  WHERE (candidate.session_id = action.session_id))) AND (current_goal.event_kind = ANY (ARRAY['commissioned'::text, 'resumed'::text, 'superseded'::text])))))) AS every_goal_nonpursuing,
    array_remove(ARRAY[
        CASE
            WHEN (batch.action_count <> ( SELECT count(*) AS count
               FROM (repo_watch_dispatch_action action
                 JOIN repo_watch_dispatch_delivery delivery ON (((delivery.dispatch_id = action.dispatch_id) AND (delivery.action_ordinal = action.action_ordinal))))
              WHERE (action.dispatch_id = batch.dispatch_id))) THEN 'undelivered_action'::text
            ELSE NULL::text
        END,
        CASE
            WHEN (batch.action_count <> ( SELECT count(*) AS count
               FROM ((repo_watch_dispatch_action action
                 JOIN repo_watch_dispatch_delivery delivery ON (((delivery.dispatch_id = action.dispatch_id) AND (delivery.action_ordinal = action.action_ordinal))))
                 JOIN turn_lifecycle turn ON (((turn.turn_id = delivery.turn_id) AND ((turn.state_kind = 'terminal'::text) OR (NOT goal_turn_is_runtime_relevant(turn.session_id, turn.turn_id))))))
              WHERE (action.dispatch_id = batch.dispatch_id))) THEN 'delivery_turn_runtime_relevant'::text
            ELSE NULL::text
        END,
        CASE
            WHEN (EXISTS ( SELECT 1
               FROM (repo_watch_dispatch_action action
                 JOIN turn_lifecycle live_turn ON ((live_turn.session_id = action.session_id)))
              WHERE ((action.dispatch_id = batch.dispatch_id) AND (live_turn.state_kind <> 'terminal'::text) AND goal_turn_is_runtime_relevant(live_turn.session_id, live_turn.turn_id)))) THEN 'live_runtime_turn'::text
            ELSE NULL::text
        END,
        CASE
            WHEN (EXISTS ( SELECT 1
               FROM (repo_watch_dispatch_action action
                 JOIN goal_event current_goal ON ((current_goal.session_id = action.session_id)))
              WHERE ((action.dispatch_id = batch.dispatch_id) AND (current_goal.event_ordinal = ( SELECT max(candidate.event_ordinal) AS max
                       FROM goal_event candidate
                      WHERE (candidate.session_id = action.session_id))) AND (current_goal.event_kind = ANY (ARRAY['commissioned'::text, 'resumed'::text, 'superseded'::text]))))) THEN 'pursuing_goal'::text
            ELSE NULL::text
        END], NULL::text) AS blockers,
    origin.workflow_branch
   FROM (repo_watch_dispatch_batch batch
     JOIN repo_watch_event origin ON ((origin.event_id = batch.event_id)))
  WHERE (NOT (EXISTS ( SELECT 1
           FROM repo_watch_dispatch_release released
          WHERE (released.dispatch_id = batch.dispatch_id))));


--
-- Name: repo_watch_lifecycle_cutoff; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_lifecycle_cutoff (
    event_id uuid NOT NULL,
    disposition_kind text NOT NULL,
    processed_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT repo_watch_lifecycle_cutoff_disposition_kind_check CHECK ((disposition_kind = ANY (ARRAY['terminal'::text, 'reopened'::text])))
);


--
-- Name: repo_watch_lifecycle_cutoff_goal; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_lifecycle_cutoff_goal (
    event_id uuid NOT NULL,
    session_id uuid NOT NULL,
    goal_command_id uuid NOT NULL
);


--
-- Name: repo_watch_outstanding_dispatch_obligation; Type: VIEW; Schema: public
--

CREATE VIEW repo_watch_outstanding_dispatch_obligation AS
 SELECT obligation.obligation_id,
    obligation.repository,
    obligation.rule_id,
    obligation.rule_version,
    obligation.singleton_scope,
    obligation.singleton_repository,
    obligation.singleton_pull_request_number,
    obligation.singleton_stack_root_pull_request_number,
    obligation.first_repository,
    obligation.first_event_id,
    obligation.latest_event_id,
    obligation.matched_event_count,
    obligation.owed_since,
    obligation.latest_match_at,
    occupying.dispatch_id AS occupying_dispatch_id,
    COALESCE(occupying.session_ids,
        CASE
            WHEN (external_blocker.session_id IS NULL) THEN NULL::uuid[]
            ELSE ARRAY[external_blocker.session_id]
        END) AS occupying_session_ids,
    cooldown.eligible_at,
    ((occupying.dispatch_id IS NULL) AND (external_blocker.session_id IS NULL) AND ((cooldown.eligible_at IS NULL) OR (cooldown.eligible_at <= clock_timestamp())) AND (obligation.parked_at IS NULL) AND (obligation.failed_attempts < repo_watch_dispatch_attempt_budget())) AS ready,
    obligation.failed_attempts,
    obligation.last_failed_attempt_at,
    obligation.parked_at,
    obligation.external_blocking_session_id
   FROM (((repo_watch_dispatch_obligation obligation
     LEFT JOIN LATERAL ( SELECT held.dispatch_id,
            array_agg(action.session_id ORDER BY action.action_ordinal) AS session_ids
           FROM (repo_watch_current_held_dispatch held
             JOIN repo_watch_dispatch_action action USING (dispatch_id))
          WHERE ((held.rule_id = obligation.rule_id) AND (held.rule_version = obligation.rule_version) AND (held.singleton_scope = obligation.singleton_scope) AND (NOT (held.singleton_repository IS DISTINCT FROM obligation.singleton_repository)) AND (NOT (held.singleton_pull_request_number IS DISTINCT FROM obligation.singleton_pull_request_number)) AND (NOT (held.singleton_stack_root_pull_request_number IS DISTINCT FROM obligation.singleton_stack_root_pull_request_number)))
          GROUP BY held.dispatch_id, held.held_since
          ORDER BY held.held_since
         LIMIT 1) occupying ON (true))
     LEFT JOIN LATERAL ( SELECT obligation.external_blocking_session_id AS session_id
          WHERE ( SELECT (event.event_kind = ANY (ARRAY['commissioned'::text, 'resumed'::text, 'superseded'::text]))
                   FROM goal_event event
                  WHERE (event.session_id = obligation.external_blocking_session_id)
                  ORDER BY event.event_ordinal DESC
                 LIMIT 1)) external_blocker ON (true))
     LEFT JOIN repo_watch_current_singleton_cooldown cooldown ON (((cooldown.rule_id = obligation.rule_id) AND (cooldown.rule_version = obligation.rule_version) AND (cooldown.singleton_scope = obligation.singleton_scope) AND (NOT (cooldown.singleton_repository IS DISTINCT FROM obligation.singleton_repository)) AND (NOT (cooldown.singleton_pull_request_number IS DISTINCT FROM obligation.singleton_pull_request_number)) AND (NOT (cooldown.singleton_stack_root_pull_request_number IS DISTINCT FROM obligation.singleton_stack_root_pull_request_number)))))
  WHERE (obligation.settled_kind IS NULL);


--
-- Name: repo_watch_parked_dispatch_obligation; Type: VIEW; Schema: public
--

CREATE VIEW repo_watch_parked_dispatch_obligation AS
 SELECT obligation.obligation_id,
    obligation.repository,
    parked_state.repository AS stalled_repository,
    obligation.rule_id,
    obligation.rule_version,
    obligation.singleton_scope,
    obligation.singleton_repository,
    obligation.singleton_pull_request_number,
    obligation.singleton_stack_root_pull_request_number,
    obligation.latest_event_id,
    obligation.matched_event_count,
    obligation.owed_since,
    obligation.latest_match_at,
    obligation.failed_attempts,
    obligation.last_failed_attempt_at,
    obligation.parked_at,
    obligation.parked_state_event_id,
    parked_state.pull_request_number,
    parked_state.head_sha,
    ( SELECT max(park.recorded_at) AS max
           FROM repo_watch_dispatch_obligation_park park
          WHERE ((park.obligation_id = obligation.obligation_id) AND (park.transition_kind = 'parked'::text))) AS latest_park_recorded_at
   FROM (repo_watch_dispatch_obligation obligation
     JOIN repo_watch_event parked_state ON ((parked_state.event_id = obligation.parked_state_event_id)))
  WHERE ((obligation.settled_kind IS NULL) AND (obligation.parked_at IS NOT NULL));


--
-- Name: repo_watch_repository_key; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_repository_key (
    repository text NOT NULL,
    CONSTRAINT repo_watch_repository_key_repository_check CHECK (repo_watch_repository_is_valid(repository))
);


--
-- Name: repo_watch_rule_activation; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_rule_activation (
    repository text NOT NULL,
    rule_id text NOT NULL,
    rule_version bigint NOT NULL,
    rule_digest bytea NOT NULL,
    after_cursor_generation bigint,
    after_event_ordinal integer,
    activated_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT repo_watch_rule_activation_check CHECK ((((after_cursor_generation IS NULL) AND (after_event_ordinal IS NULL)) OR ((after_cursor_generation IS NOT NULL) AND (after_cursor_generation > 0) AND (after_event_ordinal IS NOT NULL) AND (after_event_ordinal > 0)))),
    CONSTRAINT repo_watch_rule_activation_repository_check CHECK (repo_watch_repository_is_valid(repository)),
    CONSTRAINT repo_watch_rule_activation_rule_digest_check CHECK ((octet_length(rule_digest) = 32)),
    CONSTRAINT repo_watch_rule_activation_rule_id_check CHECK (repo_watch_rule_id_is_valid(rule_id)),
    CONSTRAINT repo_watch_rule_activation_rule_version_check CHECK ((rule_version > 0))
);


--
-- Name: repo_watch_rule_deactivation; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_rule_deactivation (
    repository text NOT NULL,
    rule_id text NOT NULL,
    rule_version bigint NOT NULL,
    deactivated_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL
);


--
-- Name: repo_watch_rule_evaluation; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_rule_evaluation (
    repository text NOT NULL,
    rule_id text NOT NULL,
    rule_version bigint NOT NULL,
    event_id uuid NOT NULL,
    cursor_generation bigint NOT NULL,
    event_ordinal integer NOT NULL,
    outcome_kind text NOT NULL,
    dispatch_id uuid,
    evaluated_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    pull_request_number numeric(20,0),
    CONSTRAINT repo_watch_rule_evaluation_check CHECK (((dispatch_id IS NOT NULL) = (outcome_kind = 'dispatched'::text))),
    CONSTRAINT repo_watch_rule_evaluation_cursor_generation_check CHECK ((cursor_generation > 0)),
    CONSTRAINT repo_watch_rule_evaluation_event_ordinal_check CHECK ((event_ordinal > 0)),
    CONSTRAINT repo_watch_rule_evaluation_outcome_kind_check CHECK ((outcome_kind = ANY (ARRAY['not_matched'::text, 'target_closed'::text, 'occupied'::text, 'coalesced'::text, 'cooldown'::text, 'dispatched'::text]))),
    CONSTRAINT repo_watch_rule_evaluation_pull_request_number_check CHECK (((pull_request_number IS NULL) OR ((pull_request_number > (0)::numeric) AND (pull_request_number <= '18446744073709551615'::numeric)))),
    CONSTRAINT repo_watch_rule_evaluation_repository_check CHECK (repo_watch_repository_is_valid(repository)),
    CONSTRAINT repo_watch_rule_evaluation_rule_id_check CHECK (repo_watch_rule_id_is_valid(rule_id)),
    CONSTRAINT repo_watch_rule_evaluation_rule_version_check CHECK ((rule_version > 0))
);


--
-- Name: repo_watch_rule_field_fingerprint; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_rule_field_fingerprint (
    repository text NOT NULL,
    rule_id text NOT NULL,
    rule_version bigint NOT NULL,
    rule_field_digests bytea NOT NULL,
    recorded_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT repo_watch_rule_field_fingerprint_repository_check CHECK (repo_watch_repository_is_valid(repository)),
    CONSTRAINT repo_watch_rule_field_fingerprint_rule_field_digests_check CHECK ((octet_length(rule_field_digests) = 512)),
    CONSTRAINT repo_watch_rule_field_fingerprint_rule_id_check CHECK (repo_watch_rule_id_is_valid(rule_id)),
    CONSTRAINT repo_watch_rule_field_fingerprint_rule_version_check CHECK ((rule_version > 0))
);


--
-- Name: repo_watch_webhook_delivery; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_webhook_delivery (
    hook_id numeric(20,0) NOT NULL,
    delivery_id uuid NOT NULL,
    repository text NOT NULL,
    event_name text NOT NULL,
    action_name text,
    body_digest bytea NOT NULL,
    receipt_sequence bigint NOT NULL,
    received_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT repo_watch_webhook_delivery_action_name_check CHECK (((action_name IS NULL) OR ((octet_length(action_name) BETWEEN 1 AND 64) AND ((action_name COLLATE "C") ~ '^[a-z0-9_]+$'::text)))),
    CONSTRAINT repo_watch_webhook_delivery_body_digest_check CHECK ((octet_length(body_digest) = 32)),
    CONSTRAINT repo_watch_webhook_delivery_event_name_check CHECK (((octet_length(event_name) BETWEEN 1 AND 64) AND ((event_name COLLATE "C") ~ '^[a-z0-9_]+$'::text))),
    CONSTRAINT repo_watch_webhook_delivery_hook_id_check CHECK (((hook_id > (0)::numeric) AND (hook_id <= '18446744073709551615'::numeric))),
    CONSTRAINT repo_watch_webhook_delivery_receipt_sequence_check CHECK ((receipt_sequence > 0)),
    CONSTRAINT repo_watch_webhook_delivery_repository_check CHECK (repo_watch_repository_is_valid(repository))
);


--
-- Name: repo_watch_webhook_delivery_receipt_sequence_seq; Type: SEQUENCE; Schema: public
--

ALTER TABLE repo_watch_webhook_delivery ALTER COLUMN receipt_sequence ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME repo_watch_webhook_delivery_receipt_sequence_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: repo_watch_webhook_disposition; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_webhook_disposition (
    hook_id numeric(20,0) NOT NULL,
    delivery_id uuid NOT NULL,
    disposition text NOT NULL,
    outcome_code text,
    recorded_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT repo_watch_webhook_disposition_disposition_check CHECK ((disposition = ANY (ARRAY['projected'::text, 'committed'::text, 'duplicate_state'::text, 'superseded'::text, 'ignored'::text, 'quarantined'::text]))),
    CONSTRAINT repo_watch_webhook_disposition_outcome_code_check CHECK (((outcome_code IS NULL) OR ((octet_length(outcome_code) BETWEEN 1 AND 64) AND ((outcome_code COLLATE "C") ~ '^[a-z0-9_]+$'::text))))
);


--
-- Name: repo_watch_webhook_projection; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_webhook_projection (
    hook_id numeric(20,0) NOT NULL,
    delivery_id uuid NOT NULL,
    projection_ordinal integer NOT NULL,
    projection_kind text NOT NULL,
    content_identity_version smallint,
    content_identity bytea,
    event_kind text,
    targeted_query_kind text,
    targeted_query_key text,
    occurrence_key bytea,
    projected_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    cause_code text,
    repository text NOT NULL,
    received_at timestamp with time zone NOT NULL,
    CONSTRAINT repo_watch_webhook_projection_cause_code_check CHECK (((cause_code IS NULL) OR (cause_code = ANY (ARRAY['compressed_transition'::text, 'context_drift'::text, 'cross_drain_shadow_gap'::text])))),
    CONSTRAINT repo_watch_webhook_projection_check CHECK (((targeted_query_kind IS NULL) OR ((targeted_query_kind = ANY (ARRAY['pull_request_hydration'::text, 'mergeability'::text])) AND ((targeted_query_key COLLATE "C") ~ '^[1-9][0-9]*$'::text) AND ((targeted_query_key)::numeric <= '18446744073709551615'::numeric)) OR ((targeted_query_kind = 'check_rollup'::text) AND ((targeted_query_key COLLATE "C") ~ '^[0-9a-f]{40}$'::text)))),
    CONSTRAINT repo_watch_webhook_projection_check1 CHECK ((((projection_kind = 'event'::text) AND (content_identity_version = 1) AND (content_identity IS NOT NULL) AND (event_kind IS NOT NULL) AND (targeted_query_kind IS NULL) AND (targeted_query_key IS NULL) AND (occurrence_key IS NOT NULL)) OR ((projection_kind = 'targeted_query'::text) AND (content_identity_version IS NULL) AND (content_identity IS NULL) AND (event_kind IS NULL) AND (targeted_query_kind IS NOT NULL) AND (targeted_query_key IS NOT NULL) AND (occurrence_key IS NULL)))),
    CONSTRAINT repo_watch_webhook_projection_content_identity_check CHECK (((content_identity IS NULL) OR (octet_length(content_identity) = 32))),
    CONSTRAINT repo_watch_webhook_projection_content_identity_version_check CHECK (((content_identity_version IS NULL) OR (content_identity_version = 1))),
    CONSTRAINT repo_watch_webhook_projection_event_kind_check CHECK (((event_kind IS NULL) OR (event_kind = ANY (ARRAY['pull_request_opened'::text, 'pull_request_closed'::text, 'pull_request_merged'::text, 'head_changed'::text, 'mergeable_state_changed'::text, 'checks_completed'::text, 'check_run_completed'::text, 'branch_workflow_run_completed'::text, 'review_submitted'::text, 'thread_opened'::text, 'thread_resolved'::text, 'labeled'::text, 'unlabeled'::text, 'base_advanced'::text, 'reaction_changed'::text])))),
    CONSTRAINT repo_watch_webhook_projection_occurrence_key_check CHECK (((occurrence_key IS NULL) OR (octet_length(occurrence_key) > 0))),
    CONSTRAINT repo_watch_webhook_projection_projection_kind_check CHECK ((projection_kind = ANY (ARRAY['event'::text, 'targeted_query'::text]))),
    CONSTRAINT repo_watch_webhook_projection_projection_ordinal_check CHECK ((projection_ordinal > 0)),
    CONSTRAINT repo_watch_webhook_projection_repository_check CHECK (repo_watch_repository_is_valid(repository)),
    CONSTRAINT repo_watch_webhook_projection_targeted_query_key_check CHECK (((targeted_query_key IS NULL) OR ((octet_length(targeted_query_key) >= 1) AND (octet_length(targeted_query_key) <= 256)))),
    CONSTRAINT repo_watch_webhook_projection_targeted_query_kind_check CHECK (((targeted_query_kind IS NULL) OR (targeted_query_kind = ANY (ARRAY['pull_request_hydration'::text, 'mergeability'::text, 'check_rollup'::text]))))
);


--
-- Name: repo_watch_webhook_parity; Type: VIEW; Schema: public
--

CREATE VIEW repo_watch_webhook_parity AS
 WITH event_projection AS (
         SELECT delivery.repository,
            delivery.hook_id,
            delivery.delivery_id,
            delivery.event_name,
            delivery.action_name,
            delivery.receipt_sequence,
            delivery.received_at,
            projection.projection_ordinal,
            projection.projection_kind,
            projection.event_kind,
            projection.targeted_query_kind,
            projection.targeted_query_key,
            projection.content_identity_version,
            projection.content_identity,
            projection.cause_code,
            projection.projected_at
           FROM (repo_watch_webhook_delivery delivery
             JOIN repo_watch_webhook_projection projection ON (((projection.hook_id = delivery.hook_id) AND (projection.delivery_id = delivery.delivery_id))))
          WHERE (projection.projection_kind = 'event'::text)
        ), shadow_start AS (
         SELECT repo_watch_webhook_delivery.repository,
            min(repo_watch_webhook_delivery.received_at) AS started_at
           FROM repo_watch_webhook_delivery
          GROUP BY repo_watch_webhook_delivery.repository
        ), primary_start AS (
         SELECT delivery.repository,
            min(disposition.recorded_at) AS promoted_at
           FROM (repo_watch_webhook_disposition disposition
             JOIN repo_watch_webhook_delivery delivery ON (((delivery.hook_id = disposition.hook_id) AND (delivery.delivery_id = disposition.delivery_id))))
          WHERE (disposition.disposition = 'committed'::text)
          GROUP BY delivery.repository
        ), poll_event AS (
         SELECT event.repository,
            event.event_id,
            event.cursor_generation,
            event.event_ordinal,
            event.event_kind,
            event.content_identity_version,
            event.content_identity,
            event.recorded_at
           FROM ((repo_watch_event event
             JOIN shadow_start shadow ON (((shadow.repository = event.repository) AND (event.recorded_at >= shadow.started_at))))
             LEFT JOIN primary_start promotion ON ((promotion.repository = event.repository)))
          WHERE ((event.producer = 'poll'::text) AND (event.content_identity_version = 1) AND ((promotion.promoted_at IS NULL) OR (event.recorded_at < promotion.promoted_at)))
        )
 SELECT COALESCE(webhook.repository, poll.repository) AS repository,
    webhook.hook_id,
    webhook.delivery_id,
    webhook.event_name,
    webhook.action_name,
    webhook.receipt_sequence,
    webhook.projection_ordinal,
    COALESCE(webhook.projection_kind, 'event'::text) AS projection_kind,
    COALESCE(webhook.event_kind, poll.event_kind) AS projected_event_kind,
    webhook.targeted_query_kind,
    webhook.targeted_query_key,
    COALESCE(webhook.content_identity_version, poll.content_identity_version) AS content_identity_version,
    COALESCE(webhook.content_identity, poll.content_identity) AS content_identity,
    poll.event_id AS poll_event_id,
    poll.cursor_generation AS poll_cursor_generation,
    poll.event_ordinal AS poll_event_ordinal,
    webhook.received_at,
    webhook.projected_at,
    poll.recorded_at AS poll_recorded_at,
    (webhook.projected_at - webhook.received_at) AS projection_latency,
    (poll.recorded_at - webhook.received_at) AS poll_latency,
        CASE
            WHEN ((webhook.delivery_id IS NOT NULL) AND (poll.event_id IS NOT NULL)) THEN 'matched'::text
            WHEN (webhook.delivery_id IS NOT NULL) THEN 'webhook_only'::text
            ELSE 'poll_only'::text
        END AS status,
        CASE
            WHEN ((webhook.delivery_id IS NOT NULL) AND (poll.event_id IS NOT NULL)) THEN NULL::text
            WHEN (webhook.delivery_id IS NOT NULL) THEN webhook.cause_code
            WHEN (poll.event_kind = ANY (ARRAY['mergeable_state_changed'::text, 'checks_completed'::text, 'reaction_changed'::text])) THEN 'poll_only_family'::text
            ELSE NULL::text
        END AS cause
   FROM (event_projection webhook
     FULL JOIN poll_event poll ON (((poll.content_identity_version = webhook.content_identity_version) AND (poll.content_identity = webhook.content_identity))))
UNION ALL
 SELECT delivery.repository,
    delivery.hook_id,
    delivery.delivery_id,
    delivery.event_name,
    delivery.action_name,
    delivery.receipt_sequence,
    projection.projection_ordinal,
    projection.projection_kind,
    NULL::text AS projected_event_kind,
    projection.targeted_query_kind,
    projection.targeted_query_key,
    NULL::smallint AS content_identity_version,
    NULL::bytea AS content_identity,
    NULL::uuid AS poll_event_id,
    NULL::bigint AS poll_cursor_generation,
    NULL::integer AS poll_event_ordinal,
    delivery.received_at,
    projection.projected_at,
    NULL::timestamp with time zone AS poll_recorded_at,
    (projection.projected_at - delivery.received_at) AS projection_latency,
    NULL::interval AS poll_latency,
    'not_directly_mapped'::text AS status,
    NULL::text AS cause
   FROM (repo_watch_webhook_delivery delivery
     JOIN repo_watch_webhook_projection projection ON (((projection.hook_id = delivery.hook_id) AND (projection.delivery_id = delivery.delivery_id))))
  WHERE (projection.projection_kind = 'targeted_query'::text);


--
-- Name: repo_watch_webhook_payload; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_webhook_payload (
    hook_id numeric(20,0) NOT NULL,
    delivery_id uuid NOT NULL,
    body bytea NOT NULL,
    CONSTRAINT repo_watch_webhook_payload_body_check CHECK ((octet_length(body) > 0))
);


--
-- Name: repo_watch_webhook_pending; Type: TABLE; Schema: public
--

CREATE TABLE repo_watch_webhook_pending (
    hook_id numeric(20,0) NOT NULL,
    delivery_id uuid NOT NULL,
    repository text NOT NULL,
    receipt_sequence bigint NOT NULL,
    CONSTRAINT repo_watch_webhook_pending_hook_id_check CHECK (((hook_id > (0)::numeric) AND (hook_id <= '18446744073709551615'::numeric))),
    CONSTRAINT repo_watch_webhook_pending_receipt_sequence_check CHECK ((receipt_sequence > 0)),
    CONSTRAINT repo_watch_webhook_pending_repository_check CHECK (repo_watch_repository_is_valid(repository))
);


--
-- Constraints.
--

--
-- Name: commissioned_dispatch commissioned_dispatch_create_command_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY commissioned_dispatch
    ADD CONSTRAINT commissioned_dispatch_create_command_id_key UNIQUE (create_command_id);


--
-- Name: commissioned_dispatch commissioned_dispatch_dispatch_id_session_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY commissioned_dispatch
    ADD CONSTRAINT commissioned_dispatch_dispatch_id_session_id_key UNIQUE (dispatch_id, session_id);


--
-- Name: commissioned_dispatch_headless_approval_escalation commissioned_dispatch_headless_approva_terminal_frontier_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY commissioned_dispatch_headless_approval_escalation
    ADD CONSTRAINT commissioned_dispatch_headless_approva_terminal_frontier_id_key UNIQUE (terminal_frontier_id);


--
-- Name: commissioned_dispatch_headless_approval_escalation commissioned_dispatch_headless_approval_es_failure_entry_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY commissioned_dispatch_headless_approval_escalation
    ADD CONSTRAINT commissioned_dispatch_headless_approval_es_failure_entry_id_key UNIQUE (failure_entry_id);


--
-- Name: commissioned_dispatch_headless_approval_escalation commissioned_dispatch_headless_approval_escalati_request_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY commissioned_dispatch_headless_approval_escalation
    ADD CONSTRAINT commissioned_dispatch_headless_approval_escalati_request_id_key UNIQUE (request_id);


--
-- Name: commissioned_dispatch_headless_approval_escalation commissioned_dispatch_headless_approval_escalation_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY commissioned_dispatch_headless_approval_escalation
    ADD CONSTRAINT commissioned_dispatch_headless_approval_escalation_pkey PRIMARY KEY (model_call_id);


--
-- Name: commissioned_dispatch_headless_approval_escalation commissioned_dispatch_headless_approval_terminal_attempt_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY commissioned_dispatch_headless_approval_escalation
    ADD CONSTRAINT commissioned_dispatch_headless_approval_terminal_attempt_id_key UNIQUE (terminal_attempt_id);


--
-- Name: commissioned_dispatch commissioned_dispatch_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY commissioned_dispatch
    ADD CONSTRAINT commissioned_dispatch_pkey PRIMARY KEY (dispatch_id);


--
-- Name: commissioned_dispatch commissioned_dispatch_session_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY commissioned_dispatch
    ADD CONSTRAINT commissioned_dispatch_session_id_key UNIQUE (session_id);


--
-- Name: repo_watch_achieved_dispatch_settlement repo_watch_achieved_dispatch_settlement_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_achieved_dispatch_settlement
    ADD CONSTRAINT repo_watch_achieved_dispatch_settlement_pkey PRIMARY KEY (dispatch_id);


--
-- Name: repo_watch_complete_poll repo_watch_complete_poll_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_complete_poll
    ADD CONSTRAINT repo_watch_complete_poll_pkey PRIMARY KEY (repository);


--
-- Name: repo_watch_current_held_dispatch repo_watch_current_held_dispatch_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_current_held_dispatch
    ADD CONSTRAINT repo_watch_current_held_dispatch_pkey PRIMARY KEY (dispatch_id);


--
-- Name: repo_watch_current_pull_request repo_watch_current_pull_request_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_current_pull_request
    ADD CONSTRAINT repo_watch_current_pull_request_pkey PRIMARY KEY (repository, pull_request_number);


--
-- Name: repo_watch_current_pull_request_session_count repo_watch_current_pull_request_session_count_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_current_pull_request_session_count
    ADD CONSTRAINT repo_watch_current_pull_request_session_count_pkey PRIMARY KEY (repository, pull_request_number);


--
-- Name: repo_watch_current_pull_request_work_count repo_watch_current_pull_request_work_count_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_current_pull_request_work_count
    ADD CONSTRAINT repo_watch_current_pull_request_work_count_pkey PRIMARY KEY (repository, pull_request_number);


--
-- Name: repo_watch_current_repository_held_count repo_watch_current_repository_held_count_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_current_repository_held_count
    ADD CONSTRAINT repo_watch_current_repository_held_count_pkey PRIMARY KEY (repository);


--
-- Name: repo_watch_current_repository_obligation_count repo_watch_current_repository_obligation_count_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_current_repository_obligation_count
    ADD CONSTRAINT repo_watch_current_repository_obligation_count_pkey PRIMARY KEY (repository);


--
-- Name: repo_watch_cursor repo_watch_cursor_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_cursor
    ADD CONSTRAINT repo_watch_cursor_pkey PRIMARY KEY (repository, generation);


--
-- Name: repo_watch_dispatch_action repo_watch_dispatch_action_create_command_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_action
    ADD CONSTRAINT repo_watch_dispatch_action_create_command_id_key UNIQUE (create_command_id);


--
-- Name: repo_watch_dispatch_action repo_watch_dispatch_action_dispatch_id_session_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_action
    ADD CONSTRAINT repo_watch_dispatch_action_dispatch_id_session_id_key UNIQUE (dispatch_id, session_id);


--
-- Name: repo_watch_dispatch_action repo_watch_dispatch_action_lease_correlation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_action
    ADD CONSTRAINT repo_watch_dispatch_action_lease_correlation_key UNIQUE (dispatch_id, action_ordinal, session_id);


--
-- Name: repo_watch_dispatch_action repo_watch_dispatch_action_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_action
    ADD CONSTRAINT repo_watch_dispatch_action_pkey PRIMARY KEY (dispatch_id, action_ordinal);


--
-- Name: repo_watch_dispatch_batch repo_watch_dispatch_batch_dispatch_id_event_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_batch
    ADD CONSTRAINT repo_watch_dispatch_batch_dispatch_id_event_id_key UNIQUE (dispatch_id, event_id);


--
-- Name: repo_watch_dispatch_batch repo_watch_dispatch_batch_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_batch
    ADD CONSTRAINT repo_watch_dispatch_batch_pkey PRIMARY KEY (dispatch_id);


--
-- Name: repo_watch_dispatch_delivery_intent repo_watch_dispatch_delivery__dispatch_id_action_ordinal_su_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_delivery_intent
    ADD CONSTRAINT repo_watch_dispatch_delivery__dispatch_id_action_ordinal_su_key UNIQUE (dispatch_id, action_ordinal, submit_command_id, accepted_input_id, turn_id);


--
-- Name: repo_watch_dispatch_delivery repo_watch_dispatch_delivery_accepted_input_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_delivery
    ADD CONSTRAINT repo_watch_dispatch_delivery_accepted_input_id_key UNIQUE (accepted_input_id);


--
-- Name: repo_watch_dispatch_delivery_intent repo_watch_dispatch_delivery_inten_cancellation_frontier_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_delivery_intent
    ADD CONSTRAINT repo_watch_dispatch_delivery_inten_cancellation_frontier_id_key UNIQUE (cancellation_frontier_id);


--
-- Name: repo_watch_dispatch_delivery_intent repo_watch_dispatch_delivery_intent_accepted_input_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_delivery_intent
    ADD CONSTRAINT repo_watch_dispatch_delivery_intent_accepted_input_id_key UNIQUE (accepted_input_id);


--
-- Name: repo_watch_dispatch_delivery_intent repo_watch_dispatch_delivery_intent_cancellation_entry_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_delivery_intent
    ADD CONSTRAINT repo_watch_dispatch_delivery_intent_cancellation_entry_id_key UNIQUE (cancellation_entry_id);


--
-- Name: repo_watch_dispatch_delivery_intent repo_watch_dispatch_delivery_intent_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_delivery_intent
    ADD CONSTRAINT repo_watch_dispatch_delivery_intent_pkey PRIMARY KEY (dispatch_id, action_ordinal);


--
-- Name: repo_watch_dispatch_delivery_intent repo_watch_dispatch_delivery_intent_submit_command_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_delivery_intent
    ADD CONSTRAINT repo_watch_dispatch_delivery_intent_submit_command_id_key UNIQUE (submit_command_id);


--
-- Name: repo_watch_dispatch_delivery_intent repo_watch_dispatch_delivery_intent_turn_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_delivery_intent
    ADD CONSTRAINT repo_watch_dispatch_delivery_intent_turn_id_key UNIQUE (turn_id);


--
-- Name: repo_watch_dispatch_delivery repo_watch_dispatch_delivery_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_delivery
    ADD CONSTRAINT repo_watch_dispatch_delivery_pkey PRIMARY KEY (dispatch_id, action_ordinal);


--
-- Name: repo_watch_dispatch_delivery repo_watch_dispatch_delivery_submit_command_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_delivery
    ADD CONSTRAINT repo_watch_dispatch_delivery_submit_command_id_key UNIQUE (submit_command_id);


--
-- Name: repo_watch_dispatch_delivery repo_watch_dispatch_delivery_turn_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_delivery
    ADD CONSTRAINT repo_watch_dispatch_delivery_turn_id_key UNIQUE (turn_id);


--
-- Name: repo_watch_dispatch_obligation repo_watch_dispatch_obligatio_obligation_id_latest_event_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_obligation
    ADD CONSTRAINT repo_watch_dispatch_obligatio_obligation_id_latest_event_id_key UNIQUE (obligation_id, latest_event_id);


--
-- Name: repo_watch_dispatch_obligation_park repo_watch_dispatch_obligation_park_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_obligation_park
    ADD CONSTRAINT repo_watch_dispatch_obligation_park_pkey PRIMARY KEY (obligation_id, transition_ordinal);


--
-- Name: repo_watch_dispatch_obligation repo_watch_dispatch_obligation_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_obligation
    ADD CONSTRAINT repo_watch_dispatch_obligation_pkey PRIMARY KEY (obligation_id);


--
-- Name: repo_watch_dispatch_obligation repo_watch_dispatch_obligation_settled_dispatch_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_obligation
    ADD CONSTRAINT repo_watch_dispatch_obligation_settled_dispatch_id_key UNIQUE (settled_dispatch_id);


--
-- Name: repo_watch_dispatch_release repo_watch_dispatch_release_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_release
    ADD CONSTRAINT repo_watch_dispatch_release_pkey PRIMARY KEY (dispatch_id);


--
-- Name: repo_watch_dispatch_start_lease repo_watch_dispatch_start_lea_dispatch_id_action_ordinal_se_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_start_lease
    ADD CONSTRAINT repo_watch_dispatch_start_lea_dispatch_id_action_ordinal_se_key UNIQUE (dispatch_id, action_ordinal, session_id);


--
-- Name: repo_watch_dispatch_start_lease_expiration repo_watch_dispatch_start_lease_expiration_goal_command_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_start_lease_expiration
    ADD CONSTRAINT repo_watch_dispatch_start_lease_expiration_goal_command_id_key UNIQUE (goal_command_id);


--
-- Name: repo_watch_dispatch_start_lease_expiration repo_watch_dispatch_start_lease_expiration_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_start_lease_expiration
    ADD CONSTRAINT repo_watch_dispatch_start_lease_expiration_pkey PRIMARY KEY (dispatch_id, action_ordinal);


--
-- Name: repo_watch_dispatch_start_lease repo_watch_dispatch_start_lease_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_start_lease
    ADD CONSTRAINT repo_watch_dispatch_start_lease_pkey PRIMARY KEY (dispatch_id, action_ordinal);


--
-- Name: repo_watch_dispatch_start_lease_quarantine repo_watch_dispatch_start_lease_quarantine_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_start_lease_quarantine
    ADD CONSTRAINT repo_watch_dispatch_start_lease_quarantine_pkey PRIMARY KEY (dispatch_id, action_ordinal);


--
-- Name: repo_watch_dispatch_start_lease repo_watch_dispatch_start_lease_session_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_start_lease
    ADD CONSTRAINT repo_watch_dispatch_start_lease_session_id_key UNIQUE (session_id);


--
-- Name: repo_watch_event repo_watch_event_content_identity_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_event
    ADD CONSTRAINT repo_watch_event_content_identity_key UNIQUE (content_identity_version, content_identity);


--
-- Name: repo_watch_event repo_watch_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_event
    ADD CONSTRAINT repo_watch_event_pkey PRIMARY KEY (event_id);


--
-- Name: repo_watch_event repo_watch_event_position_identity_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_event
    ADD CONSTRAINT repo_watch_event_position_identity_key UNIQUE (repository, cursor_generation, event_ordinal, event_id);


--
-- Name: repo_watch_event repo_watch_event_repository_cursor_generation_event_ordinal_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_event
    ADD CONSTRAINT repo_watch_event_repository_cursor_generation_event_ordinal_key UNIQUE (repository, cursor_generation, event_ordinal);


--
-- Name: repo_watch_headless_approval_escalation repo_watch_headless_approval_escalatio_terminal_frontier_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_headless_approval_escalation
    ADD CONSTRAINT repo_watch_headless_approval_escalatio_terminal_frontier_id_key UNIQUE (terminal_frontier_id);


--
-- Name: repo_watch_headless_approval_escalation repo_watch_headless_approval_escalation_failure_entry_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_headless_approval_escalation
    ADD CONSTRAINT repo_watch_headless_approval_escalation_failure_entry_id_key UNIQUE (failure_entry_id);


--
-- Name: repo_watch_headless_approval_escalation repo_watch_headless_approval_escalation_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_headless_approval_escalation
    ADD CONSTRAINT repo_watch_headless_approval_escalation_pkey PRIMARY KEY (model_call_id);


--
-- Name: repo_watch_headless_approval_escalation repo_watch_headless_approval_escalation_request_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_headless_approval_escalation
    ADD CONSTRAINT repo_watch_headless_approval_escalation_request_id_key UNIQUE (request_id);


--
-- Name: repo_watch_headless_approval_escalation repo_watch_headless_approval_escalation_terminal_attempt_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_headless_approval_escalation
    ADD CONSTRAINT repo_watch_headless_approval_escalation_terminal_attempt_id_key UNIQUE (terminal_attempt_id);


--
-- Name: repo_watch_lifecycle_cutoff_goal repo_watch_lifecycle_cutoff_goal_goal_command_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_lifecycle_cutoff_goal
    ADD CONSTRAINT repo_watch_lifecycle_cutoff_goal_goal_command_id_key UNIQUE (goal_command_id);


--
-- Name: repo_watch_lifecycle_cutoff_goal repo_watch_lifecycle_cutoff_goal_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_lifecycle_cutoff_goal
    ADD CONSTRAINT repo_watch_lifecycle_cutoff_goal_pkey PRIMARY KEY (event_id, session_id);


--
-- Name: repo_watch_lifecycle_cutoff repo_watch_lifecycle_cutoff_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_lifecycle_cutoff
    ADD CONSTRAINT repo_watch_lifecycle_cutoff_pkey PRIMARY KEY (event_id);


--
-- Name: repo_watch_repository_key repo_watch_repository_key_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_repository_key
    ADD CONSTRAINT repo_watch_repository_key_pkey PRIMARY KEY (repository);


--
-- Name: repo_watch_rule_activation repo_watch_rule_activation_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_rule_activation
    ADD CONSTRAINT repo_watch_rule_activation_pkey PRIMARY KEY (repository, rule_id, rule_version);


--
-- Name: repo_watch_rule_deactivation repo_watch_rule_deactivation_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_rule_deactivation
    ADD CONSTRAINT repo_watch_rule_deactivation_pkey PRIMARY KEY (repository, rule_id, rule_version);


--
-- Name: repo_watch_rule_evaluation repo_watch_rule_evaluation_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_rule_evaluation
    ADD CONSTRAINT repo_watch_rule_evaluation_pkey PRIMARY KEY (repository, rule_id, rule_version, event_id);


--
-- Name: repo_watch_rule_evaluation repo_watch_rule_evaluation_repository_rule_id_rule_version__key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_rule_evaluation
    ADD CONSTRAINT repo_watch_rule_evaluation_repository_rule_id_rule_version__key UNIQUE (repository, rule_id, rule_version, cursor_generation, event_ordinal);


--
-- Name: repo_watch_rule_field_fingerprint repo_watch_rule_field_fingerprint_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_rule_field_fingerprint
    ADD CONSTRAINT repo_watch_rule_field_fingerprint_pkey PRIMARY KEY (repository, rule_id, rule_version);


--
-- Name: repo_watch_webhook_delivery repo_watch_webhook_delivery_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_webhook_delivery
    ADD CONSTRAINT repo_watch_webhook_delivery_pkey PRIMARY KEY (hook_id, delivery_id);


--
-- Name: repo_watch_webhook_delivery repo_watch_webhook_delivery_receipt_sequence_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_webhook_delivery
    ADD CONSTRAINT repo_watch_webhook_delivery_receipt_sequence_key UNIQUE (receipt_sequence);


--
-- Name: repo_watch_webhook_disposition repo_watch_webhook_disposition_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_webhook_disposition
    ADD CONSTRAINT repo_watch_webhook_disposition_pkey PRIMARY KEY (hook_id, delivery_id);


--
-- Name: repo_watch_webhook_payload repo_watch_webhook_payload_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_webhook_payload
    ADD CONSTRAINT repo_watch_webhook_payload_pkey PRIMARY KEY (hook_id, delivery_id);


--
-- Name: repo_watch_webhook_pending repo_watch_webhook_pending_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_webhook_pending
    ADD CONSTRAINT repo_watch_webhook_pending_pkey PRIMARY KEY (hook_id, delivery_id);


--
-- Name: repo_watch_webhook_pending repo_watch_webhook_pending_receipt_sequence_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_webhook_pending
    ADD CONSTRAINT repo_watch_webhook_pending_receipt_sequence_key UNIQUE (receipt_sequence);


--
-- Name: repo_watch_webhook_projection repo_watch_webhook_projection_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_webhook_projection
    ADD CONSTRAINT repo_watch_webhook_projection_pkey PRIMARY KEY (hook_id, delivery_id, projection_ordinal);


--
-- Indexes.
--

--
-- Name: commissioned_dispatch_pull_request_recorded_at; Type: INDEX; Schema: public
--

CREATE INDEX commissioned_dispatch_pull_request_recorded_at ON commissioned_dispatch USING btree (repository, pull_request_number, recorded_at DESC, session_id DESC) WHERE (target_kind = 'pull_request'::text);


--
-- Name: commissioned_dispatch_pull_request_target; Type: INDEX; Schema: public
--

CREATE INDEX commissioned_dispatch_pull_request_target ON commissioned_dispatch USING btree (target_kind, repository, pull_request_number, recorded_at DESC, dispatch_id DESC) WHERE (target_kind = 'pull_request'::text);


--
-- Name: repo_watch_achieved_dispatch_settlement_pull_request; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_achieved_dispatch_settlement_pull_request ON repo_watch_achieved_dispatch_settlement USING btree (repository, pull_request_number, released_at DESC, dispatch_id DESC) WHERE (pull_request_number IS NOT NULL);


--
-- Name: repo_watch_achieved_dispatch_settlement_repository; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_achieved_dispatch_settlement_repository ON repo_watch_achieved_dispatch_settlement USING btree (repository, released_at DESC, dispatch_id DESC);


--
-- Name: repo_watch_current_held_dispatch_pull_request; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_current_held_dispatch_pull_request ON repo_watch_current_held_dispatch USING btree (repository, pull_request_number, held_since DESC, dispatch_id DESC) WHERE (pull_request_number IS NOT NULL);


--
-- Name: repo_watch_current_held_dispatch_repository_page; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_current_held_dispatch_repository_page ON repo_watch_current_held_dispatch USING btree (repository, held_since, dispatch_id);


--
-- Name: repo_watch_current_held_dispatch_singleton; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX repo_watch_current_held_dispatch_singleton ON repo_watch_current_held_dispatch USING btree (rule_id, rule_version, singleton_scope, singleton_repository, singleton_pull_request_number, singleton_stack_root_pull_request_number) NULLS NOT DISTINCT;


--
-- Name: repo_watch_current_pull_request_children; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_current_pull_request_children ON repo_watch_current_pull_request USING btree (repository, lifecycle, base_branch, pull_request_number);


--
-- Name: repo_watch_current_pull_request_parent; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_current_pull_request_parent ON repo_watch_current_pull_request USING btree (repository, lifecycle, head_repository, head_branch, pull_request_number);


--
-- Name: repo_watch_current_singleton_cooldown_identity; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX repo_watch_current_singleton_cooldown_identity ON repo_watch_current_singleton_cooldown USING btree (rule_id, rule_version, singleton_scope, singleton_repository, singleton_pull_request_number, singleton_stack_root_pull_request_number) NULLS NOT DISTINCT;


--
-- Name: repo_watch_dispatch_action_event_target; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_dispatch_action_event_target ON repo_watch_dispatch_action USING btree (event_id, recorded_at DESC, dispatch_id DESC, session_id DESC);


--
-- Name: repo_watch_dispatch_action_pull_request_recorded_at; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_dispatch_action_pull_request_recorded_at ON repo_watch_dispatch_action USING btree (repository, pull_request_number, recorded_at DESC, session_id DESC) WHERE (pull_request_number IS NOT NULL);


--
-- Name: repo_watch_dispatch_action_session; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_dispatch_action_session ON repo_watch_dispatch_action USING btree (session_id);


--
-- Name: repo_watch_dispatch_batch_pull_request_admission; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_dispatch_batch_pull_request_admission ON repo_watch_dispatch_batch USING btree (repository, pull_request_number, admitted_at DESC, dispatch_id DESC) WHERE (pull_request_number IS NOT NULL);


--
-- Name: repo_watch_dispatch_batch_repository_admission; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_dispatch_batch_repository_admission ON repo_watch_dispatch_batch USING btree (repository, admitted_at DESC, dispatch_id DESC);


--
-- Name: repo_watch_dispatch_obligation_active_singleton; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX repo_watch_dispatch_obligation_active_singleton ON repo_watch_dispatch_obligation USING btree (rule_id, rule_version, singleton_scope, singleton_repository, singleton_pull_request_number, singleton_stack_root_pull_request_number) NULLS NOT DISTINCT WHERE (settled_kind IS NULL);


--
-- Name: repo_watch_dispatch_obligation_latest_event; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_dispatch_obligation_latest_event ON repo_watch_dispatch_obligation USING btree (latest_event_id) WHERE (settled_kind IS NULL);


--
-- Name: repo_watch_dispatch_obligation_park_release_event; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_dispatch_obligation_park_release_event ON repo_watch_dispatch_obligation_park USING btree (release_event_id) WHERE (release_event_id IS NOT NULL);


--
-- Name: repo_watch_dispatch_obligation_parked; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_dispatch_obligation_parked ON repo_watch_dispatch_obligation USING btree (repository, rule_id, rule_version) WHERE ((settled_kind IS NULL) AND (parked_at IS NOT NULL));


--
-- Name: repo_watch_dispatch_obligation_ready_order; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_dispatch_obligation_ready_order ON repo_watch_dispatch_obligation USING btree (repository, rule_id, rule_version, owed_since) WHERE (settled_kind IS NULL);


--
-- Name: repo_watch_dispatch_obligation_repository_page; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_dispatch_obligation_repository_page ON repo_watch_dispatch_obligation USING btree (repository, owed_since, obligation_id) WHERE (settled_kind IS NULL);


--
-- Name: repo_watch_dispatch_singleton_lookup; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_dispatch_singleton_lookup ON repo_watch_dispatch_batch USING btree (rule_id, rule_version, singleton_scope, singleton_repository, singleton_pull_request_number, singleton_stack_root_pull_request_number, admitted_at DESC);


--
-- Name: repo_watch_dispatch_start_lease_expiry; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_dispatch_start_lease_expiry ON repo_watch_dispatch_start_lease USING btree (expires_at, session_id);


--
-- Name: repo_watch_event_pull_request_position; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_event_pull_request_position ON repo_watch_event USING btree (repository, pull_request_number, cursor_generation DESC, event_ordinal DESC) WHERE (pull_request_number IS NOT NULL);


--
-- Name: repo_watch_event_pull_request_target; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_event_pull_request_target ON repo_watch_event USING btree (repository, pull_request_number, event_id) WHERE (target_kind = 'pull_request'::text);


--
-- Name: repo_watch_event_repository_recorded_at; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_event_repository_recorded_at ON repo_watch_event USING btree (repository, recorded_at DESC);


--
-- Name: repo_watch_repository_key_c_order; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_repository_key_c_order ON repo_watch_repository_key USING btree (repository COLLATE "C");


--
-- Name: repo_watch_rule_evaluation_actionable_event; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_rule_evaluation_actionable_event ON repo_watch_rule_evaluation USING btree (repository, pull_request_number, cursor_generation DESC, event_ordinal DESC, event_id) WHERE (outcome_kind <> 'not_matched'::text);


--
-- Name: repo_watch_rule_evaluation_cursor; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_rule_evaluation_cursor ON repo_watch_rule_evaluation USING btree (repository, rule_id, rule_version, cursor_generation DESC, event_ordinal DESC);


--
-- Name: repo_watch_webhook_delivery_pending_order; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_webhook_delivery_pending_order ON repo_watch_webhook_delivery USING btree (repository, receipt_sequence);


--
-- Name: repo_watch_webhook_delivery_repository_received_at; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_webhook_delivery_repository_received_at ON repo_watch_webhook_delivery USING btree (repository, received_at DESC);


--
-- Name: repo_watch_webhook_pending_order; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_webhook_pending_order ON repo_watch_webhook_pending USING btree (repository, receipt_sequence);


--
-- Name: repo_watch_webhook_projection_content_identity; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_webhook_projection_content_identity ON repo_watch_webhook_projection USING btree (content_identity_version, content_identity) WHERE (projection_kind = 'event'::text);


--
-- Name: repo_watch_webhook_projection_repository_time; Type: INDEX; Schema: public
--

CREATE INDEX repo_watch_webhook_projection_repository_time ON repo_watch_webhook_projection USING btree (repository, projected_at DESC, delivery_id, projection_ordinal DESC);


--
-- Triggers.
--

--
-- Name: commissioned_dispatch commissioned_dispatch_counts_pull_request_session; Type: TRIGGER; Schema: public
--

CREATE TRIGGER commissioned_dispatch_counts_pull_request_session AFTER INSERT ON commissioned_dispatch FOR EACH ROW EXECUTE FUNCTION increment_repo_watch_pull_request_session_count();


--
-- Name: commissioned_dispatch_headless_approval_escalation commissioned_dispatch_headless_escalation_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER commissioned_dispatch_headless_escalation_is_append_only BEFORE DELETE OR UPDATE ON commissioned_dispatch_headless_approval_escalation FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: commissioned_dispatch_headless_approval_escalation commissioned_dispatch_headless_escalation_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER commissioned_dispatch_headless_escalation_reject_truncate BEFORE TRUNCATE ON commissioned_dispatch_headless_approval_escalation FOR EACH STATEMENT EXECUTE FUNCTION reject_commissioned_dispatch_table_truncate();


--
-- Name: commissioned_dispatch commissioned_dispatch_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER commissioned_dispatch_is_append_only BEFORE DELETE OR UPDATE ON commissioned_dispatch FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: commissioned_dispatch commissioned_dispatch_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER commissioned_dispatch_reject_truncate BEFORE TRUNCATE ON commissioned_dispatch FOR EACH STATEMENT EXECUTE FUNCTION reject_commissioned_dispatch_table_truncate();


--
-- Name: repo_watch_achieved_dispatch_settlement repo_watch_achieved_dispatch_settlement_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_achieved_dispatch_settlement_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_achieved_dispatch_settlement FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_achieved_dispatch_settlement repo_watch_achieved_dispatch_settlement_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_achieved_dispatch_settlement_reject_truncate BEFORE TRUNCATE ON repo_watch_achieved_dispatch_settlement FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_current_held_dispatch repo_watch_current_held_dispatch_maintains_pull_request_count; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_current_held_dispatch_maintains_pull_request_count AFTER INSERT OR DELETE ON repo_watch_current_held_dispatch FOR EACH ROW EXECUTE FUNCTION maintain_repo_watch_pull_request_held_count();


--
-- Name: repo_watch_current_held_dispatch repo_watch_current_held_dispatch_maintains_repository_count; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_current_held_dispatch_maintains_repository_count AFTER INSERT OR DELETE ON repo_watch_current_held_dispatch FOR EACH ROW EXECUTE FUNCTION maintain_repo_watch_repository_held_count();


--
-- Name: repo_watch_cursor repo_watch_cursor_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_cursor_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_cursor FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_cursor repo_watch_cursor_records_transaction; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_cursor_records_transaction BEFORE INSERT ON repo_watch_cursor FOR EACH ROW EXECUTE FUNCTION stamp_repo_watch_cursor_transaction();


--
-- Name: repo_watch_cursor repo_watch_cursor_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_cursor_reject_truncate BEFORE TRUNCATE ON repo_watch_cursor FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_cursor repo_watch_cursor_remembers_repository; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_cursor_remembers_repository AFTER INSERT ON repo_watch_cursor FOR EACH ROW EXECUTE FUNCTION remember_repo_watch_repository_key();


--
-- Name: repo_watch_dispatch_action repo_watch_dispatch_action_completes_batch; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER repo_watch_dispatch_action_completes_batch AFTER INSERT OR DELETE OR UPDATE ON repo_watch_dispatch_action DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION repo_watch_dispatch_batch_has_complete_actions();


--
-- Name: repo_watch_dispatch_action repo_watch_dispatch_action_counts_pull_request_session; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_action_counts_pull_request_session AFTER INSERT ON repo_watch_dispatch_action FOR EACH ROW EXECUTE FUNCTION increment_repo_watch_pull_request_session_count();


--
-- Name: repo_watch_dispatch_action repo_watch_dispatch_action_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_action_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_dispatch_action FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_dispatch_action repo_watch_dispatch_action_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_action_reject_truncate BEFORE TRUNCATE ON repo_watch_dispatch_action FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_dispatch_action repo_watch_dispatch_action_stamps_target; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_action_stamps_target BEFORE INSERT ON repo_watch_dispatch_action FOR EACH ROW EXECUTE FUNCTION stamp_repo_watch_dispatch_action_target();


--
-- Name: repo_watch_dispatch_batch repo_watch_dispatch_batch_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_batch_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_dispatch_batch FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_dispatch_batch repo_watch_dispatch_batch_projects_current_hold; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_batch_projects_current_hold AFTER INSERT ON repo_watch_dispatch_batch FOR EACH ROW EXECUTE FUNCTION project_repo_watch_current_held_dispatch();


--
-- Name: repo_watch_dispatch_batch repo_watch_dispatch_batch_records_delivered_state; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_batch_records_delivered_state BEFORE INSERT ON repo_watch_dispatch_batch FOR EACH ROW EXECUTE FUNCTION repo_watch_stamp_dispatch_batch_delivered_state();


--
-- Name: repo_watch_dispatch_batch repo_watch_dispatch_batch_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_batch_reject_truncate BEFORE TRUNCATE ON repo_watch_dispatch_batch FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_dispatch_batch repo_watch_dispatch_batch_requires_complete_actions; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER repo_watch_dispatch_batch_requires_complete_actions AFTER INSERT ON repo_watch_dispatch_batch DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION repo_watch_dispatch_batch_has_complete_actions();


--
-- Name: repo_watch_dispatch_batch repo_watch_dispatch_batch_stamps_target; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_batch_stamps_target BEFORE INSERT ON repo_watch_dispatch_batch FOR EACH ROW EXECUTE FUNCTION stamp_repo_watch_dispatch_batch_target();


--
-- Name: repo_watch_dispatch_delivery_intent repo_watch_dispatch_delivery_intent_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_delivery_intent_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_dispatch_delivery_intent FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_dispatch_delivery_intent repo_watch_dispatch_delivery_intent_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_delivery_intent_reject_truncate BEFORE TRUNCATE ON repo_watch_dispatch_delivery_intent FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_dispatch_delivery repo_watch_dispatch_delivery_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_delivery_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_dispatch_delivery FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_dispatch_delivery repo_watch_dispatch_delivery_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_delivery_reject_truncate BEFORE TRUNCATE ON repo_watch_dispatch_delivery FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_dispatch_obligation repo_watch_dispatch_obligation_maintains_pull_request_count; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_obligation_maintains_pull_request_count AFTER INSERT OR UPDATE OF latest_event_id, settled_kind ON repo_watch_dispatch_obligation FOR EACH ROW EXECUTE FUNCTION maintain_repo_watch_pull_request_obligation_count();


--
-- Name: repo_watch_dispatch_obligation repo_watch_dispatch_obligation_maintains_repository_count; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_obligation_maintains_repository_count AFTER INSERT OR UPDATE OF repository, settled_kind ON repo_watch_dispatch_obligation FOR EACH ROW EXECUTE FUNCTION maintain_repo_watch_repository_obligation_count();


--
-- Name: repo_watch_dispatch_obligation_park repo_watch_dispatch_obligation_park_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_obligation_park_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_dispatch_obligation_park FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_dispatch_obligation_park repo_watch_dispatch_obligation_park_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_obligation_park_reject_truncate BEFORE TRUNCATE ON repo_watch_dispatch_obligation_park FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_dispatch_obligation repo_watch_dispatch_obligation_reject_delete; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_obligation_reject_delete BEFORE DELETE ON repo_watch_dispatch_obligation FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_dispatch_obligation repo_watch_dispatch_obligation_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_obligation_reject_truncate BEFORE TRUNCATE ON repo_watch_dispatch_obligation FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_rule_deactivation repo_watch_dispatch_obligation_settles_on_deactivation; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_obligation_settles_on_deactivation AFTER INSERT ON repo_watch_rule_deactivation FOR EACH ROW EXECUTE FUNCTION repo_watch_settle_deactivated_dispatch_obligations();


--
-- Name: repo_watch_dispatch_release repo_watch_dispatch_release_clears_current_hold; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_release_clears_current_hold AFTER INSERT ON repo_watch_dispatch_release FOR EACH ROW EXECUTE FUNCTION clear_repo_watch_current_held_dispatch();


--
-- Name: repo_watch_dispatch_release repo_watch_dispatch_release_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_release_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_dispatch_release FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: goal_event repo_watch_dispatch_release_on_terminal_goal; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER repo_watch_dispatch_release_on_terminal_goal AFTER INSERT ON goal_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION repo_watch_release_completed_dispatch_batches_for_goal();


--
-- Name: turn_lifecycle repo_watch_dispatch_release_on_terminal_turn; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_release_on_terminal_turn AFTER UPDATE OF state_kind ON turn_lifecycle FOR EACH ROW EXECUTE FUNCTION repo_watch_release_completed_dispatch_batches();


--
-- Name: repo_watch_dispatch_release repo_watch_dispatch_release_projects_achievement; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_release_projects_achievement AFTER INSERT ON repo_watch_dispatch_release FOR EACH ROW EXECUTE FUNCTION project_repo_watch_achieved_dispatch_settlement();


--
-- Name: repo_watch_dispatch_release repo_watch_dispatch_release_projects_singleton_cooldown; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_release_projects_singleton_cooldown AFTER INSERT ON repo_watch_dispatch_release FOR EACH ROW EXECUTE FUNCTION project_repo_watch_singleton_cooldown();


--
-- Name: repo_watch_dispatch_release repo_watch_dispatch_release_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_release_reject_truncate BEFORE TRUNCATE ON repo_watch_dispatch_release FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_dispatch_release repo_watch_dispatch_release_restarts_obligation_backoff; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_release_restarts_obligation_backoff AFTER INSERT ON repo_watch_dispatch_release FOR EACH ROW EXECUTE FUNCTION repo_watch_restart_dispatch_obligation_backoff();


--
-- Name: repo_watch_dispatch_start_lease_expiration repo_watch_dispatch_start_lease_expiration_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_start_lease_expiration_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_dispatch_start_lease_expiration FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_dispatch_start_lease_expiration repo_watch_dispatch_start_lease_expiration_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_start_lease_expiration_reject_truncate BEFORE TRUNCATE ON repo_watch_dispatch_start_lease_expiration FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_dispatch_start_lease repo_watch_dispatch_start_lease_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_start_lease_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_dispatch_start_lease FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_dispatch_start_lease_quarantine repo_watch_dispatch_start_lease_quarantine_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_start_lease_quarantine_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_dispatch_start_lease_quarantine FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_dispatch_start_lease_quarantine repo_watch_dispatch_start_lease_quarantine_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_start_lease_quarantine_reject_truncate BEFORE TRUNCATE ON repo_watch_dispatch_start_lease_quarantine FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_dispatch_start_lease repo_watch_dispatch_start_lease_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_dispatch_start_lease_reject_truncate BEFORE TRUNCATE ON repo_watch_dispatch_start_lease FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_event repo_watch_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_event_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_event repo_watch_event_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_event_reject_truncate BEFORE TRUNCATE ON repo_watch_event FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_event repo_watch_event_requires_cursor_commit; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER repo_watch_event_requires_cursor_commit AFTER INSERT ON repo_watch_event DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION require_repo_watch_event_cursor_commit();


--
-- Name: repo_watch_headless_approval_escalation repo_watch_headless_approval_escalation_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_headless_approval_escalation_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_headless_approval_escalation FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_headless_approval_escalation repo_watch_headless_approval_escalation_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_headless_approval_escalation_reject_truncate BEFORE TRUNCATE ON repo_watch_headless_approval_escalation FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_lifecycle_cutoff_goal repo_watch_lifecycle_cutoff_goal_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_lifecycle_cutoff_goal_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_lifecycle_cutoff_goal FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_lifecycle_cutoff_goal repo_watch_lifecycle_cutoff_goal_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_lifecycle_cutoff_goal_reject_truncate BEFORE TRUNCATE ON repo_watch_lifecycle_cutoff_goal FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_lifecycle_cutoff repo_watch_lifecycle_cutoff_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_lifecycle_cutoff_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_lifecycle_cutoff FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_lifecycle_cutoff repo_watch_lifecycle_cutoff_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_lifecycle_cutoff_reject_truncate BEFORE TRUNCATE ON repo_watch_lifecycle_cutoff FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_rule_activation repo_watch_rule_activation_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_rule_activation_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_rule_activation FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_rule_activation repo_watch_rule_activation_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_rule_activation_reject_truncate BEFORE TRUNCATE ON repo_watch_rule_activation FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_rule_deactivation repo_watch_rule_deactivation_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_rule_deactivation_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_rule_deactivation FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_rule_deactivation repo_watch_rule_deactivation_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_rule_deactivation_reject_truncate BEFORE TRUNCATE ON repo_watch_rule_deactivation FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_rule_evaluation repo_watch_rule_evaluation_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_rule_evaluation_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_rule_evaluation FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_rule_evaluation repo_watch_rule_evaluation_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_rule_evaluation_reject_truncate BEFORE TRUNCATE ON repo_watch_rule_evaluation FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_rule_field_fingerprint repo_watch_rule_field_fingerprint_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_rule_field_fingerprint_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_rule_field_fingerprint FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_rule_field_fingerprint repo_watch_rule_field_fingerprint_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_rule_field_fingerprint_reject_truncate BEFORE TRUNCATE ON repo_watch_rule_field_fingerprint FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_webhook_delivery repo_watch_webhook_delivery_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_webhook_delivery_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_webhook_delivery FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_webhook_delivery repo_watch_webhook_delivery_registers_pending; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_webhook_delivery_registers_pending AFTER INSERT ON repo_watch_webhook_delivery FOR EACH ROW EXECUTE FUNCTION register_repo_watch_webhook_pending();


--
-- Name: repo_watch_webhook_delivery repo_watch_webhook_delivery_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_webhook_delivery_reject_truncate BEFORE TRUNCATE ON repo_watch_webhook_delivery FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_webhook_delivery repo_watch_webhook_delivery_remembers_repository; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_webhook_delivery_remembers_repository AFTER INSERT ON repo_watch_webhook_delivery FOR EACH ROW EXECUTE FUNCTION remember_repo_watch_repository_key();


--
-- Name: repo_watch_webhook_disposition repo_watch_webhook_disposition_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_webhook_disposition_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_webhook_disposition FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_webhook_disposition repo_watch_webhook_disposition_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_webhook_disposition_reject_truncate BEFORE TRUNCATE ON repo_watch_webhook_disposition FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_webhook_disposition repo_watch_webhook_disposition_retires_pending; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_webhook_disposition_retires_pending AFTER INSERT ON repo_watch_webhook_disposition FOR EACH ROW EXECUTE FUNCTION retire_repo_watch_webhook_pending();


--
-- Name: repo_watch_webhook_payload repo_watch_webhook_payload_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_webhook_payload_reject_truncate BEFORE TRUNCATE ON repo_watch_webhook_payload FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_webhook_payload repo_watch_webhook_payload_retains_unexpired; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_webhook_payload_retains_unexpired BEFORE DELETE OR UPDATE ON repo_watch_webhook_payload FOR EACH ROW EXECUTE FUNCTION retain_repo_watch_webhook_payload_until_expired();


--
-- Name: repo_watch_webhook_pending repo_watch_webhook_pending_guards_mutation; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_webhook_pending_guards_mutation BEFORE INSERT OR DELETE OR UPDATE ON repo_watch_webhook_pending FOR EACH ROW EXECUTE FUNCTION guard_repo_watch_webhook_pending_mutation();


--
-- Name: repo_watch_webhook_pending repo_watch_webhook_pending_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_webhook_pending_reject_truncate BEFORE TRUNCATE ON repo_watch_webhook_pending FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_webhook_projection repo_watch_webhook_projection_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_webhook_projection_is_append_only BEFORE DELETE OR UPDATE ON repo_watch_webhook_projection FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: repo_watch_webhook_projection repo_watch_webhook_projection_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_webhook_projection_reject_truncate BEFORE TRUNCATE ON repo_watch_webhook_projection FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();


--
-- Name: repo_watch_webhook_projection repo_watch_webhook_projection_stamps_delivery; Type: TRIGGER; Schema: public
--

CREATE TRIGGER repo_watch_webhook_projection_stamps_delivery BEFORE INSERT ON repo_watch_webhook_projection FOR EACH ROW EXECUTE FUNCTION stamp_repo_watch_webhook_projection_delivery();


--
-- Foreign keys.
--

--
-- Name: commissioned_dispatch commissioned_dispatch_create_command_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY commissioned_dispatch
    ADD CONSTRAINT commissioned_dispatch_create_command_id_fkey FOREIGN KEY (create_command_id) REFERENCES durable_command(command_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: commissioned_dispatch_headless_approval_escalation commissioned_dispatch_headles_request_id_turn_id_session_i_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY commissioned_dispatch_headless_approval_escalation
    ADD CONSTRAINT commissioned_dispatch_headles_request_id_turn_id_session_i_fkey FOREIGN KEY (request_id, turn_id, session_id) REFERENCES tool_request(request_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: commissioned_dispatch_headless_approval_escalation commissioned_dispatch_headles_session_id_terminal_frontier_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY commissioned_dispatch_headless_approval_escalation
    ADD CONSTRAINT commissioned_dispatch_headles_session_id_terminal_frontier_fkey FOREIGN KEY (session_id, terminal_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: commissioned_dispatch_headless_approval_escalation commissioned_dispatch_headles_terminal_attempt_id_turn_id__fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY commissioned_dispatch_headless_approval_escalation
    ADD CONSTRAINT commissioned_dispatch_headles_terminal_attempt_id_turn_id__fkey FOREIGN KEY (terminal_attempt_id, turn_id, session_id) REFERENCES turn_attempt(turn_attempt_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: commissioned_dispatch_headless_approval_escalation commissioned_dispatch_headless_ap_model_call_id_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY commissioned_dispatch_headless_approval_escalation
    ADD CONSTRAINT commissioned_dispatch_headless_ap_model_call_id_session_id_fkey FOREIGN KEY (model_call_id, session_id) REFERENCES tool_approval_judge_model_call(model_call_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: commissioned_dispatch_headless_approval_escalation commissioned_dispatch_headless_appr_dispatch_id_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY commissioned_dispatch_headless_approval_escalation
    ADD CONSTRAINT commissioned_dispatch_headless_appr_dispatch_id_session_id_fkey FOREIGN KEY (dispatch_id, session_id) REFERENCES commissioned_dispatch(dispatch_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: commissioned_dispatch_headless_approval_escalation commissioned_dispatch_headless_approval_turn_id_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY commissioned_dispatch_headless_approval_escalation
    ADD CONSTRAINT commissioned_dispatch_headless_approval_turn_id_session_id_fkey FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: commissioned_dispatch_headless_approval_escalation commissioned_dispatch_headless_session_id_failure_entry_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY commissioned_dispatch_headless_approval_escalation
    ADD CONSTRAINT commissioned_dispatch_headless_session_id_failure_entry_id_fkey FOREIGN KEY (session_id, failure_entry_id) REFERENCES semantic_transcript_entry(source_session_id, semantic_entry_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: commissioned_dispatch commissioned_dispatch_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY commissioned_dispatch
    ADD CONSTRAINT commissioned_dispatch_session_id_fkey FOREIGN KEY (session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_achieved_dispatch_settlement repo_watch_achieved_dispatch_settlement_dispatch_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_achieved_dispatch_settlement
    ADD CONSTRAINT repo_watch_achieved_dispatch_settlement_dispatch_id_fkey FOREIGN KEY (dispatch_id) REFERENCES repo_watch_dispatch_release(dispatch_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_achieved_dispatch_settlement repo_watch_achieved_dispatch_settlement_event_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_achieved_dispatch_settlement
    ADD CONSTRAINT repo_watch_achieved_dispatch_settlement_event_id_fkey FOREIGN KEY (event_id) REFERENCES repo_watch_event(event_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_current_held_dispatch repo_watch_current_held_dispatch_dispatch_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_current_held_dispatch
    ADD CONSTRAINT repo_watch_current_held_dispatch_dispatch_id_fkey FOREIGN KEY (dispatch_id) REFERENCES repo_watch_dispatch_batch(dispatch_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_current_pull_request repo_watch_current_pull_reque_repository_cursor_generation_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_current_pull_request
    ADD CONSTRAINT repo_watch_current_pull_reque_repository_cursor_generation_fkey FOREIGN KEY (repository, cursor_generation) REFERENCES repo_watch_cursor(repository, generation) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_action repo_watch_dispatch_action_create_command_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_action
    ADD CONSTRAINT repo_watch_dispatch_action_create_command_id_fkey FOREIGN KEY (create_command_id) REFERENCES durable_command(command_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_action repo_watch_dispatch_action_dispatch_id_event_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_action
    ADD CONSTRAINT repo_watch_dispatch_action_dispatch_id_event_id_fkey FOREIGN KEY (dispatch_id, event_id) REFERENCES repo_watch_dispatch_batch(dispatch_id, event_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_action repo_watch_dispatch_action_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_action
    ADD CONSTRAINT repo_watch_dispatch_action_session_id_fkey FOREIGN KEY (session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_batch repo_watch_dispatch_batch_delivered_state_event_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_batch
    ADD CONSTRAINT repo_watch_dispatch_batch_delivered_state_event_id_fkey FOREIGN KEY (delivered_state_event_id) REFERENCES repo_watch_event(event_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_batch repo_watch_dispatch_batch_event_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_batch
    ADD CONSTRAINT repo_watch_dispatch_batch_event_id_fkey FOREIGN KEY (event_id) REFERENCES repo_watch_event(event_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_delivery repo_watch_dispatch_delivery_accepted_input_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_delivery
    ADD CONSTRAINT repo_watch_dispatch_delivery_accepted_input_id_fkey FOREIGN KEY (accepted_input_id) REFERENCES accepted_input(accepted_input_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_delivery repo_watch_dispatch_delivery_dispatch_id_action_ordinal_su_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_delivery
    ADD CONSTRAINT repo_watch_dispatch_delivery_dispatch_id_action_ordinal_su_fkey FOREIGN KEY (dispatch_id, action_ordinal, submit_command_id, accepted_input_id, turn_id) REFERENCES repo_watch_dispatch_delivery_intent(dispatch_id, action_ordinal, submit_command_id, accepted_input_id, turn_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_delivery_intent repo_watch_dispatch_delivery_in_dispatch_id_action_ordinal_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_delivery_intent
    ADD CONSTRAINT repo_watch_dispatch_delivery_in_dispatch_id_action_ordinal_fkey FOREIGN KEY (dispatch_id, action_ordinal) REFERENCES repo_watch_dispatch_action(dispatch_id, action_ordinal) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_delivery repo_watch_dispatch_delivery_submit_command_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_delivery
    ADD CONSTRAINT repo_watch_dispatch_delivery_submit_command_id_fkey FOREIGN KEY (submit_command_id) REFERENCES durable_command(command_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_delivery repo_watch_dispatch_delivery_turn_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_delivery
    ADD CONSTRAINT repo_watch_dispatch_delivery_turn_id_fkey FOREIGN KEY (turn_id) REFERENCES turn_lifecycle(turn_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_obligation repo_watch_dispatch_obligati_repository_rule_id_rule_vers_fkey1; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_obligation
    ADD CONSTRAINT repo_watch_dispatch_obligati_repository_rule_id_rule_vers_fkey1 FOREIGN KEY (repository, rule_id, rule_version, latest_event_id) REFERENCES repo_watch_rule_evaluation(repository, rule_id, rule_version, event_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_obligation repo_watch_dispatch_obligatio_first_repository_rule_id_rul_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_obligation
    ADD CONSTRAINT repo_watch_dispatch_obligatio_first_repository_rule_id_rul_fkey FOREIGN KEY (first_repository, rule_id, rule_version, first_event_id) REFERENCES repo_watch_rule_evaluation(repository, rule_id, rule_version, event_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_obligation repo_watch_dispatch_obligatio_repository_rule_id_rule_vers_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_obligation
    ADD CONSTRAINT repo_watch_dispatch_obligatio_repository_rule_id_rule_vers_fkey FOREIGN KEY (repository, rule_id, rule_version) REFERENCES repo_watch_rule_activation(repository, rule_id, rule_version) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_obligation repo_watch_dispatch_obligation_blocking_dispatch_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_obligation
    ADD CONSTRAINT repo_watch_dispatch_obligation_blocking_dispatch_id_fkey FOREIGN KEY (blocking_dispatch_id) REFERENCES repo_watch_dispatch_batch(dispatch_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_obligation repo_watch_dispatch_obligation_counted_dispatch_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_obligation
    ADD CONSTRAINT repo_watch_dispatch_obligation_counted_dispatch_id_fkey FOREIGN KEY (counted_dispatch_id) REFERENCES repo_watch_dispatch_batch(dispatch_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_obligation repo_watch_dispatch_obligation_external_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_obligation
    ADD CONSTRAINT repo_watch_dispatch_obligation_external_session_id_fkey FOREIGN KEY (external_blocking_session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_obligation_park repo_watch_dispatch_obligation_park_obligation_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_obligation_park
    ADD CONSTRAINT repo_watch_dispatch_obligation_park_obligation_id_fkey FOREIGN KEY (obligation_id) REFERENCES repo_watch_dispatch_obligation(obligation_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_obligation_park repo_watch_dispatch_obligation_park_release_event_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_obligation_park
    ADD CONSTRAINT repo_watch_dispatch_obligation_park_release_event_id_fkey FOREIGN KEY (release_event_id) REFERENCES repo_watch_event(event_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_obligation repo_watch_dispatch_obligation_parked_state_event_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_obligation
    ADD CONSTRAINT repo_watch_dispatch_obligation_parked_state_event_id_fkey FOREIGN KEY (parked_state_event_id) REFERENCES repo_watch_event(event_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_obligation repo_watch_dispatch_obligation_settled_dispatch_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_obligation
    ADD CONSTRAINT repo_watch_dispatch_obligation_settled_dispatch_id_fkey FOREIGN KEY (settled_dispatch_id) REFERENCES repo_watch_dispatch_batch(dispatch_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_release repo_watch_dispatch_release_dispatch_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_release
    ADD CONSTRAINT repo_watch_dispatch_release_dispatch_id_fkey FOREIGN KEY (dispatch_id) REFERENCES repo_watch_dispatch_batch(dispatch_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_start_lease_expiration repo_watch_dispatch_start_le_dispatch_id_action_ordinal_s_fkey1; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_start_lease_expiration
    ADD CONSTRAINT repo_watch_dispatch_start_le_dispatch_id_action_ordinal_s_fkey1 FOREIGN KEY (dispatch_id, action_ordinal, session_id) REFERENCES repo_watch_dispatch_start_lease(dispatch_id, action_ordinal, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_start_lease_quarantine repo_watch_dispatch_start_le_dispatch_id_action_ordinal_s_fkey2; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_start_lease_quarantine
    ADD CONSTRAINT repo_watch_dispatch_start_le_dispatch_id_action_ordinal_s_fkey2 FOREIGN KEY (dispatch_id, action_ordinal, session_id) REFERENCES repo_watch_dispatch_start_lease(dispatch_id, action_ordinal, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_start_lease repo_watch_dispatch_start_lea_dispatch_id_action_ordinal_s_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_start_lease
    ADD CONSTRAINT repo_watch_dispatch_start_lea_dispatch_id_action_ordinal_s_fkey FOREIGN KEY (dispatch_id, action_ordinal, session_id) REFERENCES repo_watch_dispatch_action(dispatch_id, action_ordinal, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_dispatch_start_lease_expiration repo_watch_dispatch_start_lease_goal_command_id_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_dispatch_start_lease_expiration
    ADD CONSTRAINT repo_watch_dispatch_start_lease_goal_command_id_session_id_fkey FOREIGN KEY (goal_command_id, session_id) REFERENCES goal_command(command_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_event repo_watch_event_repository_cursor_generation_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_event
    ADD CONSTRAINT repo_watch_event_repository_cursor_generation_fkey FOREIGN KEY (repository, cursor_generation) REFERENCES repo_watch_cursor(repository, generation) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_headless_approval_escalation repo_watch_headless_approval__request_id_turn_id_session_i_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_headless_approval_escalation
    ADD CONSTRAINT repo_watch_headless_approval__request_id_turn_id_session_i_fkey FOREIGN KEY (request_id, turn_id, session_id) REFERENCES tool_request(request_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_headless_approval_escalation repo_watch_headless_approval__session_id_terminal_frontier_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_headless_approval_escalation
    ADD CONSTRAINT repo_watch_headless_approval__session_id_terminal_frontier_fkey FOREIGN KEY (session_id, terminal_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_headless_approval_escalation repo_watch_headless_approval__terminal_attempt_id_turn_id__fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_headless_approval_escalation
    ADD CONSTRAINT repo_watch_headless_approval__terminal_attempt_id_turn_id__fkey FOREIGN KEY (terminal_attempt_id, turn_id, session_id) REFERENCES turn_attempt(turn_attempt_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_headless_approval_escalation repo_watch_headless_approval_e_session_id_failure_entry_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_headless_approval_escalation
    ADD CONSTRAINT repo_watch_headless_approval_e_session_id_failure_entry_id_fkey FOREIGN KEY (session_id, failure_entry_id) REFERENCES semantic_transcript_entry(source_session_id, semantic_entry_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_headless_approval_escalation repo_watch_headless_approval_es_dispatch_id_action_ordinal_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_headless_approval_escalation
    ADD CONSTRAINT repo_watch_headless_approval_es_dispatch_id_action_ordinal_fkey FOREIGN KEY (dispatch_id, action_ordinal) REFERENCES repo_watch_dispatch_action(dispatch_id, action_ordinal) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_headless_approval_escalation repo_watch_headless_approval_esca_model_call_id_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_headless_approval_escalation
    ADD CONSTRAINT repo_watch_headless_approval_esca_model_call_id_session_id_fkey FOREIGN KEY (model_call_id, session_id) REFERENCES tool_approval_judge_model_call(model_call_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_headless_approval_escalation repo_watch_headless_approval_escalation_turn_id_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_headless_approval_escalation
    ADD CONSTRAINT repo_watch_headless_approval_escalation_turn_id_session_id_fkey FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_lifecycle_cutoff repo_watch_lifecycle_cutoff_event_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_lifecycle_cutoff
    ADD CONSTRAINT repo_watch_lifecycle_cutoff_event_id_fkey FOREIGN KEY (event_id) REFERENCES repo_watch_event(event_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_lifecycle_cutoff_goal repo_watch_lifecycle_cutoff_goa_goal_command_id_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_lifecycle_cutoff_goal
    ADD CONSTRAINT repo_watch_lifecycle_cutoff_goa_goal_command_id_session_id_fkey FOREIGN KEY (goal_command_id, session_id) REFERENCES goal_command(command_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_lifecycle_cutoff_goal repo_watch_lifecycle_cutoff_goal_event_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_lifecycle_cutoff_goal
    ADD CONSTRAINT repo_watch_lifecycle_cutoff_goal_event_id_fkey FOREIGN KEY (event_id) REFERENCES repo_watch_lifecycle_cutoff(event_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_rule_activation repo_watch_rule_activation_repository_after_cursor_generat_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_rule_activation
    ADD CONSTRAINT repo_watch_rule_activation_repository_after_cursor_generat_fkey FOREIGN KEY (repository, after_cursor_generation, after_event_ordinal) REFERENCES repo_watch_event(repository, cursor_generation, event_ordinal) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_rule_deactivation repo_watch_rule_deactivation_repository_rule_id_rule_versi_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_rule_deactivation
    ADD CONSTRAINT repo_watch_rule_deactivation_repository_rule_id_rule_versi_fkey FOREIGN KEY (repository, rule_id, rule_version) REFERENCES repo_watch_rule_activation(repository, rule_id, rule_version) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_rule_evaluation repo_watch_rule_evaluation_dispatch_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_rule_evaluation
    ADD CONSTRAINT repo_watch_rule_evaluation_dispatch_id_fkey FOREIGN KEY (dispatch_id) REFERENCES repo_watch_dispatch_batch(dispatch_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_rule_evaluation repo_watch_rule_evaluation_repository_cursor_generation_ev_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_rule_evaluation
    ADD CONSTRAINT repo_watch_rule_evaluation_repository_cursor_generation_ev_fkey FOREIGN KEY (repository, cursor_generation, event_ordinal, event_id) REFERENCES repo_watch_event(repository, cursor_generation, event_ordinal, event_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_rule_evaluation repo_watch_rule_evaluation_repository_rule_id_rule_version_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_rule_evaluation
    ADD CONSTRAINT repo_watch_rule_evaluation_repository_rule_id_rule_version_fkey FOREIGN KEY (repository, rule_id, rule_version) REFERENCES repo_watch_rule_activation(repository, rule_id, rule_version) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_rule_field_fingerprint repo_watch_rule_field_fingerp_repository_rule_id_rule_vers_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_rule_field_fingerprint
    ADD CONSTRAINT repo_watch_rule_field_fingerp_repository_rule_id_rule_vers_fkey FOREIGN KEY (repository, rule_id, rule_version) REFERENCES repo_watch_rule_activation(repository, rule_id, rule_version) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_webhook_disposition repo_watch_webhook_disposition_hook_id_delivery_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_webhook_disposition
    ADD CONSTRAINT repo_watch_webhook_disposition_hook_id_delivery_id_fkey FOREIGN KEY (hook_id, delivery_id) REFERENCES repo_watch_webhook_delivery(hook_id, delivery_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_webhook_payload repo_watch_webhook_payload_hook_id_delivery_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_webhook_payload
    ADD CONSTRAINT repo_watch_webhook_payload_hook_id_delivery_id_fkey FOREIGN KEY (hook_id, delivery_id) REFERENCES repo_watch_webhook_delivery(hook_id, delivery_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_webhook_pending repo_watch_webhook_pending_hook_id_delivery_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_webhook_pending
    ADD CONSTRAINT repo_watch_webhook_pending_hook_id_delivery_id_fkey FOREIGN KEY (hook_id, delivery_id) REFERENCES repo_watch_webhook_delivery(hook_id, delivery_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: repo_watch_webhook_projection repo_watch_webhook_projection_hook_id_delivery_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY repo_watch_webhook_projection
    ADD CONSTRAINT repo_watch_webhook_projection_hook_id_delivery_id_fkey FOREIGN KEY (hook_id, delivery_id) REFERENCES repo_watch_webhook_delivery(hook_id, delivery_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


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
    -- the server default captured at creation time by SET search_path FROM CURRENT
    FOREACH signature IN ARRAY ARRAY[
        'adjust_repo_watch_latest_event_obligation_count(uuid, bigint)',
        'adjust_repo_watch_pull_request_work_count(text, numeric, bigint, bigint)',
        'clear_repo_watch_current_held_dispatch()',
        'decrement_repo_watch_repository_obligation_count(text)',
        'increment_repo_watch_pull_request_session_count()',
        'increment_repo_watch_repository_obligation_count(text)',
        'maintain_repo_watch_pull_request_held_count()',
        'maintain_repo_watch_pull_request_obligation_count()',
        'maintain_repo_watch_repository_held_count()',
        'maintain_repo_watch_repository_obligation_count()',
        'project_repo_watch_current_held_dispatch()',
        'project_repo_watch_singleton_cooldown()',
        'remember_repo_watch_repository_key()',
        'stamp_repo_watch_dispatch_action_target()',
        'stamp_repo_watch_dispatch_batch_target()',
        'stamp_repo_watch_webhook_projection_delivery()'
    ] LOOP
        EXECUTE format('ALTER FUNCTION %s SET search_path TO "$user", %I',
                   signature, current_schema);
    END LOOP;
    -- the canonical restore-safe pin: the migration-selected schema, then pg_catalog, then pg_temp
    FOREACH signature IN ARRAY ARRAY[
        'repo_watch_branch_is_valid(text)',
        'repo_watch_labels_are_valid(text[])',
        'repo_watch_login_is_valid(text)',
        'repo_watch_repository_is_valid(text)',
        'repo_watch_rule_id_is_valid(text)'
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
