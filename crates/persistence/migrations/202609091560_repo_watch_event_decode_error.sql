SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

ALTER TABLE gh_event
    ADD COLUMN decode_error text,
    ADD CONSTRAINT gh_event_decode_error_nonempty CHECK (
        decode_error IS NULL OR octet_length(decode_error) > 0);

RESET search_path;
RESET ROLE;
