-- Retain one follow-up delivery after every released repository-watch dispatch
-- until the pull request is closed or its exact head is convergence-sealed.

-- An evaluated source fact still has one rule evaluation, but its retained
-- obligation is a distinct delivery identity and may create a later batch.
ALTER TABLE repo_watch_dispatch_batch
    DROP CONSTRAINT IF EXISTS repo_watch_dispatch_batch_event_id_rule_id_rule_version_key;

CREATE FUNCTION repo_watch_retain_released_dispatch_obligation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO repo_watch_dispatch_obligation (
        obligation_id,
        repository,
        rule_id,
        rule_version,
        singleton_scope,
        singleton_repository,
        singleton_pull_request_number,
        singleton_stack_root_pull_request_number,
        first_repository,
        first_event_id,
        latest_event_id,
        matched_event_count,
        blocking_dispatch_id,
        owed_since,
        latest_match_at
    )
    SELECT batch.dispatch_id,
           origin.repository,
           batch.rule_id,
           batch.rule_version,
           batch.singleton_scope,
           batch.singleton_repository,
           batch.singleton_pull_request_number,
           batch.singleton_stack_root_pull_request_number,
           origin.repository,
           batch.event_id,
           batch.event_id,
           1,
           batch.dispatch_id,
           NEW.released_at,
           NEW.released_at
      FROM repo_watch_dispatch_batch AS batch
      JOIN repo_watch_event AS origin
        ON origin.event_id = batch.event_id
     WHERE batch.dispatch_id = NEW.dispatch_id
       AND origin.pull_request_number IS NOT NULL
       AND NOT EXISTS (
            SELECT 1
              FROM repo_watch_rule_deactivation AS deactivation
             WHERE deactivation.repository = origin.repository
               AND deactivation.rule_id = batch.rule_id
               AND deactivation.rule_version = batch.rule_version
       )
    ON CONFLICT DO NOTHING;
    RETURN NULL;
END;
$$;

CREATE TRIGGER repo_watch_dispatch_release_retains_obligation
AFTER INSERT ON repo_watch_dispatch_release
FOR EACH ROW
EXECUTE FUNCTION repo_watch_retain_released_dispatch_obligation();
