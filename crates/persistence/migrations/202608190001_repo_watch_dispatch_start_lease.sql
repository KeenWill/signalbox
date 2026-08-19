-- Bound repository-watch dispatch acceptance before its first model call.
--
-- One immutable lease is admitted with every dispatched action. Durable
-- model-call evidence ends the start obligation without rewriting this audit
-- record; an immutable expiry records the normal goal stop that retired work
-- which did not start inside the production ceiling.

CREATE TABLE repo_watch_dispatch_start_lease (
    dispatch_id uuid NOT NULL,
    action_ordinal integer NOT NULL,
    session_id uuid NOT NULL,
    leased_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    expires_at timestamptz NOT NULL,

    PRIMARY KEY (dispatch_id, action_ordinal),
    UNIQUE (dispatch_id, action_ordinal, session_id),
    UNIQUE (session_id),
    FOREIGN KEY (dispatch_id, action_ordinal)
        REFERENCES repo_watch_dispatch_action(dispatch_id, action_ordinal)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    FOREIGN KEY (dispatch_id, session_id)
        REFERENCES repo_watch_dispatch_action(dispatch_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CHECK (expires_at > leased_at),
    CHECK (expires_at <= leased_at + INTERVAL '5 minutes')
);

CREATE INDEX repo_watch_dispatch_start_lease_expiry
    ON repo_watch_dispatch_start_lease (expires_at, session_id);

CREATE TABLE repo_watch_dispatch_start_lease_expiration (
    dispatch_id uuid NOT NULL,
    action_ordinal integer NOT NULL,
    session_id uuid NOT NULL,
    goal_command_id uuid NOT NULL UNIQUE,
    expired_at timestamptz NOT NULL DEFAULT clock_timestamp(),

    PRIMARY KEY (dispatch_id, action_ordinal),
    FOREIGN KEY (dispatch_id, action_ordinal, session_id)
        REFERENCES repo_watch_dispatch_start_lease(
            dispatch_id,
            action_ordinal,
            session_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    FOREIGN KEY (goal_command_id)
        REFERENCES durable_command(command_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE TRIGGER repo_watch_dispatch_start_lease_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_dispatch_start_lease
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_dispatch_start_lease_expiration_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_dispatch_start_lease_expiration
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_dispatch_start_lease_reject_truncate
BEFORE TRUNCATE ON repo_watch_dispatch_start_lease
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_dispatch_start_lease_expiration_reject_truncate
BEFORE TRUNCATE ON repo_watch_dispatch_start_lease_expiration
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();
