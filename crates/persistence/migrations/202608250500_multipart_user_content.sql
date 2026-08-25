-- Canonical ordered multipart content for submit commands and accepted inputs.
-- Existing text rows are converted once in SQL; no runtime compatibility
-- decoder or startup upgrade path exists.

ALTER TABLE submit_input_command
    ADD COLUMN content_parts_creation_xid xid8 NOT NULL
        DEFAULT pg_current_xact_id();

ALTER TABLE submit_input_command
    ADD COLUMN result_attachment_digest bytea,
    ADD COLUMN result_attachment_maximum_bytes numeric(20, 0),
    ADD CONSTRAINT submit_input_command_result_attachment_digest_shape
        CHECK (
            result_attachment_digest IS NULL
            OR octet_length(result_attachment_digest) = 32
        ),
    ADD CONSTRAINT submit_input_command_result_attachment_maximum_bytes_u64
        CHECK (
            result_attachment_maximum_bytes IS NULL
            OR (
                result_attachment_maximum_bytes >= 1
                AND result_attachment_maximum_bytes <= 18446744073709551615
            )
        );

ALTER TABLE submit_input_command
    DROP CONSTRAINT submit_input_command_rejection_kind_closed;

ALTER TABLE submit_input_command
    ADD CONSTRAINT submit_input_command_rejection_kind_closed
        CHECK (
            rejection_kind IS NULL
            OR rejection_kind IN (
                'attachment_blob_not_found',
                'attachment_byte_budget_exceeded',
                'session_not_found',
                'no_active_turn',
                'active_turn_present',
                'active_turn_mismatch',
                'session_defaults_version_mismatch',
                'unknown_model_alias',
                'acceptance_position_exhausted',
                'safe_point_unavailable_while_stopping',
                'interrupt_already_applied',
                'interrupt_unavailable_while_awaiting_approval'
            )
        ),
    ADD CONSTRAINT submit_input_command_attachment_result_evidence_shape
        CHECK (
            (
                rejection_kind = 'attachment_blob_not_found'
                AND result_kind = 'rejected'
                AND result_accepted_input_id IS NULL
                AND result_turn_id IS NULL
                AND result_actual_active_turn_id IS NULL
                AND result_expected_active_turn_id IS NULL
                AND result_expected_defaults_version IS NULL
                AND result_current_defaults_version IS NULL
                AND result_unknown_alias_id IS NULL
                AND result_selected_defaults_version IS NULL
                AND result_last_position IS NULL
                AND result_existing_interrupt_command_id IS NULL
                AND result_attachment_digest IS NOT NULL
                AND result_attachment_maximum_bytes IS NULL
            )
            OR
            (
                rejection_kind = 'attachment_byte_budget_exceeded'
                AND result_kind = 'rejected'
                AND result_accepted_input_id IS NULL
                AND result_turn_id IS NULL
                AND result_actual_active_turn_id IS NULL
                AND result_expected_active_turn_id IS NULL
                AND result_expected_defaults_version IS NULL
                AND result_current_defaults_version IS NULL
                AND result_unknown_alias_id IS NULL
                AND result_selected_defaults_version IS NULL
                AND result_last_position IS NULL
                AND result_existing_interrupt_command_id IS NULL
                AND result_attachment_digest IS NULL
                AND result_attachment_maximum_bytes IS NOT NULL
            )
            OR
            (
                (
                    rejection_kind IS NULL
                    OR rejection_kind NOT IN (
                        'attachment_blob_not_found',
                        'attachment_byte_budget_exceeded'
                    )
                )
                AND result_attachment_digest IS NULL
                AND result_attachment_maximum_bytes IS NULL
            )
        );

-- Preserve every inherited terminal-result correlation while admitting only
-- the two claim-first attachment-authority rejection shapes.
DO $$
DECLARE
    inherited_result_shape text;
BEGIN
    SELECT pg_get_expr(conbin, conrelid)
      INTO inherited_result_shape
      FROM pg_constraint
     WHERE conrelid = 'submit_input_command'::regclass
       AND conname = 'submit_input_command_result_shape';

    IF inherited_result_shape IS NULL THEN
        RAISE EXCEPTION 'submit-input result-shape constraint is absent';
    END IF;

    ALTER TABLE submit_input_command
        DROP CONSTRAINT submit_input_command_result_shape;

    EXECUTE format(
        'ALTER TABLE submit_input_command ADD CONSTRAINT submit_input_command_result_shape CHECK ((%s) OR (result_kind = ''rejected'' AND rejection_kind = ''attachment_blob_not_found'' AND result_accepted_input_id IS NULL AND result_turn_id IS NULL AND result_actual_active_turn_id IS NULL AND result_expected_active_turn_id IS NULL AND result_expected_defaults_version IS NULL AND result_current_defaults_version IS NULL AND result_unknown_alias_id IS NULL AND result_selected_defaults_version IS NULL AND result_last_position IS NULL AND result_existing_interrupt_command_id IS NULL AND result_attachment_digest IS NOT NULL AND result_attachment_maximum_bytes IS NULL) OR (result_kind = ''rejected'' AND rejection_kind = ''attachment_byte_budget_exceeded'' AND result_accepted_input_id IS NULL AND result_turn_id IS NULL AND result_actual_active_turn_id IS NULL AND result_expected_active_turn_id IS NULL AND result_expected_defaults_version IS NULL AND result_current_defaults_version IS NULL AND result_unknown_alias_id IS NULL AND result_selected_defaults_version IS NULL AND result_last_position IS NULL AND result_existing_interrupt_command_id IS NULL AND result_attachment_digest IS NULL AND result_attachment_maximum_bytes IS NOT NULL))',
        inherited_result_shape
    );
END;
$$;

ALTER TABLE accepted_input
    ADD COLUMN content_parts_creation_xid xid8 NOT NULL
        DEFAULT pg_current_xact_id();

