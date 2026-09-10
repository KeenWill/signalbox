SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

ALTER TABLE rule_evaluation_cursor
    ADD COLUMN effect_id uuid UNIQUE,
    ADD COLUMN effect_input bytea,
    ADD COLUMN effect_result bytea,
    ADD CONSTRAINT evaluation_effect_receipt_shape CHECK (
        (effect_id IS NULL AND effect_input IS NULL AND effect_result IS NULL)
        OR (effect_id IS NOT NULL AND effect_input IS NOT NULL AND effect_result IS NOT NULL));

ALTER TABLE dispatch_ledger
    ADD COLUMN effect_id uuid UNIQUE,
    ADD COLUMN effect_input bytea,
    ADD COLUMN effect_result bytea,
    ADD CONSTRAINT submission_effect_receipt_shape CHECK (
        (effect_id IS NULL AND effect_input IS NULL AND effect_result IS NULL)
        OR (effect_id IS NOT NULL AND effect_input IS NOT NULL));

RESET search_path;
RESET ROLE;

ALTER TABLE program_registration DROP CONSTRAINT program_registration_grants_check;
ALTER TABLE program_registration ADD CONSTRAINT program_registration_grants_check CHECK (
    array_position(grants, NULL) IS NULL AND
    grants <@ ARRAY['time', 'random', 'sleep', 'subscribe', 'session', 'judge',
        'exec-stage', 'corpus', 'eval-record', 'blob', 'register', 'repo-watch']::text[]);

DO $$
DECLARE constraint_record record;
BEGIN
    FOR constraint_record IN
        SELECT conrelid::regclass AS relation, conname, pg_get_constraintdef(oid) AS definition
        FROM pg_constraint
        WHERE conname IN ('program_run_journal_entry_effect_shape',
            'program_run_journal_nondeterminism_expected_effect_shape',
            'program_run_journal_nondeterminism_observed_effect_shape')
    LOOP
        EXECUTE format('ALTER TABLE %s DROP CONSTRAINT %I', constraint_record.relation, constraint_record.conname);
        EXECUTE format('ALTER TABLE %s ADD CONSTRAINT %I %s', constraint_record.relation, constraint_record.conname,
            replace(constraint_record.definition, '''register''::text', '''register''::text, ''repo-watch''::text'));
    END LOOP;
END $$;
