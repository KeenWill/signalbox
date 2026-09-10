SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

ALTER TABLE repository_state
    ADD COLUMN observation_effect_id uuid UNIQUE,
    ADD COLUMN observation_effect_input bytea,
    ADD COLUMN observation_effect_result bytea,
    ADD CONSTRAINT observation_effect_receipt_shape CHECK (
        (observation_effect_id IS NULL AND observation_effect_input IS NULL AND observation_effect_result IS NULL)
        OR (observation_effect_id IS NOT NULL AND observation_effect_input IS NOT NULL AND observation_effect_result IS NOT NULL));

RESET search_path;
RESET ROLE;
