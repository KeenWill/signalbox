-- Bound the remaining repository-watch operator reads by maintained keys and
-- page-ordered access paths.

CREATE TABLE repo_watch_repository_key (
    repository text PRIMARY KEY,
    CHECK (repo_watch_repository_is_valid(repository))
);

INSERT INTO repo_watch_repository_key (repository)
SELECT repository FROM repo_watch_cursor
UNION
SELECT repository FROM repo_watch_webhook_delivery;

CREATE FUNCTION remember_repo_watch_repository_key()
RETURNS trigger
LANGUAGE plpgsql
SET search_path FROM CURRENT
AS $$
BEGIN
    INSERT INTO repo_watch_repository_key (repository)
    VALUES (NEW.repository)
    ON CONFLICT DO NOTHING;
    RETURN NULL;
END;
$$;

CREATE TRIGGER repo_watch_cursor_remembers_repository
AFTER INSERT ON repo_watch_cursor
FOR EACH ROW EXECUTE FUNCTION remember_repo_watch_repository_key();

CREATE TRIGGER repo_watch_webhook_delivery_remembers_repository
AFTER INSERT ON repo_watch_webhook_delivery
FOR EACH ROW EXECUTE FUNCTION remember_repo_watch_repository_key();

ALTER TABLE repo_watch_dispatch_batch
    ADD COLUMN repository text,
    ADD COLUMN pull_request_number numeric(20, 0);

DROP TRIGGER repo_watch_dispatch_batch_is_append_only
    ON repo_watch_dispatch_batch;

UPDATE repo_watch_dispatch_batch AS batch
   SET repository = event.repository,
       pull_request_number = event.pull_request_number
  FROM repo_watch_event AS event
 WHERE event.event_id = batch.event_id;

ALTER TABLE repo_watch_dispatch_batch
    ALTER COLUMN repository SET NOT NULL,
    ADD CONSTRAINT repo_watch_dispatch_batch_repository_check
        CHECK (repo_watch_repository_is_valid(repository)),
    ADD CONSTRAINT repo_watch_dispatch_batch_pull_request_number_check
        CHECK (
            pull_request_number IS NULL
            OR (pull_request_number > 0 AND pull_request_number <= 18446744073709551615)
        );

CREATE FUNCTION stamp_repo_watch_dispatch_batch_target()
RETURNS trigger
LANGUAGE plpgsql
SET search_path FROM CURRENT
AS $$
BEGIN
    SELECT event.repository, event.pull_request_number
      INTO NEW.repository, NEW.pull_request_number
      FROM repo_watch_event AS event
     WHERE event.event_id = NEW.event_id;
    RETURN NEW;
END;
$$;

CREATE TRIGGER repo_watch_dispatch_batch_stamps_target
BEFORE INSERT ON repo_watch_dispatch_batch
FOR EACH ROW EXECUTE FUNCTION stamp_repo_watch_dispatch_batch_target();

CREATE TRIGGER repo_watch_dispatch_batch_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_dispatch_batch
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE INDEX repo_watch_dispatch_batch_pull_request_admission
    ON repo_watch_dispatch_batch (
        repository, pull_request_number, admitted_at DESC, dispatch_id DESC
    )
    WHERE pull_request_number IS NOT NULL;

CREATE INDEX repo_watch_dispatch_batch_repository_admission
    ON repo_watch_dispatch_batch (repository, admitted_at DESC, dispatch_id DESC);

ALTER TABLE repo_watch_dispatch_action
    ADD COLUMN repository text,
    ADD COLUMN pull_request_number numeric(20, 0);

DROP TRIGGER repo_watch_dispatch_action_is_append_only
    ON repo_watch_dispatch_action;

UPDATE repo_watch_dispatch_action AS action
   SET repository = batch.repository,
       pull_request_number = batch.pull_request_number
  FROM repo_watch_dispatch_batch AS batch
 WHERE batch.dispatch_id = action.dispatch_id;

ALTER TABLE repo_watch_dispatch_action
    ALTER COLUMN repository SET NOT NULL,
    ADD CONSTRAINT repo_watch_dispatch_action_repository_check
        CHECK (repo_watch_repository_is_valid(repository)),
    ADD CONSTRAINT repo_watch_dispatch_action_pull_request_number_check
        CHECK (
            pull_request_number IS NULL
            OR (pull_request_number > 0 AND pull_request_number <= 18446744073709551615)
        );

