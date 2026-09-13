ALTER TABLE session_title_model_call
    ADD COLUMN abandoned boolean NOT NULL DEFAULT false,
    ADD CONSTRAINT session_title_abandoned_terminal CHECK (
        NOT abandoned OR (state_kind = 'terminal' AND title IS NULL)
    );

DROP INDEX session_title_initial_call;
CREATE UNIQUE INDEX session_title_initial_call ON session_title_model_call(session_id)
    WHERE initial_for_turn IS NOT NULL AND NOT abandoned;