CREATE TABLE submit_input_command_content_part (
    command_id uuid NOT NULL,
    position smallint NOT NULL,
    part_kind text NOT NULL,
    text_value text,
    blob_digest bytea,
    attachment_kind text,
    declared_media_type text,
    display_filename text,

    CONSTRAINT submit_input_command_content_part_pk
        PRIMARY KEY (command_id, position),
    CONSTRAINT submit_input_command_content_part_position
        CHECK (position BETWEEN 0 AND 255),
    CONSTRAINT submit_input_command_content_part_shape CHECK (
        (part_kind = 'text'
            AND text_value IS NOT NULL
            AND char_length(text_value) > 0
            AND blob_digest IS NULL
            AND attachment_kind IS NULL
            AND declared_media_type IS NULL
            AND display_filename IS NULL)
        OR
        (part_kind = 'attachment'
            AND text_value IS NULL
            AND blob_digest IS NOT NULL
            AND octet_length(blob_digest) = 32
            AND attachment_kind IS NOT NULL
            AND attachment_kind IN ('image', 'document', 'file')
            AND declared_media_type IS NOT NULL
            AND octet_length(declared_media_type) BETWEEN 1 AND 255
            AND declared_media_type COLLATE "C" ~ '^[!-~]+$'
            AND (display_filename IS NULL OR (
                octet_length(convert_to(display_filename, 'UTF8')) BETWEEN 1 AND 255
                AND display_filename NOT IN ('.', '..')
                AND position('/' IN display_filename) = 0
                AND position(chr(92) IN display_filename) = 0)))
    ),
    CONSTRAINT submit_input_command_content_part_command_fk
        FOREIGN KEY (command_id) REFERENCES submit_input_command(command_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE accepted_input_content_part (
    accepted_input_id uuid NOT NULL,
    position smallint NOT NULL,
    part_kind text NOT NULL,
    text_value text,
    blob_digest bytea,
    attachment_kind text,
    declared_media_type text,
    display_filename text,

    CONSTRAINT accepted_input_content_part_pk
        PRIMARY KEY (accepted_input_id, position),
    CONSTRAINT accepted_input_content_part_position
        CHECK (position BETWEEN 0 AND 255),
    CONSTRAINT accepted_input_content_part_shape CHECK (
        (part_kind = 'text'
            AND text_value IS NOT NULL
            AND char_length(text_value) > 0
            AND blob_digest IS NULL
            AND attachment_kind IS NULL
            AND declared_media_type IS NULL
            AND display_filename IS NULL)
        OR
        (part_kind = 'attachment'
            AND text_value IS NULL
            AND blob_digest IS NOT NULL
            AND octet_length(blob_digest) = 32
            AND attachment_kind IS NOT NULL
            AND attachment_kind IN ('image', 'document', 'file')
            AND declared_media_type IS NOT NULL
            AND octet_length(declared_media_type) BETWEEN 1 AND 255
            AND declared_media_type COLLATE "C" ~ '^[!-~]+$'
            AND (display_filename IS NULL OR (
                octet_length(convert_to(display_filename, 'UTF8')) BETWEEN 1 AND 255
                AND display_filename NOT IN ('.', '..')
                AND position('/' IN display_filename) = 0
                AND position(chr(92) IN display_filename) = 0)))
    ),
    CONSTRAINT accepted_input_content_part_input_fk
        FOREIGN KEY (accepted_input_id) REFERENCES accepted_input(accepted_input_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT accepted_input_content_part_blob_fk
        FOREIGN KEY (blob_digest) REFERENCES blob(digest)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

INSERT INTO submit_input_command_content_part
    (command_id, position, part_kind, text_value)
SELECT command_id, 0, 'text', content_text
  FROM submit_input_command;

INSERT INTO accepted_input_content_part
    (accepted_input_id, position, part_kind, text_value)
SELECT accepted_input_id, 0, 'text', content_text
  FROM accepted_input;

CREATE FUNCTION submit_input_command_parts_are_valid(checked_command uuid)
RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT count(*) BETWEEN 1 AND 256
       AND min(position) = 0
       AND max(position) = count(*) - 1
       AND COALESCE(sum(octet_length(convert_to(text_value, 'UTF8')))
            FILTER (WHERE part_kind = 'text'), 0) <= 1048576
       AND NOT EXISTS (
            SELECT 1
              FROM submit_input_command_content_part AS current
              JOIN submit_input_command_content_part AS prior
                ON prior.command_id = current.command_id
               AND prior.position + 1 = current.position
             WHERE current.command_id = checked_command
               AND current.part_kind = 'text'
               AND prior.part_kind = 'text')
      FROM submit_input_command_content_part
     WHERE command_id = checked_command;
$$;

CREATE FUNCTION accepted_input_parts_are_valid(checked_input uuid)
RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT count(*) BETWEEN 1 AND 256
       AND min(position) = 0
       AND max(position) = count(*) - 1
       AND COALESCE(sum(octet_length(convert_to(text_value, 'UTF8')))
            FILTER (WHERE part_kind = 'text'), 0) <= 1048576
       AND NOT EXISTS (
            SELECT 1
              FROM accepted_input_content_part AS current
              JOIN accepted_input_content_part AS prior
                ON prior.accepted_input_id = current.accepted_input_id
               AND prior.position + 1 = current.position
             WHERE current.accepted_input_id = checked_input
               AND current.part_kind = 'text'
               AND prior.part_kind = 'text')
      FROM accepted_input_content_part
     WHERE accepted_input_id = checked_input;
$$;

CREATE FUNCTION accepted_input_parts_match_command(checked_input uuid)
RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT accepting_command_id IS NULL OR (
        NOT EXISTS (
            SELECT position, part_kind, text_value, blob_digest,
                   attachment_kind, declared_media_type, display_filename
              FROM accepted_input_content_part
             WHERE accepted_input_id = checked_input
            EXCEPT
            SELECT position, part_kind, text_value, blob_digest,
                   attachment_kind, declared_media_type, display_filename
              FROM submit_input_command_content_part
             WHERE command_id = accepted.accepting_command_id)
        AND NOT EXISTS (
            SELECT position, part_kind, text_value, blob_digest,
                   attachment_kind, declared_media_type, display_filename
              FROM submit_input_command_content_part
             WHERE command_id = accepted.accepting_command_id
            EXCEPT
            SELECT position, part_kind, text_value, blob_digest,
                   attachment_kind, declared_media_type, display_filename
              FROM accepted_input_content_part
             WHERE accepted_input_id = checked_input)
    )
      FROM accepted_input AS accepted
     WHERE accepted.accepted_input_id = checked_input;
$$;

CREATE FUNCTION accepted_input_content_parts_json(checked_input uuid)
RETURNS jsonb LANGUAGE sql STABLE AS $$
    SELECT COALESCE(
        jsonb_agg(
            jsonb_build_object(
                'position', position,
                'part_kind', part_kind,
                'text_value', text_value,
                'blob_digest', CASE
                    WHEN blob_digest IS NULL THEN NULL
                    ELSE 'sha256:' || encode(blob_digest, 'hex')
                END,
                'attachment_kind', attachment_kind,
                'declared_media_type', declared_media_type,
                'display_filename', display_filename
            ) ORDER BY position
        ),
        '[]'::jsonb
    )
      FROM accepted_input_content_part
     WHERE accepted_input_id = checked_input;
$$;

CREATE FUNCTION submit_input_command_content_parts_json(checked_command uuid)
RETURNS jsonb LANGUAGE sql STABLE AS $$
    SELECT COALESCE(
        jsonb_agg(
            jsonb_build_object(
                'position', position,
                'part_kind', part_kind,
                'text_value', text_value,
                'blob_digest', CASE
                    WHEN blob_digest IS NULL THEN NULL
                    ELSE 'sha256:' || encode(blob_digest, 'hex')
                END,
                'attachment_kind', attachment_kind,
                'declared_media_type', declared_media_type,
                'display_filename', display_filename
            ) ORDER BY position
        ),
        '[]'::jsonb
    )
      FROM submit_input_command_content_part
     WHERE command_id = checked_command;
$$;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM submit_input_command AS command
         WHERE NOT submit_input_command_parts_are_valid(command.command_id)
    ) THEN
        RAISE EXCEPTION
            'one-time submit-input content migration produced an invalid sequence'
            USING ERRCODE = '23514',
                CONSTRAINT = 'submit_input_command_content_parts_valid';
    END IF;
    IF EXISTS (
        SELECT 1
          FROM accepted_input AS accepted
         WHERE NOT accepted_input_parts_are_valid(accepted.accepted_input_id)
            OR NOT accepted_input_parts_match_command(accepted.accepted_input_id)
    ) THEN
        RAISE EXCEPTION
            'one-time accepted-input content migration produced an invalid sequence'
            USING ERRCODE = '23514',
                CONSTRAINT = 'accepted_input_content_parts_valid';
    END IF;
END;
$$;

CREATE FUNCTION require_submit_input_command_parts()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE checked_command uuid;
BEGIN
    checked_command := CASE WHEN TG_TABLE_NAME = 'submit_input_command'
        THEN NEW.command_id ELSE COALESCE(NEW.command_id, OLD.command_id) END;
    IF NOT submit_input_command_parts_are_valid(checked_command) THEN
        RAISE EXCEPTION 'submit-input command has invalid ordered content parts'
            USING ERRCODE = '23514',
                CONSTRAINT = 'submit_input_command_content_parts_valid';
    END IF;
    RETURN NULL;
END;
$$;

CREATE FUNCTION require_accepted_input_parts()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE checked_input uuid;
BEGIN
    checked_input := CASE WHEN TG_TABLE_NAME = 'accepted_input'
        THEN NEW.accepted_input_id
        ELSE COALESCE(NEW.accepted_input_id, OLD.accepted_input_id) END;
    IF NOT accepted_input_parts_are_valid(checked_input)
       OR NOT accepted_input_parts_match_command(checked_input)
    THEN
        RAISE EXCEPTION 'accepted input has invalid ordered content parts'
            USING ERRCODE = '23514',
                CONSTRAINT = 'accepted_input_content_parts_valid';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER submit_input_command_requires_content_parts
AFTER INSERT ON submit_input_command DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_submit_input_command_parts();

CREATE CONSTRAINT TRIGGER submit_input_command_content_parts_are_valid
AFTER INSERT OR UPDATE OR DELETE ON submit_input_command_content_part
DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
EXECUTE FUNCTION require_submit_input_command_parts();

CREATE CONSTRAINT TRIGGER accepted_input_requires_content_parts
AFTER INSERT ON accepted_input DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_accepted_input_parts();

CREATE CONSTRAINT TRIGGER accepted_input_content_parts_are_valid
AFTER INSERT OR UPDATE OR DELETE ON accepted_input_content_part
DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
EXECUTE FUNCTION require_accepted_input_parts();

CREATE FUNCTION reject_content_part_insert_after_parent_transaction()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE parent_creation_xid xid8;
BEGIN
    IF TG_TABLE_NAME = 'submit_input_command_content_part' THEN
        SELECT parent.content_parts_creation_xid
          INTO parent_creation_xid
          FROM submit_input_command AS parent
         WHERE parent.command_id = NEW.command_id;
    ELSE
        SELECT parent.content_parts_creation_xid
          INTO parent_creation_xid
          FROM accepted_input AS parent
         WHERE parent.accepted_input_id = NEW.accepted_input_id;
    END IF;

    IF parent_creation_xid IS DISTINCT FROM pg_current_xact_id() THEN
        RAISE EXCEPTION 'content parts are immutable after parent creation'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER submit_input_command_content_part_insert_is_creation_local
BEFORE INSERT ON submit_input_command_content_part
FOR EACH ROW EXECUTE FUNCTION reject_content_part_insert_after_parent_transaction();

CREATE TRIGGER accepted_input_content_part_insert_is_creation_local
BEFORE INSERT ON accepted_input_content_part
FOR EACH ROW EXECUTE FUNCTION reject_content_part_insert_after_parent_transaction();

CREATE TRIGGER accepted_input_content_parts_creation_is_immutable
BEFORE UPDATE OF content_parts_creation_xid ON accepted_input
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER submit_input_command_content_part_is_append_only
BEFORE UPDATE OR DELETE ON submit_input_command_content_part
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER accepted_input_content_part_is_append_only
BEFORE UPDATE OR DELETE ON accepted_input_content_part
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE OR REPLACE FUNCTION reject_invalid_accepted_input_change()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'accepted_input is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.disposition_kind = 'pending_steering'
       AND NEW.disposition_kind IN (
            'consumed_as_steering',
            'reclassified_as_turn_origin'
       )
       AND OLD.origin_turn_id IS NULL
       AND (
            (
                NEW.disposition_kind = 'consumed_as_steering'
                AND NEW.origin_turn_id IS NULL
                AND OLD.consuming_model_call_id IS NULL
                AND NEW.consuming_model_call_id IS NOT NULL
            )
            OR
            (
                NEW.disposition_kind = 'reclassified_as_turn_origin'
                AND NEW.origin_turn_id IS NOT NULL
                AND OLD.consuming_model_call_id IS NULL
                AND NEW.consuming_model_call_id IS NULL
            )
       )
       AND ROW(
            OLD.accepted_input_id,
            OLD.accepting_command_id,
            OLD.session_id,
            OLD.delivery_kind,
            OLD.expected_active_turn_id,
            OLD.expected_defaults_version,
            OLD.model_override_kind,
            OLD.replacement_model_kind,
            OLD.replacement_direct_model_selection_id,
            OLD.replacement_model_alias_id,
            OLD.acceptance_position
       ) IS NOT DISTINCT FROM ROW(
            NEW.accepted_input_id,
            NEW.accepting_command_id,
            NEW.session_id,
            NEW.delivery_kind,
            NEW.expected_active_turn_id,
            NEW.expected_defaults_version,
            NEW.model_override_kind,
            NEW.replacement_model_kind,
            NEW.replacement_direct_model_selection_id,
            NEW.replacement_model_alias_id,
            NEW.acceptance_position
       )
    THEN
        RETURN NEW;
    END IF;

    RAISE EXCEPTION
        'accepted_input is immutable outside pending-steering disposition'
        USING ERRCODE = '23514';
END;
$$;

CREATE OR REPLACE FUNCTION require_submit_input_legacy_effect_correlation()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    matching_records bigint;
BEGIN
    IF NEW.result_kind = 'applied' AND NEW.result_turn_id IS NOT NULL THEN
        SELECT count(*)
          INTO matching_records
          FROM accepted_input AS accepted
          JOIN queued_input_origin AS queued
            ON queued.accepted_input_id = accepted.accepted_input_id
           AND queued.session_id = accepted.session_id
           AND queued.acceptance_position = accepted.acceptance_position
           AND queued.turn_id = accepted.origin_turn_id
          JOIN session_defaults_version AS defaults
            ON defaults.session_id = queued.session_id
           AND defaults.version = queued.defaults_version
         WHERE accepted.accepting_command_id = NEW.command_id
           AND accepted.accepted_input_id = NEW.result_accepted_input_id
           AND accepted.session_id = NEW.result_session_id
           AND accepted_input_parts_match_command(accepted.accepted_input_id)
           AND accepted.delivery_kind = NEW.delivery_kind
           AND accepted.expected_active_turn_id
               IS NOT DISTINCT FROM NEW.expected_active_turn_id
           AND accepted.expected_defaults_version = NEW.expected_defaults_version
           AND accepted.model_override_kind = NEW.model_override_kind
           AND accepted.replacement_model_kind
               IS NOT DISTINCT FROM NEW.replacement_model_kind
           AND accepted.replacement_direct_model_selection_id
               IS NOT DISTINCT FROM NEW.replacement_direct_model_selection_id
           AND accepted.replacement_model_alias_id
               IS NOT DISTINCT FROM NEW.replacement_model_alias_id
           AND accepted.disposition_kind = 'origin_of'
           AND accepted.origin_turn_id = NEW.result_turn_id
           AND queued.priority_kind = 'ordinary'
           AND queued.defaults_version = NEW.expected_defaults_version
           AND (
               (
                   NEW.model_override_kind = 'use_session_default'
                   AND queued.requested_model_kind = defaults.model_selection_kind
                   AND queued.requested_direct_model_selection_id
                       IS NOT DISTINCT FROM defaults.direct_model_selection_id
                   AND queued.requested_model_alias_id
                       IS NOT DISTINCT FROM defaults.model_alias_id
               )
               OR
               (
                   NEW.model_override_kind = 'replace_with'
                   AND queued.requested_model_kind = NEW.replacement_model_kind
                   AND queued.requested_direct_model_selection_id
                       IS NOT DISTINCT FROM
                           NEW.replacement_direct_model_selection_id
                   AND queued.requested_model_alias_id
                       IS NOT DISTINCT FROM NEW.replacement_model_alias_id
               )
           )
           AND (
               (
                   queued.requested_model_kind = 'direct'
                   AND queued.frozen_model_kind = 'direct'
                   AND queued.frozen_direct_model_selection_id
                       = queued.requested_direct_model_selection_id
               )
               OR
               (
                   queued.requested_model_kind = 'alias'
                   AND queued.frozen_model_kind = 'frozen_alias'
                   AND queued.frozen_model_alias_id = queued.requested_model_alias_id
               )
           )
           AND queued.model_parameters = 'provider_defaults'
           AND queued.known_provider_failure_retry = 'disabled'
           AND queued.model_fallback = 'disabled';
    ELSIF NEW.result_kind = 'applied' THEN
        SELECT count(*)
          INTO matching_records
          FROM accepted_input AS accepted
         WHERE accepted.accepting_command_id = NEW.command_id
           AND accepted.accepted_input_id = NEW.result_accepted_input_id
           AND accepted.session_id = NEW.result_session_id
           AND accepted_input_parts_match_command(accepted.accepted_input_id)
           AND accepted.delivery_kind = 'next_safe_point'
           AND accepted.delivery_kind = NEW.delivery_kind
           AND accepted.expected_active_turn_id = NEW.expected_active_turn_id
           AND accepted.expected_defaults_version IS NULL
           AND accepted.model_override_kind IS NULL
           AND accepted.replacement_model_kind IS NULL
           AND accepted.replacement_direct_model_selection_id IS NULL
           AND accepted.replacement_model_alias_id IS NULL
           AND accepted.disposition_kind = 'pending_steering'
           AND accepted.origin_turn_id IS NULL
           AND accepted.expected_active_turn_id = NEW.result_actual_active_turn_id
           AND NOT EXISTS (
               SELECT 1
                 FROM queued_input_origin
                WHERE accepted_input_id = accepted.accepted_input_id
           );
    ELSE
        SELECT count(*)
          INTO matching_records
          FROM accepted_input
         WHERE accepting_command_id = NEW.command_id;

        IF matching_records = 0
           AND NEW.rejection_kind = 'unknown_model_alias'
        THEN
            SELECT count(*)
              INTO matching_records
              FROM session_defaults_version AS defaults
             WHERE defaults.session_id = NEW.result_session_id
               AND defaults.version = NEW.result_selected_defaults_version
               AND (
                   (
                       NEW.model_override_kind = 'use_session_default'
                       AND defaults.model_selection_kind = 'alias'
                       AND defaults.model_alias_id = NEW.result_unknown_alias_id
                   )
                   OR
                   (
                       NEW.model_override_kind = 'replace_with'
                       AND NEW.replacement_model_kind = 'alias'
                       AND NEW.replacement_model_alias_id = NEW.result_unknown_alias_id
                   )
               );

            IF matching_records <> 1 THEN
                RAISE EXCEPTION
                    'submit-input command % has cross-wired unknown-alias evidence',
                    NEW.command_id
                    USING ERRCODE = '23503';
            END IF;
            matching_records := 0;
        END IF;

        IF matching_records = 0
           AND NEW.rejection_kind IN (
               'session_defaults_version_mismatch',
               'unknown_model_alias',
               'acceptance_position_exhausted'
           )
           AND NEW.delivery_kind IN ('after_current_turn', 'next_safe_point')
        THEN
            SELECT count(*)
              INTO matching_records
              FROM turn_lifecycle AS turn
              JOIN queued_input_origin AS queued
                ON queued.turn_id = turn.turn_id
               AND queued.session_id = turn.session_id
               AND queued.accepted_input_id = turn.origin_accepted_input_id
              JOIN accepted_input AS accepted
                ON accepted.accepted_input_id = queued.accepted_input_id
               AND accepted.session_id = turn.session_id
               AND accepted.origin_turn_id = turn.turn_id
               AND accepted.disposition_kind = 'origin_of'
             WHERE turn.turn_id = NEW.expected_active_turn_id
               AND turn.session_id = NEW.result_session_id;

            IF matching_records <> 1 THEN
                RAISE EXCEPTION
                    'submit-input rejection % has cross-wired source-turn evidence',
                    NEW.command_id
                    USING ERRCODE = '23503',
                        CONSTRAINT =
                            'submit_input_command_rejected_source_origin';
            END IF;
            matching_records := 0;
        END IF;
    END IF;

    IF matching_records <> (
        CASE WHEN NEW.result_kind = 'applied' THEN 1 ELSE 0 END
    ) THEN
        RAISE EXCEPTION
            'submit-input command % has an incomplete or cross-wired terminal effect',
            NEW.command_id
            USING ERRCODE = '23503';
    END IF;

    RETURN NULL;
END;
$$;

CREATE OR REPLACE FUNCTION require_interrupt_submit_input_effect_correlation()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    matching_records bigint;
BEGIN
    IF NEW.result_kind = 'applied'
       AND EXISTS (
            SELECT 1
              FROM turn_runner_recovery_interrupt_effect
             WHERE command_id = NEW.command_id
       )
    THEN
        SELECT count(*)
          INTO matching_records
          FROM turn_runner_recovery_interrupt_effect AS effect
          JOIN accepted_input AS accepted
            ON accepted.accepting_command_id = effect.command_id
           AND accepted.session_id = effect.session_id
          JOIN queued_input_origin AS successor
            ON successor.accepted_input_id = accepted.accepted_input_id
           AND successor.turn_id = accepted.origin_turn_id
           AND successor.session_id = accepted.session_id
           AND successor.acceptance_position = accepted.acceptance_position
          JOIN turn_lifecycle AS cancelled
            ON cancelled.session_id = effect.session_id
           AND cancelled.turn_id = effect.turn_id
          JOIN turn_attempt AS yielded_attempt
            ON yielded_attempt.turn_attempt_id = effect.yielded_turn_attempt_id
           AND yielded_attempt.turn_id = effect.turn_id
           AND yielded_attempt.session_id = effect.session_id
         WHERE effect.command_id = NEW.command_id
           AND effect.session_id = NEW.session_id
           AND effect.turn_id = NEW.expected_active_turn_id
           AND accepted.accepted_input_id = NEW.result_accepted_input_id
           AND accepted.session_id = NEW.result_session_id
           AND accepted_input_parts_match_command(accepted.accepted_input_id)
           AND accepted.delivery_kind = 'interrupt'
           AND accepted.expected_active_turn_id = NEW.expected_active_turn_id
           AND accepted.expected_defaults_version = NEW.expected_defaults_version
           AND accepted.model_override_kind = NEW.model_override_kind
           AND accepted.replacement_model_kind
               IS NOT DISTINCT FROM NEW.replacement_model_kind
           AND accepted.replacement_direct_model_selection_id
               IS NOT DISTINCT FROM NEW.replacement_direct_model_selection_id
           AND accepted.replacement_model_alias_id
               IS NOT DISTINCT FROM NEW.replacement_model_alias_id
           AND accepted.disposition_kind = 'origin_of'
           AND accepted.origin_turn_id = NEW.result_turn_id
           AND successor.priority_kind = 'interrupt_immediately_after'
           AND successor.interrupt_predecessor_turn_id = effect.turn_id
           AND successor.defaults_version = NEW.expected_defaults_version
           AND cancelled.state_kind = 'terminal'
           AND cancelled.terminal_attempt_id = effect.yielded_turn_attempt_id
           AND cancelled.terminal_model_call_id IS NULL
           AND (
                (
                    effect.interrupted_tool_attempt_id IS NULL
                    AND cancelled.terminal_disposition_kind = 'cancelled'
                    AND cancelled.terminal_tool_attempt_id IS NULL
                )
                OR (
                    effect.interrupted_tool_attempt_id IS NOT NULL
                    AND cancelled.terminal_disposition_kind =
                        'reconciliation_required'
                    AND cancelled.terminal_tool_attempt_id =
                        effect.interrupted_tool_attempt_id
                    AND EXISTS (
                        SELECT 1
                          FROM tool_attempt AS stopped_tool
                          JOIN runner_physical_attempt_lease_binding AS binding
                            ON binding.attempt_id = stopped_tool.attempt_id
                          JOIN runner_lease_generation AS lease
                            ON lease.lease_id = binding.lease_id
                           AND lease.attempt_id = stopped_tool.attempt_id
                           AND lease.session_id = stopped_tool.session_id
                          JOIN runner_current_lease_event AS lease_head
                            ON lease_head.lease_id = lease.lease_id
                           AND lease_head.generation = lease.generation
                          JOIN runner_lease_event AS lease_event
                            ON lease_event.lease_id = lease_head.lease_id
                           AND lease_event.generation = lease_head.generation
                           AND lease_event.event_ordinal = lease_head.event_ordinal
                          JOIN runner_session_placement_record AS leased_placement
                            ON leased_placement.session_id = lease.session_id
                           AND leased_placement.event_ordinal =
                                lease.placement_event_ordinal
                         WHERE stopped_tool.attempt_id =
                                effect.interrupted_tool_attempt_id
                           AND stopped_tool.session_id = effect.session_id
                           AND stopped_tool.turn_id = effect.turn_id
                           AND stopped_tool.state_kind = 'terminal'
                           AND stopped_tool.terminal_disposition_kind = 'ambiguous'
                           AND lease.runner_id = effect.runner_id
                           AND lease_event.state_kind IN (
                                'lost_execution_possible', 'lost_claimed'
                           )
                           AND lease.effect_class IN ('idempotent', 'side_effecting')
                           AND leased_placement.placement_revision =
                                effect.placement_revision
                           AND leased_placement.state_kind = 'pinned'
                           AND leased_placement.pinned_runner_id = effect.runner_id
                    )
                )
                OR (
                    effect.interrupted_tool_attempt_id IS NOT NULL
                    AND cancelled.terminal_disposition_kind = 'cancelled'
                    AND cancelled.terminal_tool_attempt_id IS NULL
                    AND EXISTS (
                        SELECT 1
                          FROM tool_attempt AS stopped_tool
                         WHERE stopped_tool.attempt_id =
                                effect.interrupted_tool_attempt_id
                           AND stopped_tool.session_id = effect.session_id
                           AND stopped_tool.turn_id = effect.turn_id
                           AND stopped_tool.state_kind = 'terminal'
                           AND stopped_tool.terminal_disposition_kind = 'known_failed'
                           AND stopped_tool.error_kind = 'crash_lost'
                           AND stopped_tool.error_detail IS NULL
                    )
                )
           )
           AND yielded_attempt.state_kind = 'ended'
           AND yielded_attempt.end_variant = 'without_stop'
           AND yielded_attempt.end_disposition = 'yielded_to_durable_wait'
           AND yielded_attempt.interrupt_command_id IS NULL
           AND yielded_attempt.interrupt_predecessor_turn_id IS NULL;
    ELSIF NEW.result_kind = 'applied' THEN
        SELECT count(*)
          INTO matching_records
          FROM accepted_input AS accepted
          JOIN queued_input_origin AS successor
            ON successor.accepted_input_id = accepted.accepted_input_id
           AND successor.turn_id = accepted.origin_turn_id
           AND successor.session_id = accepted.session_id
           AND successor.acceptance_position = accepted.acceptance_position
          JOIN turn_attempt AS stopped_attempt
            ON stopped_attempt.turn_id = NEW.expected_active_turn_id
           AND stopped_attempt.session_id = NEW.session_id
           AND (
                (
                    stopped_attempt.interrupt_command_id = NEW.command_id
                    AND stopped_attempt.interrupt_predecessor_turn_id =
                        NEW.expected_active_turn_id
                    AND (
                        stopped_attempt.state_kind = 'stop_requested'
                        OR (
                            stopped_attempt.state_kind = 'ended'
                            AND stopped_attempt.end_variant = 'after_cancellation'
                        )
                    )
                )
                OR (
                    stopped_attempt.state_kind = 'ended'
                    AND stopped_attempt.end_variant = 'without_stop'
                    AND stopped_attempt.end_disposition IN ('ambiguous', 'lost')
                    AND stopped_attempt.interrupt_command_id IS NULL
                    AND stopped_attempt.interrupt_predecessor_turn_id IS NULL
                    AND EXISTS (
                        SELECT 1
                          FROM turn_lifecycle AS reconciled
                         WHERE reconciled.turn_id = stopped_attempt.turn_id
                           AND reconciled.session_id = stopped_attempt.session_id
                           AND reconciled.state_kind = 'terminal'
                           AND reconciled.terminal_disposition_kind =
                                'reconciliation_required'
                           AND reconciled.terminal_attempt_id =
                                stopped_attempt.turn_attempt_id
                    )
                )
                OR (
                    stopped_attempt.state_kind = 'ended'
                    AND stopped_attempt.end_variant = 'without_stop'
                    AND stopped_attempt.end_disposition = 'yielded_to_durable_wait'
                    AND stopped_attempt.interrupt_command_id IS NULL
                    AND stopped_attempt.interrupt_predecessor_turn_id IS NULL
                    AND EXISTS (
                        SELECT 1
                          FROM session_delegation_wait AS waiting
                          JOIN tool_request AS awaiting
                            ON awaiting.request_id = waiting.awaiting_tool_request_id
                           AND awaiting.turn_id = waiting.parent_turn_id
                           AND awaiting.session_id = waiting.parent_session_id
                          JOIN model_call AS producing_call
                            ON producing_call.model_call_id =
                                awaiting.producing_model_call_id
                           AND producing_call.turn_id = awaiting.turn_id
                           AND producing_call.session_id = awaiting.session_id
                          JOIN turn_lifecycle AS cancelled
                            ON cancelled.turn_id = waiting.parent_turn_id
                           AND cancelled.session_id = waiting.parent_session_id
                         WHERE waiting.parent_turn_id = NEW.expected_active_turn_id
                           AND waiting.parent_session_id = NEW.session_id
                           AND waiting.wait_mode = 'foreground'
                           AND producing_call.turn_attempt_id =
                                stopped_attempt.turn_attempt_id
                           AND cancelled.state_kind = 'terminal'
                           AND cancelled.terminal_disposition_kind = 'cancelled'
                           AND cancelled.terminal_attempt_id IS NULL
                           AND cancelled.terminal_model_call_id IS NULL
                    )
                )
           )
         WHERE accepted.accepting_command_id = NEW.command_id
           AND accepted.accepted_input_id = NEW.result_accepted_input_id
           AND accepted.session_id = NEW.result_session_id
           AND accepted_input_parts_match_command(accepted.accepted_input_id)
           AND accepted.delivery_kind = 'interrupt'
           AND accepted.expected_active_turn_id = NEW.expected_active_turn_id
           AND accepted.expected_defaults_version = NEW.expected_defaults_version
           AND accepted.model_override_kind = NEW.model_override_kind
           AND accepted.replacement_model_kind
               IS NOT DISTINCT FROM NEW.replacement_model_kind
           AND accepted.replacement_direct_model_selection_id
               IS NOT DISTINCT FROM NEW.replacement_direct_model_selection_id
           AND accepted.replacement_model_alias_id
               IS NOT DISTINCT FROM NEW.replacement_model_alias_id
           AND accepted.disposition_kind = 'origin_of'
           AND accepted.origin_turn_id = NEW.result_turn_id
           AND successor.priority_kind = 'interrupt_immediately_after'
           AND successor.interrupt_predecessor_turn_id = NEW.expected_active_turn_id
           AND successor.defaults_version = NEW.expected_defaults_version;
    ELSIF NEW.rejection_kind = 'interrupt_unavailable_while_awaiting_approval'
    THEN
        SELECT count(*)
          INTO matching_records
          FROM turn_lifecycle AS parked
         WHERE parked.turn_id = NEW.result_actual_active_turn_id
           AND parked.session_id = NEW.result_session_id
           AND parked.state_kind = 'active'
           AND parked.active_phase_kind = 'awaiting_tool_approval'
           AND NOT EXISTS (
                SELECT 1
                  FROM accepted_input
                 WHERE accepting_command_id = NEW.command_id
           );
    ELSE
        SELECT count(*)
          INTO matching_records
          FROM submit_input_command AS existing
          JOIN accepted_input AS accepted
            ON accepted.accepting_command_id = existing.command_id
           AND accepted.accepted_input_id = existing.result_accepted_input_id
           AND accepted.session_id = existing.result_session_id
           AND accepted.origin_turn_id = existing.result_turn_id
          JOIN queued_input_origin AS successor
            ON successor.accepted_input_id = accepted.accepted_input_id
           AND successor.turn_id = accepted.origin_turn_id
           AND successor.session_id = accepted.session_id
           AND successor.priority_kind = 'interrupt_immediately_after'
           AND successor.interrupt_predecessor_turn_id = NEW.result_actual_active_turn_id
          JOIN turn_lifecycle AS active
            ON active.turn_id = NEW.result_actual_active_turn_id
           AND active.session_id = NEW.result_session_id
           AND active.state_kind = 'active'
          JOIN turn_attempt AS stopped_attempt
            ON stopped_attempt.turn_attempt_id = active.current_attempt_id
           AND stopped_attempt.turn_id = active.turn_id
           AND stopped_attempt.session_id = active.session_id
           AND stopped_attempt.interrupt_command_id = existing.command_id
           AND stopped_attempt.interrupt_predecessor_turn_id = active.turn_id
           AND (
                (
                    active.active_phase_kind = 'running'
                    AND stopped_attempt.state_kind = 'stop_requested'
                )
                OR (
                    active.active_phase_kind = 'awaiting_model_call_recovery'
                    AND stopped_attempt.state_kind = 'ended'
                    AND stopped_attempt.end_variant = 'after_cancellation'
                    AND stopped_attempt.end_disposition IN ('ambiguous', 'lost')
                )
           )
         WHERE existing.command_id = NEW.result_existing_interrupt_command_id
           AND existing.result_kind = 'applied'
           AND existing.rejection_kind IS NULL
           AND existing.delivery_kind = 'interrupt'
           AND existing.expected_active_turn_id = NEW.result_actual_active_turn_id
           AND NOT EXISTS (
                SELECT 1
                  FROM accepted_input
                 WHERE accepting_command_id = NEW.command_id
           );
    END IF;

    IF matching_records <> 1 THEN
        RAISE EXCEPTION
            'interrupt submit-input command % has an incomplete or cross-wired effect',
            NEW.command_id
            USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;

CREATE OR REPLACE FUNCTION require_goal_turn_shape()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    accepted accepted_input%ROWTYPE;
    queued queued_input_origin%ROWTYPE;
    defaults session_defaults_version%ROWTYPE;
    lifecycle turn_lifecycle%ROWTYPE;
    latest_event goal_event%ROWTYPE;
    source_event goal_event%ROWTYPE;
    predecessor turn_lifecycle%ROWTYPE;
    expected_content text;
BEGIN
    SELECT * INTO accepted FROM accepted_input
     WHERE accepted_input_id = NEW.accepted_input_id;
    SELECT * INTO queued FROM queued_input_origin
     WHERE turn_id = NEW.turn_id;
    SELECT * INTO defaults FROM session_defaults_version
     WHERE session_id = NEW.session_id
       AND version = queued.defaults_version;
    SELECT * INTO lifecycle FROM turn_lifecycle
     WHERE turn_id = NEW.turn_id;
    SELECT * INTO latest_event FROM goal_event
     WHERE session_id = NEW.session_id
     ORDER BY event_ordinal DESC LIMIT 1;

    IF accepted.accepted_input_id IS NULL
        OR accepted.session_id <> NEW.session_id
        OR accepted.delivery_kind <> 'start_when_no_active_turn'
        OR accepted.expected_active_turn_id IS NOT NULL
        OR accepted.expected_defaults_version IS NULL
        OR accepted.model_override_kind <> 'use_session_default'
        OR accepted.replacement_model_kind IS NOT NULL
        OR accepted.replacement_direct_model_selection_id IS NOT NULL
        OR accepted.replacement_model_alias_id IS NOT NULL
        OR accepted.disposition_kind <> 'origin_of'
        OR accepted.origin_turn_id <> NEW.turn_id
        OR queued.turn_id IS NULL
        OR queued.accepted_input_id <> NEW.accepted_input_id
        OR queued.session_id <> NEW.session_id
        OR queued.acceptance_position <> accepted.acceptance_position
        OR queued.priority_kind <> 'ordinary'
        OR queued.interrupt_predecessor_turn_id IS NOT NULL
        OR queued.source_configuration_turn_id IS NOT NULL
        OR defaults.session_id IS NULL
        OR accepted.expected_defaults_version <> queued.defaults_version
        OR queued.requested_model_kind <> defaults.model_selection_kind
        OR queued.requested_direct_model_selection_id
            IS DISTINCT FROM defaults.direct_model_selection_id
        OR queued.requested_model_alias_id
            IS DISTINCT FROM defaults.model_alias_id
        OR NOT (
            (queued.requested_model_kind = 'direct'
                AND queued.frozen_model_kind = 'direct'
                AND queued.frozen_direct_model_selection_id =
                    queued.requested_direct_model_selection_id)
            OR (queued.requested_model_kind = 'alias'
                AND queued.frozen_model_kind = 'frozen_alias'
                AND queued.frozen_model_alias_id = queued.requested_model_alias_id)
        )
        OR queued.model_parameters <> 'provider_defaults'
        OR queued.known_provider_failure_retry <> 'disabled'
        OR queued.model_fallback <> 'disabled'
        OR queued.dangerous_tool_auto_approval <>
            defaults.dangerous_tool_auto_approval
        OR lifecycle.turn_id IS NULL
        OR lifecycle.session_id <> NEW.session_id
        OR lifecycle.origin_accepted_input_id <> NEW.accepted_input_id
        OR lifecycle.acceptance_position <> accepted.acceptance_position
        OR lifecycle.state_kind <> 'queued'
    THEN
        RAISE EXCEPTION 'goal turn lacks its exact queued accepted-input shape'
            USING ERRCODE = '23514', CONSTRAINT = 'goal_turn_runtime_shape';
    END IF;

    IF latest_event.event_ordinal IS NULL
        OR (
            latest_event.event_kind = 'superseded'
            AND latest_event.generation + 1 <> NEW.goal_generation
        )
        OR (
            latest_event.event_kind <> 'superseded'
            AND latest_event.generation <> NEW.goal_generation
        )
        OR latest_event.event_kind NOT IN ('commissioned', 'resumed', 'superseded')
    THEN
        RAISE EXCEPTION 'goal turn requires the current pursuing generation'
            USING ERRCODE = '23514', CONSTRAINT = 'goal_turn_current_pursuit';
    END IF;

    IF NEW.source_event_ordinal IS NOT NULL THEN
        SELECT * INTO source_event FROM goal_event
         WHERE session_id = NEW.session_id
           AND event_ordinal = NEW.source_event_ordinal;
        IF source_event.event_kind NOT IN ('commissioned', 'resumed', 'superseded') THEN
            RAISE EXCEPTION 'first goal turn requires a pursuing user event'
                USING ERRCODE = '23514', CONSTRAINT = 'goal_turn_source_event';
        END IF;
        IF (
            source_event.event_kind = 'superseded'
            AND source_event.generation + 1 <> NEW.goal_generation
        ) OR (
            source_event.event_kind <> 'superseded'
            AND source_event.generation <> NEW.goal_generation
        ) THEN
            RAISE EXCEPTION
                'first goal turn generation disagrees with its user event'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'goal_turn_source_generation';
        END IF;
        IF source_event.event_kind = 'resumed' THEN
            IF source_event.guidance IS NOT NULL THEN
                expected_content := source_event.guidance;
            ELSE
                SELECT statement INTO expected_content FROM goal_event
                 WHERE session_id = NEW.session_id
                   AND event_ordinal <= NEW.source_event_ordinal
                   AND event_kind IN ('commissioned', 'superseded')
                 ORDER BY event_ordinal DESC LIMIT 1;
            END IF;
        ELSE
            expected_content := source_event.statement;
        END IF;
    ELSE
        SELECT * INTO predecessor FROM turn_lifecycle
         WHERE session_id = NEW.session_id
           AND turn_id = NEW.predecessor_turn_id;
        IF predecessor.state_kind <> 'terminal'
            OR predecessor.terminal_disposition_kind <> 'completed' THEN
            RAISE EXCEPTION
                'goal continuation requires a successfully completed predecessor'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'goal_turn_completed_predecessor';
        END IF;
        IF EXISTS (
            SELECT 1
              FROM goal_turn AS later_goal
              JOIN turn_lifecycle AS later
                ON later.session_id = later_goal.session_id
               AND later.turn_id = later_goal.turn_id
             WHERE later_goal.session_id = NEW.session_id
               AND later_goal.goal_generation = NEW.goal_generation
               AND later_goal.turn_id <> NEW.turn_id
               AND later.acceptance_position > predecessor.acceptance_position
        ) THEN
            RAISE EXCEPTION
                'goal continuation requires the latest accepted goal turn'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'goal_turn_latest_predecessor';
        END IF;
        SELECT statement INTO expected_content FROM goal_event
         WHERE session_id = NEW.session_id
           AND event_kind IN ('commissioned', 'superseded')
         ORDER BY event_ordinal DESC LIMIT 1;
    END IF;

    IF expected_content IS NULL
        OR (
            accepted.accepting_command_id IS NULL
            AND NOT EXISTS (
                SELECT 1
                  FROM accepted_input_content_part AS part
                 WHERE part.accepted_input_id = accepted.accepted_input_id
                   AND part.position = 0
                   AND part.part_kind = 'text'
                   AND part.text_value = expected_content
                   AND NOT EXISTS (
                        SELECT 1
                          FROM accepted_input_content_part AS extra
                         WHERE extra.accepted_input_id = accepted.accepted_input_id
                           AND extra.position <> 0
                   )
            )
        )
    THEN
        RAISE EXCEPTION 'goal turn input does not match its immutable source'
            USING ERRCODE = '23514', CONSTRAINT = 'goal_turn_input_content';
    END IF;
    RETURN NULL;
END;
$$;

-- Drain the deferred foreign-key events created by the one-time satellite
-- backfill before replacing the submit-input version constraints.
SET CONSTRAINTS ALL IMMEDIATE;
SET CONSTRAINTS ALL DEFERRED;

ALTER TABLE durable_command DISABLE TRIGGER USER;
ALTER TABLE submit_input_command DISABLE TRIGGER USER;

ALTER TABLE submit_input_command
    DROP CONSTRAINT submit_input_command_registry_fk;
ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_storage_version_supported;
ALTER TABLE submit_input_command
    DROP CONSTRAINT submit_input_command_storage_version_supported;

UPDATE durable_command SET storage_version = 3
 WHERE command_kind = 'submit_input';
UPDATE submit_input_command SET storage_version = 3;

SET CONSTRAINTS ALL IMMEDIATE;
SET CONSTRAINTS ALL DEFERRED;

ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_storage_version_supported CHECK (
        (command_kind = 'create_session' AND storage_version IN (1, 2, 3, 4, 5, 6, 7))
        OR (command_kind = 'replace_session_defaults' AND storage_version IN (1, 2, 3, 4))
        OR (command_kind = 'create_session_from_imported_frontier'
            AND storage_version IN (1, 2, 3, 5))
        OR (command_kind = 'submit_input' AND storage_version = 3)
        OR (command_kind IN (
            'replace_session_metadata', 'decide_tool_request',
            'review_workflow', 'review_orchestration', 'compact_session',
            'goal', 'update_session_placement', 'register_workspace',
            'mint_git_remote', 'withdraw_git_remote') AND storage_version = 1)
    );

ALTER TABLE submit_input_command
    ADD CONSTRAINT submit_input_command_storage_version_supported
        CHECK (storage_version = 3);
ALTER TABLE submit_input_command
    ADD CONSTRAINT submit_input_command_registry_fk
        FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command (command_id, command_kind, storage_version)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE durable_command ENABLE TRIGGER USER;
ALTER TABLE submit_input_command ENABLE TRIGGER USER;

ALTER TABLE submit_input_command
    DROP CONSTRAINT submit_input_command_content_kind_closed,
    DROP CONSTRAINT submit_input_command_content_nonempty,
    DROP CONSTRAINT submit_input_command_content_bounded;
ALTER TABLE accepted_input
    DROP CONSTRAINT accepted_input_content_kind_closed,
    DROP CONSTRAINT accepted_input_content_nonempty,
    DROP CONSTRAINT accepted_input_content_bounded;

ALTER TABLE submit_input_command
    DROP COLUMN content_kind,
    DROP COLUMN content_text;
ALTER TABLE accepted_input
    DROP COLUMN content_kind,
    DROP COLUMN content_text;

-- Retain the closed local cause of a guarded unsent attachment-preparation
-- failure separately from definitive provider-error evidence.
ALTER TABLE model_call
    ADD COLUMN terminal_attachment_preparation_failure_cause text,
    ADD COLUMN terminal_attachment_preparation_failure_maximum_bytes numeric(20, 0),
    ADD CONSTRAINT model_call_attachment_preparation_failure_cause_closed
        CHECK (
            terminal_attachment_preparation_failure_cause IS NULL
            OR terminal_attachment_preparation_failure_cause IN (
                'too_large', 'missing', 'corrupt'
            )
        ),
    ADD CONSTRAINT model_call_attachment_preparation_failure_maximum_bytes_u64
        CHECK (
            terminal_attachment_preparation_failure_maximum_bytes IS NULL
            OR (
                terminal_attachment_preparation_failure_maximum_bytes >= 1
                AND terminal_attachment_preparation_failure_maximum_bytes <= 18446744073709551615
            )
        ),
    ADD CONSTRAINT model_call_attachment_preparation_failure_cause_shape
        CHECK (
            (
                terminal_attachment_preparation_failure_cause IS NULL
                AND terminal_attachment_preparation_failure_maximum_bytes IS NULL
            )
            OR (
                state_kind = 'terminal'
                AND terminal_disposition_kind = 'known_failed'
                AND terminal_provider_failure_cause IS NULL
                AND (
                    (
                        terminal_attachment_preparation_failure_cause = 'too_large'
                        AND terminal_attachment_preparation_failure_maximum_bytes IS NOT NULL
                    )
                    OR (
                        terminal_attachment_preparation_failure_cause IN ('missing', 'corrupt')
                        AND terminal_attachment_preparation_failure_maximum_bytes IS NULL
                    )
                )
            )
        );
