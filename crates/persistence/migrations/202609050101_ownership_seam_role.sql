-- Repository-watch owns only reconstructible state in its module schema. Core
-- lifecycle reads and command writes are mediated by the ownership-seam crate,
-- so the module database identity receives no public-table privileges.

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'mod_repo_watch') THEN
        CREATE ROLE mod_repo_watch NOLOGIN NOINHERIT;
    END IF;
END
$$;

ALTER ROLE mod_repo_watch
    NOLOGIN NOINHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;

GRANT mod_repo_watch TO CURRENT_USER;

CREATE SCHEMA mod_repo_watch AUTHORIZATION mod_repo_watch;
REVOKE ALL ON SCHEMA mod_repo_watch FROM PUBLIC;
GRANT USAGE, CREATE ON SCHEMA mod_repo_watch TO mod_repo_watch;

REVOKE ALL ON ALL TABLES IN SCHEMA public FROM mod_repo_watch;
REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM mod_repo_watch;
REVOKE ALL ON ALL FUNCTIONS IN SCHEMA public FROM mod_repo_watch;

ALTER DEFAULT PRIVILEGES IN SCHEMA public
    REVOKE ALL ON TABLES FROM mod_repo_watch;
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    REVOKE ALL ON SEQUENCES FROM mod_repo_watch;
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    REVOKE ALL ON FUNCTIONS FROM mod_repo_watch;
