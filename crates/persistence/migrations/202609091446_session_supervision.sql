-- Operator evidence is independent of the lifecycle projection it can report as corrupt.
CREATE TABLE session_supervision (
    session_id uuid PRIMARY KEY REFERENCES session(session_id),
    supervision_id uuid NOT NULL,
    supervision_failure_class text NOT NULL CHECK (supervision_failure_class IN (
        'infrastructure', 'commit_ambiguous', 'corruption', 'identity_collision', 'bug'
    )),
    supervision_cause_code text NOT NULL,
    supervision_pending boolean NOT NULL
);
