-- Retain one exact workspace-free pinned replacement command stage. The stage
-- owns no successor authority and appends no relocation facts; the later
-- terminal transaction rechecks and consumes the selected runner.

CREATE TABLE runner_workspace_free_replacement_stage (
    command_id uuid PRIMARY KEY,
    session_id uuid NOT NULL,
    lost_placement_event_ordinal numeric(20, 0) NOT NULL,
    lost_placement_revision numeric(20, 0) NOT NULL,
    requested_working_directory runner_exact_text NOT NULL,

    CONSTRAINT runner_workspace_free_replacement_stage_command_session
        UNIQUE (command_id, session_id),
    CONSTRAINT runner_workspace_free_replacement_stage_positive_u64 CHECK (
        lost_placement_event_ordinal BETWEEN 1 AND 18446744073709551615
        AND lost_placement_revision BETWEEN 1 AND 18446744073709551615
    ),
    CONSTRAINT runner_workspace_free_replacement_stage_command_fk
        FOREIGN KEY (command_id, session_id)
        REFERENCES replace_lost_runner_command (command_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT runner_workspace_free_replacement_stage_placement_fk
        FOREIGN KEY (
            session_id, lost_placement_event_ordinal,
            lost_placement_revision
        )
        REFERENCES runner_session_placement_record (
            session_id, event_ordinal, placement_revision
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION require_runner_workspace_free_replacement_stage()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    request replace_lost_runner_command%ROWTYPE;
    current_placement_ordinal numeric(20, 0);
    placement runner_session_placement_record%ROWTYPE;
BEGIN
    PERFORM 1
      FROM session_scheduler
     WHERE session_id = NEW.session_id
     FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'workspace-free replacement stage lacks its scheduler'
            USING ERRCODE = '23514';
    END IF;

    SELECT event_ordinal INTO current_placement_ordinal
      FROM runner_current_session_placement
     WHERE session_id = NEW.session_id
     FOR UPDATE;
    SELECT * INTO request
      FROM replace_lost_runner_command
     WHERE command_id = NEW.command_id;
    SELECT * INTO placement
      FROM runner_session_placement_record
     WHERE session_id = NEW.session_id
       AND event_ordinal = NEW.lost_placement_event_ordinal;

    IF NOT FOUND
       OR request.session_id IS DISTINCT FROM NEW.session_id
       OR request.expected_placement_revision IS DISTINCT FROM
            NEW.lost_placement_revision
       OR current_placement_ordinal IS DISTINCT FROM
            NEW.lost_placement_event_ordinal
       OR placement.placement_revision IS DISTINCT FROM
            NEW.lost_placement_revision
       OR placement.event_kind IS DISTINCT FROM 'runner_lost'
       OR placement.state_kind IS DISTINCT FROM 'runner_lost'
       OR placement.workspace_requirement_kind IS DISTINCT FROM 'none'
       OR placement.directory_selection_kind IS DISTINCT FROM 'exact'
       OR placement.requested_working_directory IS DISTINCT FROM
            NEW.requested_working_directory
       OR EXISTS (
            SELECT 1
              FROM runner_workspace_provisioning_authorization AS provisioning
             WHERE provisioning.command_id = NEW.command_id
       )
    THEN
        RAISE EXCEPTION 'workspace-free replacement stage lacks exact lost-placement authority'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_workspace_free_replacement_stage_is_checked
AFTER INSERT ON runner_workspace_free_replacement_stage
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_workspace_free_replacement_stage();

CREATE FUNCTION require_runner_replacement_stage_locus_exclusive()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM runner_workspace_free_replacement_stage AS workspace_free
         WHERE workspace_free.command_id = NEW.command_id
    ) THEN
        RAISE EXCEPTION 'replacement command has both workspace stage loci'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_workspace_provisioning_stage_locus_is_exclusive
AFTER INSERT ON runner_workspace_provisioning_authorization
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_replacement_stage_locus_exclusive();

CREATE TRIGGER runner_workspace_free_replacement_stage_is_append_only
BEFORE UPDATE OR DELETE ON runner_workspace_free_replacement_stage
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_workspace_free_replacement_stage_rejects_truncate
BEFORE TRUNCATE ON runner_workspace_free_replacement_stage
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();
