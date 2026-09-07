SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

-- growth: one immutable receipt per successful reload command.
-- retention: retained for command replay.
CREATE TABLE reload_activation (
    command_id uuid PRIMARY KEY CHECK (command_id NOT IN ('00000000-0000-0000-0000-000000000000', 'ffffffff-ffff-ffff-ffff-ffffffffffff')),
    rule_set_digest bytea NOT NULL CHECK (octet_length(rule_set_digest) = 32),
    activation_tails jsonb NOT NULL CHECK (jsonb_typeof(activation_tails) = 'object')
);

CREATE FUNCTION reject_reload_activation_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'reload activation records are immutable';
END;
$$;
CREATE TRIGGER reload_activation_is_immutable
    BEFORE UPDATE OR DELETE ON reload_activation
    FOR EACH ROW EXECUTE FUNCTION reject_reload_activation_change();
RESET ROLE;
SET search_path = public;
