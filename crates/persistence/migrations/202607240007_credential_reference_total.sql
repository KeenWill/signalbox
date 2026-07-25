-- The pinned credential reference becomes a total column, per the 2026-07-24
-- decision-log entry and docs/spec/configuration-and-credentials.md. The
-- nullable column from 202607220002_model_call_credential_reference.sql
-- preserved forward migration of historical calls, but no database predates
-- this stack, and the store writes the reference on every Prepared insert, so
-- the NULL state is unreachable. Pre-production schema discipline states the
-- correct shape now rather than preserving phantom history; the load path's
-- NULL-as-corruption arm is removed with this change.

ALTER TABLE model_call
    ALTER COLUMN credential_reference SET NOT NULL,
    ADD CONSTRAINT model_call_credential_reference_nonempty
        CHECK (char_length(credential_reference) > 0);
