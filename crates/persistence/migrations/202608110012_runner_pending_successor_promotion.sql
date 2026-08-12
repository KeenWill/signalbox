-- Activate one pending runner through an atomic deployment-scoped command.

-- Supersedes the durable-command kind and storage-version constraints from
-- 202608100001_workspace_and_git_remote_authority.sql.
ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_kind_closed,
    DROP CONSTRAINT durable_command_storage_version_supported,
    ADD CONSTRAINT durable_command_kind_closed CHECK (
        command_kind IN (
            'create_session', 'create_session_from_imported_frontier',
            'replace_session_defaults', 'replace_session_metadata',
            'submit_input', 'decide_tool_request', 'review_workflow',
            'review_orchestration', 'compact_session', 'goal',
            'update_session_placement', 'register_workspace',
            'mint_git_remote', 'withdraw_git_remote',
            'promote_pending_runner'
        )
    ),
    ADD CONSTRAINT durable_command_storage_version_supported CHECK (
        (command_kind = 'create_session'
            AND storage_version IN (1, 2, 3, 4, 5, 6, 7, 8))
        OR (command_kind = 'replace_session_defaults'
            AND storage_version IN (1, 2, 3, 4))
        OR (command_kind = 'create_session_from_imported_frontier'
            AND storage_version IN (1, 2, 3, 5, 6))
        OR (command_kind = 'submit_input' AND storage_version IN (1, 2))
        OR (command_kind IN (
            'replace_session_metadata', 'decide_tool_request',
            'review_workflow', 'review_orchestration', 'compact_session',
            'goal', 'update_session_placement', 'register_workspace',
            'mint_git_remote', 'withdraw_git_remote',
            'promote_pending_runner'
        ) AND storage_version = 1)
    );

-- Supersedes the enrollment state constraints and transition guard from
-- 202608110008_runner_pending_successor_enrollment.sql. A pending enrollment
-- activates at revision two and may later revoke at revision three; an
-- initially active enrollment keeps its two-revision lifecycle.
ALTER TABLE runner_enrollment_audit
    DROP CONSTRAINT runner_enrollment_audit_state_closed,
    DROP CONSTRAINT runner_enrollment_audit_state_shape,
    ADD CONSTRAINT runner_enrollment_audit_state_closed
        CHECK (state_kind IN ('pending', 'active', 'revoked')),
    ADD CONSTRAINT runner_enrollment_audit_state_shape CHECK (
        (revision = 1 AND state_kind IN ('pending', 'active'))
        OR (revision = 2 AND state_kind IN ('active', 'revoked'))
        OR (revision = 3 AND state_kind = 'revoked')
    );

ALTER TABLE runner_enrollment
    DROP CONSTRAINT runner_enrollment_state_shape,
    ADD CONSTRAINT runner_enrollment_runner_key
        UNIQUE (enrollment_id, runner_id),
    ADD CONSTRAINT runner_enrollment_state_shape CHECK (
        (revision = 1 AND state_kind IN ('pending', 'active'))
        OR (revision = 2 AND state_kind IN ('active', 'revoked'))
        OR (revision = 3 AND state_kind = 'revoked')
    );

ALTER TABLE runner_pending_enrollment
    ADD CONSTRAINT runner_pending_enrollment_request_candidate_key
        UNIQUE (request_id, enrollment_id),
    ADD CONSTRAINT runner_pending_enrollment_request_predecessor_key
        UNIQUE (request_id, predecessor_enrollment_id);

ALTER TABLE runner_enrollment_request_receipt
    ADD CONSTRAINT runner_enrollment_request_receipt_pending_registration_key
        UNIQUE (request_id, enrollment_id, registration_revision);

