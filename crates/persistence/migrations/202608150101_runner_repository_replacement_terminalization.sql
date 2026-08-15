-- Retain the immutable semantic boundary reserved when a repository-backed
-- replacement workspace becomes ready. Receipt consumption and the terminal
-- replacement transaction use these exact identities after any model
-- observation already in flight reaches its durable boundary.

ALTER TABLE runner_replacement_workspace_receipt
    ADD COLUMN boundary_entry_id uuid NOT NULL,
    ADD COLUMN boundary_frontier_id uuid NOT NULL,
    ADD CONSTRAINT runner_replacement_workspace_receipt_boundary_distinct CHECK (
        boundary_entry_id <> boundary_frontier_id
    ),
    ADD CONSTRAINT runner_replacement_workspace_receipt_boundary_entry_unique
        UNIQUE (session_id, boundary_entry_id),
    ADD CONSTRAINT runner_replacement_workspace_receipt_boundary_frontier_unique
        UNIQUE (session_id, boundary_frontier_id);

CREATE FUNCTION require_runner_replacement_workspace_receipt_boundary_fresh()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM semantic_transcript_entry AS entry
         WHERE entry.source_session_id = NEW.session_id
           AND entry.semantic_entry_id = NEW.boundary_entry_id
    ) OR EXISTS (
        SELECT 1
          FROM context_frontier AS frontier
         WHERE frontier.owning_session_id = NEW.session_id
           AND frontier.context_frontier_id = NEW.boundary_frontier_id
    ) THEN
        RAISE EXCEPTION 'repository replacement boundary identity already exists'
            USING ERRCODE = '23505';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_replacement_workspace_receipt_boundary_is_fresh
AFTER INSERT ON runner_replacement_workspace_receipt
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_replacement_workspace_receipt_boundary_fresh();

