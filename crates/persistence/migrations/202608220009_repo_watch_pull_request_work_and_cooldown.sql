-- Keep pull-request work counts and obligation cooldown reads bounded.

CREATE TABLE repo_watch_current_pull_request_work_count (
    repository text NOT NULL,
    pull_request_number numeric(20, 0) NOT NULL,
    held_count bigint NOT NULL DEFAULT 0,
    obligation_count bigint NOT NULL DEFAULT 0,

    PRIMARY KEY (repository, pull_request_number),
    CHECK (repo_watch_repository_is_valid(repository)),
    CHECK (pull_request_number > 0 AND pull_request_number <= 18446744073709551615),
    CHECK (held_count >= 0),
    CHECK (obligation_count >= 0),
    CHECK (held_count > 0 OR obligation_count > 0)
);

INSERT INTO repo_watch_current_pull_request_work_count (
    repository, pull_request_number, held_count, obligation_count
)
SELECT repository, pull_request_number, sum(held_count), sum(obligation_count)
  FROM (
        SELECT repository, pull_request_number, count(*) AS held_count,
               0::bigint AS obligation_count
          FROM repo_watch_current_held_dispatch
         WHERE pull_request_number IS NOT NULL
         GROUP BY repository, pull_request_number
        UNION ALL
        SELECT event.repository, event.pull_request_number, 0::bigint, count(*)
          FROM repo_watch_dispatch_obligation AS obligation
          JOIN repo_watch_event AS event ON event.event_id = obligation.latest_event_id
         WHERE obligation.settled_kind IS NULL
           AND event.pull_request_number IS NOT NULL
         GROUP BY event.repository, event.pull_request_number
  ) AS counts
 GROUP BY repository, pull_request_number;

CREATE FUNCTION adjust_repo_watch_pull_request_work_count(
    counted_repository text, counted_pull_request numeric,
    held_delta bigint, obligation_delta bigint
)
RETURNS void
LANGUAGE plpgsql
SET search_path FROM CURRENT
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

CREATE FUNCTION maintain_repo_watch_pull_request_held_count()
RETURNS trigger
LANGUAGE plpgsql
SET search_path FROM CURRENT
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

CREATE TRIGGER repo_watch_current_held_dispatch_maintains_pull_request_count
AFTER INSERT OR DELETE ON repo_watch_current_held_dispatch
FOR EACH ROW EXECUTE FUNCTION maintain_repo_watch_pull_request_held_count();

CREATE FUNCTION adjust_repo_watch_latest_event_obligation_count(
    counted_event_id uuid, obligation_delta bigint
)
RETURNS void
LANGUAGE plpgsql
SET search_path FROM CURRENT
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

CREATE FUNCTION maintain_repo_watch_pull_request_obligation_count()
RETURNS trigger
LANGUAGE plpgsql
SET search_path FROM CURRENT
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

CREATE TRIGGER repo_watch_dispatch_obligation_maintains_pull_request_count
AFTER INSERT OR UPDATE OF latest_event_id, settled_kind
ON repo_watch_dispatch_obligation
FOR EACH ROW EXECUTE FUNCTION maintain_repo_watch_pull_request_obligation_count();

CREATE TABLE repo_watch_current_singleton_cooldown (
    rule_id text NOT NULL,
    rule_version bigint NOT NULL,
    singleton_scope text NOT NULL,
    singleton_repository text,
    singleton_pull_request_number numeric(20, 0),
    singleton_stack_root_pull_request_number numeric(20, 0),
    eligible_at timestamptz NOT NULL,

    CHECK (repo_watch_rule_id_is_valid(rule_id)),
    CHECK (rule_version = 1)
);

CREATE UNIQUE INDEX repo_watch_current_singleton_cooldown_identity
    ON repo_watch_current_singleton_cooldown (
        rule_id, rule_version, singleton_scope, singleton_repository,
        singleton_pull_request_number, singleton_stack_root_pull_request_number
    ) NULLS NOT DISTINCT;