CREATE OR REPLACE FUNCTION guard_runner_enrollment_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.revision <> 1
           OR NEW.state_kind NOT IN ('pending', 'active')
        THEN
            RAISE EXCEPTION
                'runner enrollment must begin pending or active at revision one'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'runner enrollment is not deletable'
            USING ERRCODE = '23514';
    END IF;
    IF ROW(
        OLD.enrollment_id,
        OLD.runner_id,
        OLD.authentication_reference_id,
        OLD.allowed_class_count
    ) IS DISTINCT FROM ROW(
        NEW.enrollment_id,
        NEW.runner_id,
        NEW.authentication_reference_id,
        NEW.allowed_class_count
    )
       OR NOT (
            (OLD.revision = 1 AND OLD.state_kind = 'active'
                AND NEW.revision = 2 AND NEW.state_kind = 'revoked')
            OR (OLD.revision = 1 AND OLD.state_kind = 'pending'
                AND NEW.revision = 2 AND NEW.state_kind = 'active')
            OR (OLD.revision = 2 AND OLD.state_kind = 'active'
                AND NEW.revision = 3 AND NEW.state_kind = 'revoked')
       )
    THEN
        RAISE EXCEPTION 'runner enrollment transition is not authorized'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TABLE promote_pending_runner_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL DEFAULT 'promote_pending_runner',
    storage_version smallint NOT NULL DEFAULT 1,
    pending_request_id uuid NOT NULL,
    result_kind text NOT NULL,
    rejection_kind text,
    result_enrollment_id uuid,
    result_runner_id uuid,
    result_registration_revision numeric(20, 0),
    result_connection_epoch numeric(20, 0),
    result_connection_event_ordinal numeric(20, 0),
    predecessor_enrollment_id uuid,
    predecessor_loss_epoch numeric(20, 0),
    active_enrollment_id uuid,
    active_runner_id uuid,
    active_connection_state text,

    CONSTRAINT promote_pending_runner_command_kind
        CHECK (command_kind = 'promote_pending_runner'),
    CONSTRAINT promote_pending_runner_command_version
        CHECK (storage_version = 1),
    CONSTRAINT promote_pending_runner_command_result_closed
        CHECK (result_kind IN ('applied', 'rejected')),
    CONSTRAINT promote_pending_runner_command_rejection_closed CHECK (
        rejection_kind IS NULL
        OR rejection_kind IN (
            'no_pending_runner_enrollment', 'pending_request_mismatch',
            'pending_request_disconnected', 'active_runner_not_lost'
        )
    ),
    CONSTRAINT promote_pending_runner_command_connection_state_closed CHECK (
        active_connection_state IS NULL
        OR active_connection_state IN ('connected', 'suspect', 'shutdown')
    ),
    CONSTRAINT promote_pending_runner_command_result_shape CHECK (
        (
            result_kind = 'applied'
            AND rejection_kind IS NULL
            AND result_enrollment_id IS NOT NULL
            AND result_runner_id IS NOT NULL
            AND result_registration_revision IS NOT NULL
            AND result_registration_revision BETWEEN 1 AND 18446744073709551615
            AND result_connection_epoch IS NOT NULL
            AND result_connection_epoch BETWEEN 1 AND 18446744073709551615
            AND result_connection_event_ordinal IS NOT NULL
            AND result_connection_event_ordinal
                BETWEEN 1 AND 18446744073709551615
            AND predecessor_enrollment_id IS NOT NULL
            AND predecessor_loss_epoch IS NOT NULL
            AND predecessor_loss_epoch BETWEEN 1 AND 18446744073709551615
            AND active_enrollment_id IS NULL
            AND active_runner_id IS NULL
            AND active_connection_state IS NULL
        )
        OR (
            result_kind = 'rejected'
            AND rejection_kind IS NOT NULL
            AND rejection_kind IN (
                'no_pending_runner_enrollment', 'pending_request_mismatch',
                'pending_request_disconnected'
            )
            AND result_enrollment_id IS NULL
            AND result_runner_id IS NULL
            AND result_registration_revision IS NULL
            AND result_connection_epoch IS NULL
            AND result_connection_event_ordinal IS NULL
            AND predecessor_enrollment_id IS NULL
            AND predecessor_loss_epoch IS NULL
            AND active_enrollment_id IS NULL
            AND active_runner_id IS NULL
            AND active_connection_state IS NULL
        )
        OR (
            result_kind = 'rejected'
            AND rejection_kind IS NOT NULL
            AND rejection_kind = 'active_runner_not_lost'
            AND result_enrollment_id IS NULL
            AND result_runner_id IS NULL
            AND result_registration_revision IS NULL
            AND result_connection_epoch IS NULL
            AND result_connection_event_ordinal IS NULL
            AND predecessor_enrollment_id IS NULL
            AND predecessor_loss_epoch IS NULL
            AND active_enrollment_id IS NOT NULL
            AND active_runner_id IS NOT NULL
            AND active_connection_state IS NOT NULL
        )
    ),
    CONSTRAINT promote_pending_runner_command_registry_fk
        FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command (command_id, command_kind, storage_version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT promote_pending_runner_command_applied_request_fk
        FOREIGN KEY (
            pending_request_id,
            result_enrollment_id,
            result_registration_revision
        )
        REFERENCES runner_enrollment_request_receipt (
            request_id,
            enrollment_id,
            registration_revision
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT promote_pending_runner_command_applied_pending_fk
        FOREIGN KEY (pending_request_id, result_enrollment_id)
        REFERENCES runner_pending_enrollment (request_id, enrollment_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT promote_pending_runner_command_applied_registration_fk
        FOREIGN KEY (
            result_enrollment_id,
            result_registration_revision,
            result_runner_id
        )
        REFERENCES runner_registration (
            enrollment_id,
            registration_revision,
            runner_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT promote_pending_runner_command_applied_connection_fk
        FOREIGN KEY (
            result_enrollment_id,
            result_connection_epoch,
            result_connection_event_ordinal
        )
        REFERENCES runner_connection_event (
            enrollment_id,
            connection_epoch,
            event_ordinal
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT promote_pending_runner_command_predecessor_pending_fk
        FOREIGN KEY (pending_request_id, predecessor_enrollment_id)
        REFERENCES runner_pending_enrollment (
            request_id,
            predecessor_enrollment_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT promote_pending_runner_command_predecessor_loss_fk
        FOREIGN KEY (predecessor_enrollment_id, predecessor_loss_epoch)
        REFERENCES runner_connection_loss_epoch (enrollment_id, loss_epoch)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT promote_pending_runner_command_active_runner_fk
        FOREIGN KEY (active_enrollment_id, active_runner_id)
        REFERENCES runner_enrollment (enrollment_id, runner_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT promote_pending_runner_command_active_predecessor_fk
        FOREIGN KEY (pending_request_id, active_enrollment_id)
        REFERENCES runner_pending_enrollment (
            request_id,
            predecessor_enrollment_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE UNIQUE INDEX promote_pending_runner_one_applied_result
    ON promote_pending_runner_command (result_enrollment_id)
    WHERE result_kind = 'applied';

CREATE FUNCTION require_promote_pending_runner_connection_authority()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    matching_authority bigint;
BEGIN
    IF NEW.result_kind = 'applied' THEN
        SELECT count(*)
          INTO matching_authority
          FROM runner_pending_enrollment AS pending
          JOIN runner_connection_authority_head AS candidate_authority
            ON candidate_authority.enrollment_id = pending.enrollment_id
          JOIN runner_connection_event AS candidate_connection
            ON candidate_connection.enrollment_id =
                   candidate_authority.enrollment_id
           AND candidate_connection.connection_epoch =
                   candidate_authority.connection_epoch
           AND candidate_connection.event_ordinal =
                   candidate_authority.connection_event_ordinal
          JOIN runner_connection_authority_head AS predecessor_authority
            ON predecessor_authority.enrollment_id =
                   pending.predecessor_enrollment_id
           AND predecessor_authority.latest_loss_epoch =
                   pending.predecessor_loss_epoch
          JOIN runner_connection_event AS predecessor_connection
            ON predecessor_connection.enrollment_id =
                   predecessor_authority.enrollment_id
           AND predecessor_connection.connection_epoch =
                   predecessor_authority.connection_epoch
           AND predecessor_connection.event_ordinal =
                   predecessor_authority.connection_event_ordinal
         WHERE pending.request_id = NEW.pending_request_id
           AND pending.enrollment_id = NEW.result_enrollment_id
           AND candidate_authority.connection_epoch =
                   NEW.result_connection_epoch
           AND candidate_authority.connection_event_ordinal =
                   NEW.result_connection_event_ordinal
           AND candidate_connection.state_kind = 'connected'
           AND pending.predecessor_enrollment_id =
                   NEW.predecessor_enrollment_id
           AND pending.predecessor_loss_epoch = NEW.predecessor_loss_epoch
           AND predecessor_connection.state_kind = 'lost';
        IF matching_authority <> 1 THEN
            RAISE EXCEPTION
                'applied runner promotion lacks exact connection authority'
                USING ERRCODE = '23514';
        END IF;
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER promote_pending_runner_connection_is_authorized
AFTER INSERT ON promote_pending_runner_command
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_promote_pending_runner_connection_authority();

CREATE TRIGGER promote_pending_runner_command_is_append_only
BEFORE UPDATE OR DELETE ON promote_pending_runner_command
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER promote_pending_runner_command_rejects_truncate
BEFORE TRUNCATE ON promote_pending_runner_command
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();

-- The immutable pending relation records the exact loss that admitted the
-- candidate. This insertion guard checks that authority at admission time;
-- later reconnection is command input, not corruption of the historical row.
CREATE FUNCTION guard_runner_pending_enrollment_insert()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM 1
          FROM runner_connection_authority_head AS authority
          JOIN runner_connection_event AS connection
            ON connection.enrollment_id = authority.enrollment_id
           AND connection.connection_epoch = authority.connection_epoch
           AND connection.event_ordinal = authority.connection_event_ordinal
         WHERE authority.enrollment_id = NEW.predecessor_enrollment_id
           AND authority.latest_loss_epoch = NEW.predecessor_loss_epoch
           AND connection.state_kind = 'lost'
           FOR SHARE OF authority;
    IF NOT FOUND THEN
        RAISE EXCEPTION
            'pending runner enrollment lacks current predecessor loss authority'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_pending_enrollment_admission_is_guarded
BEFORE INSERT ON runner_pending_enrollment
FOR EACH ROW
EXECUTE FUNCTION guard_runner_pending_enrollment_insert();

-- Supersedes assert_runner_pending_enrollment_complete and its trigger helper
-- from 202608110008_runner_pending_successor_enrollment.sql. The immutable
-- relation remains after activation, and one applied command authenticates
-- the pending-to-active state transition.
CREATE OR REPLACE FUNCTION assert_runner_pending_enrollment_complete(
    checked_enrollment uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    candidate_state text;
    relation_count bigint;
    valid_relation_count bigint;
    applied_promotion_count bigint;
BEGIN
    SELECT state_kind
      INTO candidate_state
      FROM runner_enrollment
     WHERE enrollment_id = checked_enrollment;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT count(*),
           count(*) FILTER (
               WHERE receipt.authority_kind = 'replacement_pending'
                 AND loss.loss_epoch = pending.predecessor_loss_epoch
                 AND pending_audit.state_kind = 'pending'
                 AND (
                    (candidate_state = 'pending'
                        AND predecessor.state_kind = 'active'
                        AND active_audit.enrollment_id IS NULL)
                    OR (candidate_state IN ('active', 'revoked')
                        AND predecessor.state_kind = 'revoked'
                        AND active_audit.state_kind = 'active')
                 )
           ),
           count(*) FILTER (
               WHERE promotion.result_kind = 'applied'
                 AND promotion.pending_request_id = pending.request_id
                 AND promotion.result_enrollment_id = pending.enrollment_id
                 AND promotion.result_registration_revision =
                        receipt.registration_revision
                 AND pending_audit.state_kind = 'pending'
                 AND active_audit.state_kind = 'active'
                 AND predecessor.state_kind = 'revoked'
                 AND promotion_candidate_connection.state_kind = 'connected'
                 AND promotion_predecessor_connection.state_kind = 'lost'
                 AND promotion.predecessor_enrollment_id =
                        pending.predecessor_enrollment_id
                 AND promotion.predecessor_loss_epoch =
                        pending.predecessor_loss_epoch
           )
      INTO relation_count, valid_relation_count, applied_promotion_count
      FROM runner_pending_enrollment AS pending
      JOIN runner_enrollment_request_receipt AS receipt
        ON receipt.request_id = pending.request_id
       AND receipt.enrollment_id = pending.enrollment_id
      JOIN runner_connection_loss_epoch AS loss
        ON loss.enrollment_id = pending.predecessor_enrollment_id
       AND loss.loss_epoch = pending.predecessor_loss_epoch
      JOIN runner_enrollment_audit AS pending_audit
        ON pending_audit.enrollment_id = pending.enrollment_id
       AND pending_audit.revision = 1
      LEFT JOIN runner_enrollment_audit AS active_audit
        ON active_audit.enrollment_id = pending.enrollment_id
       AND active_audit.revision = 2
      JOIN runner_enrollment AS predecessor
        ON predecessor.enrollment_id = pending.predecessor_enrollment_id
      LEFT JOIN promote_pending_runner_command AS promotion
        ON promotion.pending_request_id = pending.request_id
       AND promotion.result_enrollment_id = pending.enrollment_id
       AND promotion.result_kind = 'applied'
      LEFT JOIN runner_connection_event AS promotion_candidate_connection
        ON promotion_candidate_connection.enrollment_id =
            promotion.result_enrollment_id
       AND promotion_candidate_connection.connection_epoch =
            promotion.result_connection_epoch
       AND promotion_candidate_connection.event_ordinal =
            promotion.result_connection_event_ordinal
      LEFT JOIN runner_connection_loss_epoch AS promotion_predecessor_loss
        ON promotion_predecessor_loss.enrollment_id =
            promotion.predecessor_enrollment_id
       AND promotion_predecessor_loss.loss_epoch =
            promotion.predecessor_loss_epoch
      LEFT JOIN runner_connection_event AS promotion_predecessor_connection
        ON promotion_predecessor_connection.enrollment_id =
            promotion_predecessor_loss.enrollment_id
       AND promotion_predecessor_connection.connection_epoch =
            promotion_predecessor_loss.connection_epoch
       AND promotion_predecessor_connection.event_ordinal =
            promotion_predecessor_loss.connection_event_ordinal
     WHERE pending.enrollment_id = checked_enrollment;

    IF relation_count = 0 AND candidate_state <> 'pending' THEN
        RETURN;
    END IF;
    IF ROW(relation_count, valid_relation_count) IS DISTINCT FROM ROW(1::bigint, 1::bigint)
       OR (candidate_state = 'pending' AND applied_promotion_count <> 0)
       OR (candidate_state IN ('active', 'revoked') AND applied_promotion_count <> 1)
    THEN
        RAISE EXCEPTION
            'pending runner enrollment lacks exact promotion authority'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE OR REPLACE FUNCTION require_runner_pending_enrollment_complete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    candidate uuid;
BEGIN
    candidate := COALESCE(NEW.enrollment_id, OLD.enrollment_id);
    PERFORM assert_runner_pending_enrollment_complete(candidate);
    FOR candidate IN
        SELECT pending.enrollment_id
          FROM runner_pending_enrollment AS pending
         WHERE pending.predecessor_enrollment_id =
                COALESCE(NEW.enrollment_id, OLD.enrollment_id)
    LOOP
        PERFORM assert_runner_pending_enrollment_complete(candidate);
    END LOOP;
    RETURN NULL;
END;
$$;

CREATE FUNCTION require_promote_pending_runner_complete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.result_kind = 'applied' THEN
        PERFORM assert_runner_pending_enrollment_complete(
            NEW.result_enrollment_id
        );
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER promote_pending_runner_result_is_complete
AFTER INSERT ON promote_pending_runner_command
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_promote_pending_runner_complete();

CREATE OR REPLACE FUNCTION require_durable_command_typed_record()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE matching_records bigint;
BEGIN
    IF NEW.command_kind <> 'review_orchestration' AND EXISTS (
        SELECT 1 FROM review_orchestration_command_recovery
         WHERE command_id = NEW.command_id
    ) THEN
        RAISE EXCEPTION 'durable command % is reserved by review orchestration recovery', NEW.command_id
            USING ERRCODE = '23505';
    END IF;
    CASE NEW.command_kind
        WHEN 'create_session' THEN SELECT count(*) INTO matching_records FROM create_session_command WHERE command_id = NEW.command_id;
        WHEN 'create_session_from_imported_frontier' THEN SELECT count(*) INTO matching_records FROM create_session_from_imported_frontier_command WHERE command_id = NEW.command_id;
        WHEN 'replace_session_defaults' THEN SELECT count(*) INTO matching_records FROM replace_session_defaults_command WHERE command_id = NEW.command_id;
        WHEN 'replace_session_metadata' THEN SELECT count(*) INTO matching_records FROM replace_session_metadata_command WHERE command_id = NEW.command_id;
        WHEN 'submit_input' THEN SELECT count(*) INTO matching_records FROM submit_input_command WHERE command_id = NEW.command_id;
        WHEN 'decide_tool_request' THEN SELECT count(*) INTO matching_records FROM decide_tool_request_command WHERE command_id = NEW.command_id;
        WHEN 'review_workflow' THEN SELECT count(*) INTO matching_records FROM review_workflow_command WHERE command_id = NEW.command_id;
        WHEN 'review_orchestration' THEN SELECT (SELECT count(*) FROM review_orchestration_command WHERE command_id = NEW.command_id) + (SELECT count(*) FROM review_orchestration_command_intent WHERE command_id = NEW.command_id) INTO matching_records;
        WHEN 'compact_session' THEN SELECT count(*) INTO matching_records FROM compact_session_command WHERE command_id = NEW.command_id;
        WHEN 'goal' THEN SELECT count(*) INTO matching_records FROM goal_command WHERE command_id = NEW.command_id;
        WHEN 'update_session_placement' THEN SELECT count(*) INTO matching_records FROM update_session_placement_command WHERE command_id = NEW.command_id;
        WHEN 'register_workspace' THEN SELECT count(*) INTO matching_records FROM workspace WHERE command_id = NEW.command_id;
        WHEN 'mint_git_remote' THEN SELECT count(*) INTO matching_records FROM configured_git_remote_mint WHERE command_id = NEW.command_id;
        WHEN 'withdraw_git_remote' THEN SELECT count(*) INTO matching_records FROM configured_git_remote_withdrawal WHERE command_id = NEW.command_id;
        WHEN 'promote_pending_runner' THEN SELECT count(*) INTO matching_records FROM promote_pending_runner_command WHERE command_id = NEW.command_id;
        ELSE RAISE EXCEPTION 'unsupported durable command kind %', NEW.command_kind USING ERRCODE = '23514';
    END CASE;
    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'durable command % requires exactly one % typed record', NEW.command_id, NEW.command_kind USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;
