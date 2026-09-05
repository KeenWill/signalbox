-- Module tables are derived or module-local state. Each comment declares its
-- growth class and release condition; no retention duration or pruning pass is
-- selected here.

SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

-- growth: one row per command emitted by a matched rule or lifecycle reaction.
-- retention: delete a settled command after its GitHub event and rule revision are releasable.
CREATE TABLE dispatch_ledger (
    dispatch_ref uuid NOT NULL,
    action_ordinal numeric(20,0) NOT NULL,
    command_id uuid NOT NULL PRIMARY KEY,
    repository text NOT NULL,
    rule_id text NOT NULL,
    rule_revision numeric(20,0) NOT NULL,
    event_id uuid NOT NULL,
    trigger_sequence numeric(20,0),
    command_kind text NOT NULL,
    status text NOT NULL,
    rejection_kind text,
    issued_at timestamptz NOT NULL,
    settled_at timestamptz,
    UNIQUE NULLS NOT DISTINCT
        (repository, rule_id, rule_revision, event_id, trigger_sequence, action_ordinal),
    FOREIGN KEY (repository, rule_id, rule_revision)
        REFERENCES rule_revision(repository, rule_id, revision),
    FOREIGN KEY (event_id) REFERENCES gh_event(event_id),
    CHECK (command_kind = ANY (ARRAY['create_session', 'submit_input', 'goal', 'lifecycle'])),
    CHECK (action_ordinal BETWEEN 1 AND 18446744073709551615),
    CHECK (trigger_sequence IS NULL OR trigger_sequence BETWEEN 1 AND 18446744073709551615),
    CHECK (status = ANY (ARRAY['pending', 'applied', 'rejected'])),
    CHECK ((status = 'pending') = (settled_at IS NULL)),
    CHECK ((status = 'rejected') = (rejection_kind IS NOT NULL)),
    CHECK (rejection_kind IS NULL OR octet_length(rejection_kind) BETWEEN 1 AND 128)
);

RESET search_path;
RESET ROLE;