CREATE FUNCTION stamp_repo_watch_dispatch_action_target()
RETURNS trigger
LANGUAGE plpgsql
SET search_path FROM CURRENT
AS $$
BEGIN
    SELECT batch.repository, batch.pull_request_number
      INTO NEW.repository, NEW.pull_request_number
      FROM repo_watch_dispatch_batch AS batch
     WHERE batch.dispatch_id = NEW.dispatch_id;
    RETURN NEW;
END;
$$;

CREATE TRIGGER repo_watch_dispatch_action_stamps_target
BEFORE INSERT ON repo_watch_dispatch_action
FOR EACH ROW EXECUTE FUNCTION stamp_repo_watch_dispatch_action_target();

CREATE TRIGGER repo_watch_dispatch_action_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_dispatch_action
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE INDEX repo_watch_dispatch_action_pull_request_recorded_at
    ON repo_watch_dispatch_action (
        repository, pull_request_number, recorded_at DESC, session_id DESC
    )
    WHERE pull_request_number IS NOT NULL;

CREATE TABLE repo_watch_current_held_dispatch (
    dispatch_id uuid PRIMARY KEY,
    repository text NOT NULL,
    pull_request_number numeric(20, 0),
    rule_id text NOT NULL,
    rule_version bigint NOT NULL,
    singleton_scope text NOT NULL,
    singleton_repository text,
    singleton_pull_request_number numeric(20, 0),
    singleton_stack_root_pull_request_number numeric(20, 0),
    held_since timestamptz NOT NULL,
    FOREIGN KEY (dispatch_id) REFERENCES repo_watch_dispatch_batch(dispatch_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CHECK (repo_watch_repository_is_valid(repository)),
    CHECK (repo_watch_rule_id_is_valid(rule_id)),
    CHECK (
        pull_request_number IS NULL
        OR (pull_request_number > 0 AND pull_request_number <= 18446744073709551615)
    )
);

INSERT INTO repo_watch_current_held_dispatch (
    dispatch_id, repository, pull_request_number, rule_id, rule_version,
    singleton_scope, singleton_repository, singleton_pull_request_number,
    singleton_stack_root_pull_request_number, held_since
)
SELECT batch.dispatch_id, batch.repository, batch.pull_request_number,
       batch.rule_id, batch.rule_version, batch.singleton_scope,
       batch.singleton_repository, batch.singleton_pull_request_number,
       batch.singleton_stack_root_pull_request_number, batch.admitted_at
  FROM repo_watch_dispatch_batch AS batch
 WHERE NOT EXISTS (
       SELECT 1 FROM repo_watch_dispatch_release AS release
        WHERE release.dispatch_id = batch.dispatch_id
 );

CREATE INDEX repo_watch_current_held_dispatch_repository_page
    ON repo_watch_current_held_dispatch (repository, held_since, dispatch_id);

CREATE INDEX repo_watch_current_held_dispatch_pull_request
    ON repo_watch_current_held_dispatch (
        repository, pull_request_number, held_since DESC, dispatch_id DESC
    )
    WHERE pull_request_number IS NOT NULL;

CREATE UNIQUE INDEX repo_watch_current_held_dispatch_singleton
    ON repo_watch_current_held_dispatch (
        rule_id, rule_version, singleton_scope, singleton_repository,
        singleton_pull_request_number, singleton_stack_root_pull_request_number
    ) NULLS NOT DISTINCT;

CREATE FUNCTION project_repo_watch_current_held_dispatch()
RETURNS trigger
LANGUAGE plpgsql
SET search_path FROM CURRENT
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

CREATE TRIGGER repo_watch_dispatch_batch_projects_current_hold
AFTER INSERT ON repo_watch_dispatch_batch
FOR EACH ROW EXECUTE FUNCTION project_repo_watch_current_held_dispatch();

CREATE FUNCTION clear_repo_watch_current_held_dispatch()
RETURNS trigger
LANGUAGE plpgsql
SET search_path FROM CURRENT
AS $$
BEGIN
    DELETE FROM repo_watch_current_held_dispatch
     WHERE dispatch_id = NEW.dispatch_id;
    RETURN NULL;
END;
$$;

CREATE TRIGGER repo_watch_dispatch_release_clears_current_hold
AFTER INSERT ON repo_watch_dispatch_release
FOR EACH ROW EXECUTE FUNCTION clear_repo_watch_current_held_dispatch();

CREATE INDEX repo_watch_dispatch_obligation_repository_page
    ON repo_watch_dispatch_obligation (repository, owed_since, obligation_id)
    WHERE settled_kind IS NULL;
