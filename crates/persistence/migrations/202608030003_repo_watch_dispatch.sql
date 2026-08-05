-- Durable repository-watch rule consumption, singleton admission, and dispatch audit.

CREATE FUNCTION repo_watch_rule_id_is_valid(candidate text)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
STRICT
PARALLEL SAFE
AS $$
    SELECT octet_length(candidate) BETWEEN 1 AND 128
       AND candidate COLLATE "C" ~ '^[A-Za-z0-9._-]+$'
$$;

ALTER TABLE repo_watch_event
    ADD CONSTRAINT repo_watch_event_position_identity_key
    UNIQUE (repository, cursor_generation, event_ordinal, event_id);

CREATE TABLE repo_watch_rule_activation (
    repository text NOT NULL,
    rule_id text NOT NULL,
    rule_version bigint NOT NULL,
    rule_digest bytea NOT NULL,
    after_cursor_generation bigint,
    after_event_ordinal integer,
    activated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    PRIMARY KEY (repository, rule_id, rule_version),
    CHECK (repo_watch_repository_is_valid(repository)),
    CHECK (repo_watch_rule_id_is_valid(rule_id)),
    CHECK (rule_version = 1),
    CHECK (octet_length(rule_digest) = 32),
    CHECK (
        (after_cursor_generation IS NULL AND after_event_ordinal IS NULL)
        OR (
            after_cursor_generation IS NOT NULL
            AND after_cursor_generation > 0
            AND after_event_ordinal IS NOT NULL
            AND after_event_ordinal > 0
        )
    ),
    FOREIGN KEY (repository, after_cursor_generation, after_event_ordinal)
        REFERENCES repo_watch_event(repository, cursor_generation, event_ordinal)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE TABLE repo_watch_rule_deactivation (
    repository text NOT NULL,
    rule_id text NOT NULL,
    rule_version bigint NOT NULL,
    deactivated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    PRIMARY KEY (repository, rule_id, rule_version),
    FOREIGN KEY (repository, rule_id, rule_version)
        REFERENCES repo_watch_rule_activation(repository, rule_id, rule_version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE TABLE repo_watch_dispatch_batch (
    dispatch_id uuid PRIMARY KEY,
    event_id uuid NOT NULL,
    rule_id text NOT NULL,
    rule_version bigint NOT NULL,
    singleton_scope text NOT NULL,
    singleton_repository text,
    singleton_pull_request_number numeric(20, 0),
    singleton_stack_root_pull_request_number numeric(20, 0),
    cooldown_seconds bigint NOT NULL,
    action_count integer NOT NULL,
    admitted_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    UNIQUE (event_id, rule_id, rule_version),
    UNIQUE (dispatch_id, event_id),
    FOREIGN KEY (event_id)
        REFERENCES repo_watch_event(event_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
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
    CHECK (cooldown_seconds >= 0),
    CHECK (action_count BETWEEN 1 AND 32)
);

CREATE TABLE repo_watch_dispatch_action (
    dispatch_id uuid NOT NULL,
    action_ordinal integer NOT NULL,
    event_id uuid NOT NULL,
    session_id uuid NOT NULL,
    create_command_id uuid NOT NULL UNIQUE,
    template_name text NOT NULL,
    template_content_digest bytea NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    PRIMARY KEY (dispatch_id, action_ordinal),
    UNIQUE (dispatch_id, session_id),
    FOREIGN KEY (dispatch_id, event_id)
        REFERENCES repo_watch_dispatch_batch(dispatch_id, event_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    FOREIGN KEY (session_id)
        REFERENCES session(session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    FOREIGN KEY (create_command_id)
        REFERENCES durable_command(command_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CHECK (action_ordinal > 0),
    CHECK (octet_length(template_name) BETWEEN 1 AND 128),
    CHECK (octet_length(template_content_digest) = 32)
);

CREATE INDEX repo_watch_dispatch_action_session
    ON repo_watch_dispatch_action (session_id);

CREATE TABLE repo_watch_dispatch_delivery_intent (
    dispatch_id uuid NOT NULL,
    action_ordinal integer NOT NULL,
    submit_command_id uuid NOT NULL UNIQUE,
    accepted_input_id uuid NOT NULL UNIQUE,
    turn_id uuid NOT NULL UNIQUE,
    cancellation_entry_id uuid NOT NULL UNIQUE,
    cancellation_frontier_id uuid NOT NULL UNIQUE,
    prepared_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    PRIMARY KEY (dispatch_id, action_ordinal),
    UNIQUE (
        dispatch_id,
        action_ordinal,
        submit_command_id,
        accepted_input_id,
        turn_id
    ),
    FOREIGN KEY (dispatch_id, action_ordinal)
        REFERENCES repo_watch_dispatch_action(dispatch_id, action_ordinal)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE TABLE repo_watch_dispatch_delivery (
    dispatch_id uuid NOT NULL,
    action_ordinal integer NOT NULL,
    submit_command_id uuid NOT NULL UNIQUE,
    accepted_input_id uuid NOT NULL UNIQUE,
    turn_id uuid NOT NULL UNIQUE,
    delivered_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    PRIMARY KEY (dispatch_id, action_ordinal),
    FOREIGN KEY (
        dispatch_id,
        action_ordinal,
        submit_command_id,
        accepted_input_id,
        turn_id
    ) REFERENCES repo_watch_dispatch_delivery_intent(
        dispatch_id,
        action_ordinal,
        submit_command_id,
        accepted_input_id,
        turn_id
    )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    FOREIGN KEY (submit_command_id)
        REFERENCES durable_command(command_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    FOREIGN KEY (accepted_input_id)
        REFERENCES accepted_input(accepted_input_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    FOREIGN KEY (turn_id)
        REFERENCES turn_lifecycle(turn_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE TABLE repo_watch_dispatch_release (
    dispatch_id uuid PRIMARY KEY,
    released_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    FOREIGN KEY (dispatch_id)
        REFERENCES repo_watch_dispatch_batch(dispatch_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE FUNCTION repo_watch_release_completed_dispatch_batches_for_turn(
    completed_turn_id uuid,
    completed_session_id uuid
)
RETURNS void
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

CREATE FUNCTION repo_watch_release_completed_dispatch_batches()
RETURNS trigger
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

CREATE TRIGGER repo_watch_dispatch_release_on_terminal_turn
AFTER UPDATE OF state_kind ON turn_lifecycle
FOR EACH ROW
EXECUTE FUNCTION repo_watch_release_completed_dispatch_batches();

CREATE FUNCTION repo_watch_release_completed_dispatch_batches_for_goal()
RETURNS trigger
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

CREATE TRIGGER repo_watch_dispatch_release_on_terminal_goal
AFTER INSERT ON goal_event
FOR EACH ROW
EXECUTE FUNCTION repo_watch_release_completed_dispatch_batches_for_goal();

CREATE TABLE repo_watch_rule_evaluation (
    repository text NOT NULL,
    rule_id text NOT NULL,
    rule_version bigint NOT NULL,
    event_id uuid NOT NULL,
    cursor_generation bigint NOT NULL,
    event_ordinal integer NOT NULL,
    outcome_kind text NOT NULL,
    dispatch_id uuid,
    evaluated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    PRIMARY KEY (repository, rule_id, rule_version, event_id),
    UNIQUE (repository, rule_id, rule_version, cursor_generation, event_ordinal),
    FOREIGN KEY (repository, rule_id, rule_version)
        REFERENCES repo_watch_rule_activation(repository, rule_id, rule_version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    FOREIGN KEY (repository, cursor_generation, event_ordinal, event_id)
        REFERENCES repo_watch_event(repository, cursor_generation, event_ordinal, event_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    FOREIGN KEY (dispatch_id)
        REFERENCES repo_watch_dispatch_batch(dispatch_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CHECK (repo_watch_repository_is_valid(repository)),
    CHECK (repo_watch_rule_id_is_valid(rule_id)),
    CHECK (rule_version = 1),
    CHECK (cursor_generation > 0),
    CHECK (event_ordinal > 0),
    CHECK (outcome_kind IN ('not_matched', 'occupied', 'cooldown', 'dispatched')),
    CHECK ((dispatch_id IS NOT NULL) = (outcome_kind = 'dispatched'))
);

CREATE INDEX repo_watch_dispatch_singleton_lookup
    ON repo_watch_dispatch_batch (
        rule_id,
        rule_version,
        singleton_scope,
        singleton_repository,
        singleton_pull_request_number,
        singleton_stack_root_pull_request_number,
        admitted_at DESC
    );

CREATE INDEX repo_watch_rule_evaluation_cursor
    ON repo_watch_rule_evaluation (
        repository,
        rule_id,
        rule_version,
        cursor_generation DESC,
        event_ordinal DESC
    );

CREATE FUNCTION repo_watch_dispatch_batch_has_complete_actions()
RETURNS trigger
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

CREATE CONSTRAINT TRIGGER repo_watch_dispatch_batch_requires_complete_actions
AFTER INSERT ON repo_watch_dispatch_batch
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION repo_watch_dispatch_batch_has_complete_actions();

CREATE CONSTRAINT TRIGGER repo_watch_dispatch_action_completes_batch
AFTER INSERT OR UPDATE OR DELETE ON repo_watch_dispatch_action
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION repo_watch_dispatch_batch_has_complete_actions();

CREATE TRIGGER repo_watch_rule_activation_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_rule_activation
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_rule_deactivation_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_rule_deactivation
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_dispatch_batch_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_dispatch_batch
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_dispatch_action_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_dispatch_action
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_dispatch_delivery_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_dispatch_delivery
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_dispatch_delivery_intent_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_dispatch_delivery_intent
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_dispatch_release_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_dispatch_release
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_rule_evaluation_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_rule_evaluation
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_rule_activation_reject_truncate
BEFORE TRUNCATE ON repo_watch_rule_activation
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_rule_deactivation_reject_truncate
BEFORE TRUNCATE ON repo_watch_rule_deactivation
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_dispatch_batch_reject_truncate
BEFORE TRUNCATE ON repo_watch_dispatch_batch
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_dispatch_action_reject_truncate
BEFORE TRUNCATE ON repo_watch_dispatch_action
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_dispatch_delivery_reject_truncate
BEFORE TRUNCATE ON repo_watch_dispatch_delivery
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_dispatch_delivery_intent_reject_truncate
BEFORE TRUNCATE ON repo_watch_dispatch_delivery_intent
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_dispatch_release_reject_truncate
BEFORE TRUNCATE ON repo_watch_dispatch_release
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_rule_evaluation_reject_truncate
BEFORE TRUNCATE ON repo_watch_rule_evaluation
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();
