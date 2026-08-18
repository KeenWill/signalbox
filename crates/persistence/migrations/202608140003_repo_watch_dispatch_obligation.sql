ALTER TABLE repo_watch_rule_evaluation
    DROP CONSTRAINT repo_watch_rule_evaluation_outcome_kind_check;

ALTER TABLE repo_watch_rule_evaluation
    ADD CONSTRAINT repo_watch_rule_evaluation_outcome_kind_check
    CHECK (outcome_kind IN (
        'not_matched', 'occupied', 'coalesced', 'cooldown', 'dispatched'
    ));

CREATE TABLE repo_watch_dispatch_obligation (
    obligation_id uuid PRIMARY KEY,
    repository text NOT NULL,
    rule_id text NOT NULL,
    rule_version bigint NOT NULL,
    singleton_scope text NOT NULL,
    singleton_repository text,
    singleton_pull_request_number numeric(20, 0),
    singleton_stack_root_pull_request_number numeric(20, 0),
    first_repository text NOT NULL,
    first_event_id uuid NOT NULL,
    latest_event_id uuid NOT NULL,
    matched_event_count bigint NOT NULL,
    blocking_dispatch_id uuid NOT NULL,
    owed_since timestamptz NOT NULL DEFAULT transaction_timestamp(),
    latest_match_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    settled_kind text,
    settled_dispatch_id uuid UNIQUE,
    settled_at timestamptz,

    UNIQUE (obligation_id, latest_event_id),
    FOREIGN KEY (repository, rule_id, rule_version)
        REFERENCES repo_watch_rule_activation(repository, rule_id, rule_version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    FOREIGN KEY (first_repository, rule_id, rule_version, first_event_id)
        REFERENCES repo_watch_rule_evaluation(repository, rule_id, rule_version, event_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    FOREIGN KEY (repository, rule_id, rule_version, latest_event_id)
        REFERENCES repo_watch_rule_evaluation(repository, rule_id, rule_version, event_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    FOREIGN KEY (blocking_dispatch_id)
        REFERENCES repo_watch_dispatch_batch(dispatch_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    FOREIGN KEY (settled_dispatch_id)
        REFERENCES repo_watch_dispatch_batch(dispatch_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CHECK (repo_watch_repository_is_valid(repository)),
    CHECK (repo_watch_repository_is_valid(first_repository)),
    CHECK (repo_watch_rule_id_is_valid(rule_id)),
    CHECK (rule_version = 1),
    CHECK (singleton_scope IN ('pull_request', 'stack', 'rule', 'repo')),
    CHECK (singleton_repository IS NULL OR repo_watch_repository_is_valid(singleton_repository)),
    CHECK (
        singleton_pull_request_number IS NULL
        OR (
            singleton_pull_request_number > 0
            AND singleton_pull_request_number <= 18446744073709551615
        )
    ),
    CHECK (
        singleton_stack_root_pull_request_number IS NULL
        OR (
            singleton_stack_root_pull_request_number > 0
            AND singleton_stack_root_pull_request_number <= 18446744073709551615
        )
    ),
    CHECK (
        (singleton_scope = 'pull_request'
            AND singleton_repository IS NOT NULL
            AND singleton_pull_request_number IS NOT NULL
            AND singleton_stack_root_pull_request_number IS NULL)
        OR (singleton_scope = 'stack'
            AND singleton_repository IS NOT NULL
            AND singleton_pull_request_number IS NULL
            AND singleton_stack_root_pull_request_number IS NOT NULL)
        OR (singleton_scope = 'rule'
            AND singleton_repository IS NULL
            AND singleton_pull_request_number IS NULL
            AND singleton_stack_root_pull_request_number IS NULL)
        OR (singleton_scope = 'repo'
            AND singleton_repository IS NOT NULL
            AND singleton_pull_request_number IS NULL
            AND singleton_stack_root_pull_request_number IS NULL)
    ),
    CHECK (matched_event_count > 0),
    CHECK (settled_kind IS NULL OR settled_kind IN ('dispatched', 'deactivated')),
    CHECK (
        (settled_kind IS NULL
            AND settled_dispatch_id IS NULL
            AND settled_at IS NULL)
        OR (settled_kind = 'dispatched'
            AND settled_dispatch_id IS NOT NULL
            AND settled_at IS NOT NULL)
        OR (settled_kind = 'deactivated'
            AND settled_dispatch_id IS NULL
            AND settled_at IS NOT NULL)
    )
);

CREATE UNIQUE INDEX repo_watch_dispatch_obligation_active_singleton
    ON repo_watch_dispatch_obligation (
        rule_id,
        rule_version,
        singleton_scope,
        singleton_repository,
        singleton_pull_request_number,
        singleton_stack_root_pull_request_number
    ) NULLS NOT DISTINCT
    WHERE settled_kind IS NULL;

CREATE INDEX repo_watch_dispatch_obligation_ready_order
    ON repo_watch_dispatch_obligation (repository, rule_id, rule_version, owed_since)
    WHERE settled_kind IS NULL;

CREATE FUNCTION repo_watch_settle_deactivated_dispatch_obligations()
RETURNS trigger
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

CREATE TRIGGER repo_watch_dispatch_obligation_settles_on_deactivation
AFTER INSERT ON repo_watch_rule_deactivation
FOR EACH ROW
EXECUTE FUNCTION repo_watch_settle_deactivated_dispatch_obligations();

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
       occupying.session_ids AS occupying_session_ids,
       cooldown.eligible_at,
       occupying.dispatch_id IS NULL
           AND (cooldown.eligible_at IS NULL OR cooldown.eligible_at <= clock_timestamp())
           AS ready
  FROM repo_watch_dispatch_obligation AS obligation
  LEFT JOIN LATERAL (
        SELECT batch.dispatch_id,
               array_agg(action.session_id ORDER BY action.action_ordinal) AS session_ids
          FROM repo_watch_dispatch_batch AS batch
          JOIN repo_watch_dispatch_action AS action
            ON action.dispatch_id = batch.dispatch_id
         WHERE batch.rule_id = obligation.rule_id
           AND batch.rule_version = obligation.rule_version
           AND batch.singleton_scope = obligation.singleton_scope
           AND batch.singleton_repository
                IS NOT DISTINCT FROM obligation.singleton_repository
           AND batch.singleton_pull_request_number
                IS NOT DISTINCT FROM obligation.singleton_pull_request_number
           AND batch.singleton_stack_root_pull_request_number
                IS NOT DISTINCT FROM obligation.singleton_stack_root_pull_request_number
           AND NOT EXISTS (
                SELECT 1
                  FROM repo_watch_dispatch_release AS released
                 WHERE released.dispatch_id = batch.dispatch_id
           )
         GROUP BY batch.dispatch_id, batch.admitted_at
         ORDER BY batch.admitted_at
         LIMIT 1
  ) AS occupying ON true
  LEFT JOIN LATERAL (
        SELECT max(CASE
            WHEN batch.cooldown_seconds::numeric <= extract(epoch FROM (
                '294276-12-31 23:59:59+00'::timestamptz - released.released_at
            ))
            THEN released.released_at
                + batch.cooldown_seconds * interval '1 second'
            ELSE 'infinity'::timestamptz
        END) AS eligible_at
          FROM repo_watch_dispatch_release AS released
          JOIN repo_watch_dispatch_batch AS batch
            ON batch.dispatch_id = released.dispatch_id
         WHERE batch.rule_id = obligation.rule_id
           AND batch.rule_version = obligation.rule_version
           AND batch.singleton_scope = obligation.singleton_scope
           AND batch.singleton_repository
                IS NOT DISTINCT FROM obligation.singleton_repository
           AND batch.singleton_pull_request_number
                IS NOT DISTINCT FROM obligation.singleton_pull_request_number
           AND batch.singleton_stack_root_pull_request_number
                IS NOT DISTINCT FROM obligation.singleton_stack_root_pull_request_number
  ) AS cooldown ON true
 WHERE obligation.settled_kind IS NULL;

CREATE TRIGGER repo_watch_dispatch_obligation_reject_delete
BEFORE DELETE ON repo_watch_dispatch_obligation
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_dispatch_obligation_reject_truncate
BEFORE TRUNCATE ON repo_watch_dispatch_obligation
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();