ALTER TABLE replace_lost_runner_result
    ADD COLUMN repository_authorization_id uuid,
    ADD CONSTRAINT replace_lost_runner_result_repository_authorization_shape CHECK (
        repository_authorization_id IS NULL
        OR (result_kind = 'applied' AND placement_state_kind = 'pinned')
    ),
    ADD CONSTRAINT replace_lost_runner_result_repository_authorization_fk
        FOREIGN KEY (repository_authorization_id, session_id)
        REFERENCES runner_workspace_provisioning_authorization (
            authorization_id, session_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

ALTER FUNCTION require_pinned_replace_lost_runner_result_authority()
    RENAME TO require_workspace_free_pinned_replacement_result_authority;

DROP TRIGGER pinned_replace_lost_runner_result_is_authorized
    ON replace_lost_runner_result;

CREATE CONSTRAINT TRIGGER workspace_free_pinned_replacement_result_is_authorized
AFTER INSERT ON replace_lost_runner_result
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
WHEN (
    NEW.result_kind = 'applied'
    AND NEW.placement_state_kind = 'pinned'
    AND NEW.repository_authorization_id IS NULL
)
EXECUTE FUNCTION require_workspace_free_pinned_replacement_result_authority();

CREATE FUNCTION require_repository_pinned_replacement_result_authority()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    request replace_lost_runner_command%ROWTYPE;
    staged runner_workspace_provisioning_authorization%ROWTYPE;
    receipt runner_replacement_workspace_receipt%ROWTYPE;
    lost runner_session_placement_record%ROWTYPE;
    successor runner_session_placement_record%ROWTYPE;
    enrollment runner_enrollment%ROWTYPE;
    registration runner_registration%ROWTYPE;
    loss_registration runner_registration%ROWTYPE;
    connection_head runner_connection_authority_head%ROWTYPE;
    connection runner_connection_event%ROWTYPE;
    current_registration numeric(20, 0);
    current_placement_ordinal numeric(20, 0);
    pending runner_pending_enrollment%ROWTYPE;
    predecessor runner_enrollment%ROWTYPE;
    predecessor_loss runner_connection_loss_epoch%ROWTYPE;
    boundary_count bigint;
    release_count bigint;
BEGIN
    SELECT * INTO request
      FROM replace_lost_runner_command
     WHERE command_id = NEW.command_id;
    SELECT * INTO staged
      FROM runner_workspace_provisioning_authorization
     WHERE authorization_id = NEW.repository_authorization_id;
    SELECT * INTO receipt
      FROM runner_replacement_workspace_receipt
     WHERE authorization_id = NEW.repository_authorization_id;
    SELECT * INTO lost
      FROM runner_session_placement_record
     WHERE session_id = NEW.session_id
       AND event_ordinal = staged.lost_placement_event_ordinal;
    SELECT event_ordinal INTO current_placement_ordinal
      FROM runner_current_session_placement
     WHERE session_id = NEW.session_id;
    SELECT * INTO successor
      FROM runner_session_placement_record
     WHERE session_id = NEW.session_id
       AND event_ordinal = current_placement_ordinal;
    SELECT * INTO enrollment
      FROM runner_enrollment
     WHERE enrollment_id = NEW.target_enrollment_id;
    SELECT * INTO registration
      FROM runner_registration
     WHERE enrollment_id = NEW.target_enrollment_id
       AND registration_revision = NEW.target_registration_revision;
    SELECT * INTO loss_registration
      FROM runner_registration
     WHERE enrollment_id = lost.registration_enrollment_id
       AND registration_revision = lost.loss_registration_revision;
    SELECT registration_revision INTO current_registration
      FROM runner_current_registration
     WHERE enrollment_id = NEW.target_enrollment_id;
    SELECT * INTO connection_head
      FROM runner_connection_authority_head
     WHERE enrollment_id = NEW.target_enrollment_id;
    SELECT * INTO connection
      FROM runner_connection_event
     WHERE enrollment_id = connection_head.enrollment_id
       AND connection_epoch = connection_head.connection_epoch
       AND event_ordinal = connection_head.connection_event_ordinal;
    SELECT * INTO pending
      FROM runner_pending_enrollment
     WHERE request_id = request.target_pending_request_id;
    SELECT * INTO predecessor
      FROM runner_enrollment
     WHERE enrollment_id = pending.predecessor_enrollment_id;
    SELECT * INTO predecessor_loss
      FROM runner_connection_loss_epoch
     WHERE enrollment_id = pending.predecessor_enrollment_id
       AND loss_epoch = pending.predecessor_loss_epoch;
    SELECT count(*) INTO boundary_count
      FROM session_runner_placement_frontier AS pointer
      JOIN semantic_transcript_entry AS entry
        ON entry.source_session_id = pointer.session_id
       AND entry.semantic_entry_id = pointer.semantic_entry_id
       AND entry.payload_kind = 'runner_placement_changed'
       AND entry.runner_placement_revision = pointer.placement_revision
       AND entry.runner_placement_event_ordinal = NEW.placement_event_ordinal
     WHERE pointer.session_id = NEW.session_id
       AND pointer.placement_revision = NEW.placement_revision
       AND pointer.semantic_entry_id = receipt.boundary_entry_id
       AND pointer.context_frontier_id = receipt.boundary_frontier_id;
    SELECT count(*) INTO release_count
      FROM runner_workspace_release AS release
     WHERE release.session_id = NEW.session_id
       AND release.placement_revision = staged.lost_placement_revision
       AND release.runner_id = NEW.prior_runner_id
       AND release.manifest_id = lost.workspace_manifest_id
       AND release.retired_placement_event_ordinal = lost.event_ordinal
       AND release.successor_placement_event_ordinal = successor.event_ordinal
       AND release.enrollment_id = NEW.target_enrollment_id
       AND release.connection_epoch = NEW.target_connection_epoch
       AND release.connection_event_ordinal = NEW.target_connection_event_ordinal
       AND release.state_kind = 'pending';

    IF request.command_id IS NULL
       OR request.session_id IS DISTINCT FROM NEW.session_id
       OR staged.authorization_id IS NULL
       OR staged.command_id IS DISTINCT FROM NEW.command_id
       OR staged.session_id IS DISTINCT FROM NEW.session_id
       OR receipt.authorization_id IS NULL
       OR receipt.session_id IS DISTINCT FROM NEW.session_id
       OR NOT (
            (request.target_kind = 'runner'
                AND request.target_runner_id IS NOT DISTINCT FROM NEW.new_runner_id
                AND NEW.prior_runner_id <> NEW.new_runner_id)
            OR (request.target_kind = 'same_runner_reenrollment'
                AND request.target_runner_id IS NOT DISTINCT FROM NEW.new_runner_id
                AND NEW.prior_runner_id = NEW.new_runner_id
                AND lost.loss_source_kind = 'registration'
                AND lost.loss_registration_revision IS NOT NULL
                AND lost.registration_enrollment_id IS NOT DISTINCT FROM
                        NEW.target_enrollment_id
                AND loss_registration.enrollment_id IS NOT NULL
                AND loss_registration.runner_id IS NOT DISTINCT FROM NEW.new_runner_id
                AND loss_registration.authentication_reference_id IS NOT DISTINCT FROM
                        registration.authentication_reference_id
                AND lost.loss_registration_revision <=
                        NEW.target_registration_revision)
            OR (request.target_kind = 'pending_enrollment'
                AND request.target_pending_request_id IS NOT NULL
                AND pending.request_id IS NOT NULL
                AND pending.enrollment_id IS NOT DISTINCT FROM
                        NEW.target_enrollment_id
                AND pending.predecessor_enrollment_id IS NOT DISTINCT FROM
                        lost.loss_fence_enrollment_id
                AND (lost.observed_runner_loss_epoch IS NULL
                    OR lost.observed_runner_loss_epoch <
                        pending.predecessor_loss_epoch)
                AND predecessor.enrollment_id IS NOT NULL
                AND pending.predecessor_enrollment_id IS NOT DISTINCT FROM
                        predecessor.enrollment_id
                AND predecessor.runner_id IS NOT DISTINCT FROM NEW.prior_runner_id
                AND predecessor.state_kind = 'revoked'
                AND predecessor_loss.enrollment_id IS NOT NULL
                AND predecessor_loss.enrollment_id IS NOT DISTINCT FROM
                        pending.predecessor_enrollment_id
                AND predecessor_loss.loss_epoch IS NOT DISTINCT FROM
                        pending.predecessor_loss_epoch
                AND lost.loss_source_kind = 'connection'
                AND NEW.prior_runner_id <> NEW.new_runner_id)
       )
       OR request.expected_placement_revision IS DISTINCT FROM
            staged.lost_placement_revision
       OR staged.lost_placement_event_ordinal + 1 IS DISTINCT FROM
            NEW.placement_event_ordinal
       OR staged.successor_placement_revision IS DISTINCT FROM
            NEW.placement_revision
       OR staged.enrollment_id IS DISTINCT FROM NEW.target_enrollment_id
       OR staged.runner_id IS DISTINCT FROM NEW.new_runner_id
       OR staged.registration_revision IS DISTINCT FROM
            NEW.target_registration_revision
       OR lost.event_kind IS DISTINCT FROM 'runner_lost'
       OR lost.state_kind IS DISTINCT FROM 'runner_lost'
       OR lost.placement_revision IS DISTINCT FROM
            staged.lost_placement_revision
       OR lost.lost_runner_id IS DISTINCT FROM NEW.prior_runner_id
       OR current_placement_ordinal IS DISTINCT FROM NEW.placement_event_ordinal
       OR successor.event_kind IS DISTINCT FROM 'runner_replaced'
       OR successor.state_kind IS DISTINCT FROM 'pinned'
       OR successor.placement_revision IS DISTINCT FROM NEW.placement_revision
       OR successor.pinned_runner_id IS DISTINCT FROM NEW.new_runner_id
       OR successor.registration_enrollment_id IS DISTINCT FROM
            NEW.target_enrollment_id
       OR successor.registration_revision IS DISTINCT FROM
            NEW.target_registration_revision
       OR successor.workspace_requirement_kind IS DISTINCT FROM
            'repository_worktree'
       OR successor.requested_repository_key IS DISTINCT FROM
            staged.repository_key
       OR successor.requested_sandbox_profile IS DISTINCT FROM
            staged.sandbox_profile
       OR successor.requested_credential_profile_name IS DISTINCT FROM
            staged.credential_profile_name
       OR successor.pinned_working_directory IS DISTINCT FROM
            receipt.execution_directory
       OR successor.workspace_repository_key IS DISTINCT FROM
            receipt.repository_key
       OR successor.workspace_working_directory IS DISTINCT FROM
            receipt.execution_directory
       OR successor.workspace_manifest_id IS DISTINCT FROM receipt.manifest_id
       OR successor.workspace_placement_revision IS DISTINCT FROM
            receipt.placement_revision
       OR successor.workspace_clone_url_digest IS DISTINCT FROM
            receipt.canonical_clone_url_digest
       OR successor.workspace_credential_profile_name IS DISTINCT FROM
            receipt.credential_profile_name
       OR successor.workspace_sandbox_profile IS DISTINCT FROM
            receipt.sandbox_profile
       OR successor.workspace_relative_path IS DISTINCT FROM receipt.relative_path
       OR successor.workspace_recovery_kind IS DISTINCT FROM receipt.recovery_kind
       OR successor.workspace_branch_name IS DISTINCT FROM receipt.branch_name
       OR successor.workspace_revision IS DISTINCT FROM receipt.revision
       OR NEW.working_directory IS DISTINCT FROM receipt.execution_directory
       OR NEW.sandbox_profile IS DISTINCT FROM staged.sandbox_profile
       OR enrollment.runner_id IS DISTINCT FROM NEW.new_runner_id
       OR enrollment.state_kind IS DISTINCT FROM 'active'
       OR registration.runner_id IS DISTINCT FROM NEW.new_runner_id
       OR current_registration IS DISTINCT FROM NEW.target_registration_revision
       OR connection_head.connection_epoch IS DISTINCT FROM
            NEW.target_connection_epoch
       OR connection_head.connection_event_ordinal IS DISTINCT FROM
            NEW.target_connection_event_ordinal
       OR connection.state_kind IS DISTINCT FROM 'connected'
       OR boundary_count <> 1
       OR (request.target_kind = 'same_runner_reenrollment' AND release_count <> 1)
       OR (request.target_kind <> 'same_runner_reenrollment' AND release_count <> 0)
       OR EXISTS (
            SELECT 1
              FROM runner_workspace_free_replacement_stage AS workspace_free
             WHERE workspace_free.command_id = NEW.command_id
       )
    THEN
        RAISE EXCEPTION
            'repository pinned replacement lacks exact terminal authority'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER repository_pinned_replacement_result_is_authorized
AFTER INSERT ON replace_lost_runner_result
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
WHEN (
    NEW.result_kind = 'applied'
    AND NEW.placement_state_kind = 'pinned'
    AND NEW.repository_authorization_id IS NOT NULL
)
EXECUTE FUNCTION require_repository_pinned_replacement_result_authority();
