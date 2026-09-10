CREATE TABLE evaluation_run (
    run_id uuid PRIMARY KEY REFERENCES program_run_registration(run_id),
    metadata json NOT NULL,
    scorecard_kind text NOT NULL,
    scorecard json NOT NULL,
    trial_count bigint NOT NULL CHECK (trial_count > 0),
    recording_transaction_id xid8 NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp()
);

CREATE TABLE evaluation_trial (
    run_id uuid NOT NULL REFERENCES evaluation_run(run_id),
    trial_ordinal bigint NOT NULL CHECK (trial_ordinal >= 0),
    case_position bigint NOT NULL CHECK (case_position >= 0),
    repeat_ordinal bigint NOT NULL CHECK (repeat_ordinal >= 0),
    corpus_case json NOT NULL,
    evidence_position numeric(20,0) NOT NULL,
    outcome_kind text NOT NULL CHECK (outcome_kind IN ('verdict', 'failed', 'ambiguous')),
    evidence json NOT NULL,
    PRIMARY KEY (run_id, trial_ordinal),
    FOREIGN KEY (run_id, evidence_position)
        REFERENCES program_run_journal_entry(run_id, journal_position)
);

CREATE FUNCTION stamp_evaluation_transaction() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    NEW.recording_transaction_id := pg_current_xact_id();
    RETURN NEW;
END;
$$;
CREATE TRIGGER evaluation_run_transaction
BEFORE INSERT ON evaluation_run FOR EACH ROW
EXECUTE FUNCTION stamp_evaluation_transaction();

CREATE FUNCTION admit_evaluation_trial() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM evaluation_run WHERE run_id = NEW.run_id
        AND recording_transaction_id = pg_current_xact_id()
        AND NEW.trial_ordinal < trial_count
    ) THEN
        RAISE EXCEPTION 'evaluation trial set is sealed' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER evaluation_trial_admission
BEFORE INSERT ON evaluation_trial FOR EACH ROW
EXECUTE FUNCTION admit_evaluation_trial();

CREATE FUNCTION require_complete_evaluation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF (SELECT count(*) FROM evaluation_trial WHERE run_id = NEW.run_id) <> NEW.trial_count THEN
        RAISE EXCEPTION 'evaluation snapshot is incomplete' USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER evaluation_complete
AFTER INSERT ON evaluation_run DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_complete_evaluation();

CREATE TRIGGER evaluation_run_immutable
BEFORE UPDATE OR DELETE ON evaluation_run FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER evaluation_trial_immutable
BEFORE UPDATE OR DELETE ON evaluation_trial FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER evaluation_run_no_truncate
BEFORE TRUNCATE ON evaluation_run FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER evaluation_trial_no_truncate
BEFORE TRUNCATE ON evaluation_trial FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();