INSERT INTO repo_watch_current_singleton_cooldown (
    rule_id, rule_version, singleton_scope, singleton_repository,
    singleton_pull_request_number, singleton_stack_root_pull_request_number, eligible_at
)
SELECT batch.rule_id, batch.rule_version, batch.singleton_scope,
       batch.singleton_repository, batch.singleton_pull_request_number,
       batch.singleton_stack_root_pull_request_number,
       max(CASE
           WHEN batch.cooldown_seconds::numeric <= extract(epoch FROM (
               '294276-12-31 23:59:59+00'::timestamptz - release.released_at
           ))
           THEN release.released_at + batch.cooldown_seconds * interval '1 second'
           ELSE 'infinity'::timestamptz
       END)
  FROM repo_watch_dispatch_release AS release
  JOIN repo_watch_dispatch_batch AS batch USING (dispatch_id)
 GROUP BY batch.rule_id, batch.rule_version, batch.singleton_scope,
          batch.singleton_repository, batch.singleton_pull_request_number,
          batch.singleton_stack_root_pull_request_number;

CREATE FUNCTION project_repo_watch_singleton_cooldown()
RETURNS trigger
LANGUAGE plpgsql
SET search_path FROM CURRENT
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

CREATE TRIGGER repo_watch_dispatch_release_projects_singleton_cooldown
AFTER INSERT ON repo_watch_dispatch_release
FOR EACH ROW EXECUTE FUNCTION project_repo_watch_singleton_cooldown();

CREATE OR REPLACE VIEW repo_watch_outstanding_dispatch_obligation AS
SELECT obligation.obligation_id, obligation.repository, obligation.rule_id,
       obligation.rule_version, obligation.singleton_scope,
       obligation.singleton_repository, obligation.singleton_pull_request_number,
       obligation.singleton_stack_root_pull_request_number, obligation.first_repository,
       obligation.first_event_id, obligation.latest_event_id, obligation.matched_event_count,
       obligation.owed_since, obligation.latest_match_at,
       occupying.dispatch_id AS occupying_dispatch_id,
       occupying.session_ids AS occupying_session_ids, cooldown.eligible_at,
       occupying.dispatch_id IS NULL
           AND (cooldown.eligible_at IS NULL OR cooldown.eligible_at <= clock_timestamp())
           AND obligation.parked_at IS NULL
           AND obligation.failed_attempts < repo_watch_dispatch_attempt_budget() AS ready,
       obligation.failed_attempts, obligation.last_failed_attempt_at, obligation.parked_at
  FROM repo_watch_dispatch_obligation AS obligation
  LEFT JOIN LATERAL (
        SELECT held.dispatch_id,
               array_agg(action.session_id ORDER BY action.action_ordinal) AS session_ids
          FROM repo_watch_current_held_dispatch AS held
          JOIN repo_watch_dispatch_action AS action USING (dispatch_id)
         WHERE held.rule_id = obligation.rule_id
           AND held.rule_version = obligation.rule_version
           AND held.singleton_scope = obligation.singleton_scope
           AND held.singleton_repository IS NOT DISTINCT FROM obligation.singleton_repository
           AND held.singleton_pull_request_number
                IS NOT DISTINCT FROM obligation.singleton_pull_request_number
           AND held.singleton_stack_root_pull_request_number
                IS NOT DISTINCT FROM obligation.singleton_stack_root_pull_request_number
         GROUP BY held.dispatch_id, held.held_since
         ORDER BY held.held_since
         LIMIT 1
  ) AS occupying ON true
  LEFT JOIN repo_watch_current_singleton_cooldown AS cooldown
    ON cooldown.rule_id = obligation.rule_id
   AND cooldown.rule_version = obligation.rule_version
   AND cooldown.singleton_scope = obligation.singleton_scope
   AND cooldown.singleton_repository IS NOT DISTINCT FROM obligation.singleton_repository
   AND cooldown.singleton_pull_request_number
        IS NOT DISTINCT FROM obligation.singleton_pull_request_number
   AND cooldown.singleton_stack_root_pull_request_number
        IS NOT DISTINCT FROM obligation.singleton_stack_root_pull_request_number
 WHERE obligation.settled_kind IS NULL;
