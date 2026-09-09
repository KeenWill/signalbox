SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

-- growth: one pending observation per pull request named by a primary webhook.
-- retention: removed after its observation commits.
CREATE TABLE webhook_pull_wake (
    repository text NOT NULL,
    pull_request_number numeric(20,0) NOT NULL,
    delivery_id uuid NOT NULL,
    PRIMARY KEY (repository, pull_request_number),
    CHECK (pull_request_number BETWEEN 1 AND 18446744073709551615)
);

RESET search_path;
RESET ROLE;
