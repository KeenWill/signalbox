-- Admit positive rule revisions and retain field fingerprints for exact
-- configuration diagnostics without storing configured rule contents.

-- Supersedes the single-revision repo_watch_rule_activation_rule_version_check
-- defined by 202608030004_repo_watch_dispatch.sql.
ALTER TABLE repo_watch_rule_activation
    DROP CONSTRAINT repo_watch_rule_activation_rule_version_check,
    ADD CONSTRAINT repo_watch_rule_activation_rule_version_check
        CHECK (rule_version > 0);

CREATE TABLE repo_watch_rule_field_fingerprint (
    repository text NOT NULL,
    rule_id text NOT NULL,
    rule_version bigint NOT NULL,
    rule_field_digests bytea NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    PRIMARY KEY (repository, rule_id, rule_version),
    FOREIGN KEY (repository, rule_id, rule_version)
        REFERENCES repo_watch_rule_activation(repository, rule_id, rule_version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CHECK (repo_watch_repository_is_valid(repository)),
    CHECK (repo_watch_rule_id_is_valid(rule_id)),
    CHECK (rule_version > 0),
    CHECK (octet_length(rule_field_digests) = 512)
);

CREATE TRIGGER repo_watch_rule_field_fingerprint_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_rule_field_fingerprint
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_rule_field_fingerprint_reject_truncate
BEFORE TRUNCATE ON repo_watch_rule_field_fingerprint
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();

-- Supersedes the single-revision repo_watch_dispatch_batch_rule_version_check
-- defined by 202608030004_repo_watch_dispatch.sql.
ALTER TABLE repo_watch_dispatch_batch
    DROP CONSTRAINT repo_watch_dispatch_batch_rule_version_check,
    ADD CONSTRAINT repo_watch_dispatch_batch_rule_version_check
        CHECK (rule_version > 0);

-- Supersedes the single-revision repo_watch_rule_evaluation_rule_version_check
-- defined by 202608030004_repo_watch_dispatch.sql.
ALTER TABLE repo_watch_rule_evaluation
    DROP CONSTRAINT repo_watch_rule_evaluation_rule_version_check,
    ADD CONSTRAINT repo_watch_rule_evaluation_rule_version_check
        CHECK (rule_version > 0);

-- Supersedes the single-revision repo_watch_dispatch_obligation_rule_version_check
-- defined by 202608140003_repo_watch_dispatch_obligation.sql.
ALTER TABLE repo_watch_dispatch_obligation
    DROP CONSTRAINT repo_watch_dispatch_obligation_rule_version_check,
    ADD CONSTRAINT repo_watch_dispatch_obligation_rule_version_check
        CHECK (rule_version > 0);

-- The per-field digests of an activation recorded before this migration cannot
-- be reconstructed from its aggregate rule digest, so this change invalidates
-- every such activation. Retire them once here rather than carrying a
-- fingerprint-absent shape in the daemon.
--
-- OPERATOR ACTION REQUIRED ON THE FIRST BOOT AFTER THIS MIGRATION: increment
-- `version` once for every configured repository-watch rule, including every
-- rule whose semantics did not change. Retiring an activation retires its
-- (rule ID, revision) pair, so reconciliation refuses the unchanged pair as
-- identity reuse and the daemon fails in its Configuration phase, before
-- either local socket binds, reporting that field `version` reuses a retired
-- value and must be incremented to a higher revision. Each bumped revision
-- activates after the current event tail and stays joined to the retired
-- revision's evaluations, dispatches, and sessions by its rule ID.
INSERT INTO repo_watch_rule_deactivation (repository, rule_id, rule_version)
SELECT activation.repository, activation.rule_id, activation.rule_version
  FROM repo_watch_rule_activation AS activation
 WHERE NOT EXISTS (
       SELECT 1
         FROM repo_watch_rule_deactivation AS deactivation
        WHERE deactivation.repository = activation.repository
          AND deactivation.rule_id = activation.rule_id
          AND deactivation.rule_version = activation.rule_version
 );
