SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

CREATE TABLE workflow_effect_result (
    effect_id uuid PRIMARY KEY,
    method text NOT NULL CHECK (method IN ('repo.commitEvaluation', 'repo.submitPending')),
    effect_input bytea NOT NULL,
    effect_result bytea NOT NULL
);

RESET search_path;
RESET ROLE;
