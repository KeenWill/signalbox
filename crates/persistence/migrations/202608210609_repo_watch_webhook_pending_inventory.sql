-- Keep pending webhook work directly indexable instead of rediscovering it by
-- anti-joining the append-only delivery and disposition histories.

CREATE TABLE repo_watch_webhook_pending (
    hook_id numeric(20, 0) NOT NULL,
    delivery_id uuid NOT NULL,
    repository text NOT NULL,
    receipt_sequence bigint NOT NULL,

    PRIMARY KEY (hook_id, delivery_id),
    UNIQUE (receipt_sequence),
    FOREIGN KEY (hook_id, delivery_id)
        REFERENCES repo_watch_webhook_delivery(hook_id, delivery_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CHECK (hook_id > 0 AND hook_id <= 18446744073709551615),
    CHECK (repo_watch_repository_is_valid(repository)),
    CHECK (receipt_sequence > 0)
);

CREATE INDEX repo_watch_webhook_pending_order
    ON repo_watch_webhook_pending(repository, receipt_sequence);

INSERT INTO repo_watch_webhook_pending (
    hook_id, delivery_id, repository, receipt_sequence
)
SELECT delivery.hook_id,
       delivery.delivery_id,
       delivery.repository,
       delivery.receipt_sequence
  FROM repo_watch_webhook_delivery AS delivery
  LEFT JOIN repo_watch_webhook_disposition AS disposition
    ON disposition.hook_id = delivery.hook_id
   AND disposition.delivery_id = delivery.delivery_id
 WHERE disposition.delivery_id IS NULL;

CREATE FUNCTION guard_repo_watch_webhook_pending_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'UPDATE' THEN
        RAISE EXCEPTION 'repo_watch_webhook_pending cannot be updated'
            USING ERRCODE = '23514';
    END IF;

    IF TG_OP = 'INSERT' THEN
        IF NOT EXISTS (
            SELECT 1
              FROM repo_watch_webhook_delivery AS delivery
             WHERE delivery.hook_id = NEW.hook_id
               AND delivery.delivery_id = NEW.delivery_id
               AND delivery.repository = NEW.repository
               AND delivery.receipt_sequence = NEW.receipt_sequence
        ) OR EXISTS (
            SELECT 1
              FROM repo_watch_webhook_disposition AS disposition
             WHERE disposition.hook_id = NEW.hook_id
               AND disposition.delivery_id = NEW.delivery_id
        ) THEN
            RAISE EXCEPTION
                'repo-watch webhook pending row requires its exact undispositioned delivery'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM repo_watch_webhook_disposition AS disposition
         WHERE disposition.hook_id = OLD.hook_id
           AND disposition.delivery_id = OLD.delivery_id
    ) THEN
        RAISE EXCEPTION
            'repo-watch webhook pending row retires only with its disposition'
            USING ERRCODE = '23514';
    END IF;
    RETURN OLD;
END;
$$;

CREATE FUNCTION register_repo_watch_webhook_pending()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO repo_watch_webhook_pending (
        hook_id, delivery_id, repository, receipt_sequence
    ) VALUES (
        NEW.hook_id, NEW.delivery_id, NEW.repository, NEW.receipt_sequence
    );
    RETURN NULL;
END;
$$;

CREATE FUNCTION retire_repo_watch_webhook_pending()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    DELETE FROM repo_watch_webhook_pending
     WHERE hook_id = NEW.hook_id
       AND delivery_id = NEW.delivery_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION
            'repo-watch webhook disposition requires its pending row'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE TRIGGER repo_watch_webhook_pending_guards_mutation
BEFORE INSERT OR UPDATE OR DELETE ON repo_watch_webhook_pending
FOR EACH ROW
EXECUTE FUNCTION guard_repo_watch_webhook_pending_mutation();

CREATE TRIGGER repo_watch_webhook_delivery_registers_pending
AFTER INSERT ON repo_watch_webhook_delivery
FOR EACH ROW
EXECUTE FUNCTION register_repo_watch_webhook_pending();

CREATE TRIGGER repo_watch_webhook_disposition_retires_pending
AFTER INSERT ON repo_watch_webhook_disposition
FOR EACH ROW
EXECUTE FUNCTION retire_repo_watch_webhook_pending();

CREATE TRIGGER repo_watch_webhook_pending_reject_truncate
BEFORE TRUNCATE ON repo_watch_webhook_pending
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();
