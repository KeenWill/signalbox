CREATE TABLE program_registration (
    registration_id uuid PRIMARY KEY,
    name text NOT NULL,
    revision text NOT NULL,
    source_digest bytea NOT NULL CHECK (octet_length(source_digest) = 32),
    artifact_digest bytea NOT NULL CHECK (octet_length(artifact_digest) = 32),
    artifact text NOT NULL,
    grants text[] NOT NULL CHECK (
        array_position(grants, NULL) IS NULL AND
        grants <@ ARRAY['time', 'random', 'sleep', 'subscribe', 'session', 'judge',
                        'exec-stage', 'corpus', 'eval-record', 'blob', 'register']::text[]
    ),
    UNIQUE (name, revision)
);

CREATE TABLE program_run_registration (
    run_id uuid PRIMARY KEY REFERENCES program_run_journal_stream(run_id),
    registration_id uuid NOT NULL REFERENCES program_registration(registration_id)
);

CREATE TRIGGER program_registration_is_immutable
BEFORE UPDATE OR DELETE ON program_registration
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER program_registration_cannot_be_truncated
BEFORE TRUNCATE ON program_registration
FOR EACH STATEMENT EXECUTE FUNCTION reject_program_journal_truncate();
CREATE TRIGGER program_run_registration_is_immutable
BEFORE UPDATE OR DELETE ON program_run_registration
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER program_run_registration_cannot_be_truncated
BEFORE TRUNCATE ON program_run_registration
FOR EACH STATEMENT EXECUTE FUNCTION reject_program_journal_truncate();
