-- Storage vocabulary for the human principal: owner becomes user.
--
-- The code spine has said "user" since the role rename: `Actor::User`,
-- `SessionCreationCause::UserInitiated`, and
-- `ToolApprovalDecisionSourceStorageKind::UserCommand`. Storage kept the older
-- spelling, so `crates/persistence/src/mapping.rs` translated one vocabulary
-- into the other at every encode and decode boundary, and
-- `scripts/check_user_vocabulary.py` carried a family of reviewed allowlist
-- entries whose entire justification was "the database still says owner". This
-- migration removes the split at its source, and those allowlist entries are
-- deleted in the same change so the checker enforces the new spelling.
--
-- No production database exists. That ruling is what lets this be a plain
-- rewrite: there is no dual-read window, no storage-version negotiation, and no
-- compatibility shim that a later change would have to retire. Developer and
-- dogfood databases, and any restored backup, upgrade in place by applying this
-- migration in the normal order. No external rewriter runs.
--
-- Applied migrations are never edited, so the files that created these objects
-- keep their original spelling and keep their reviewed allowlist entry. This
-- file is the last one that may legitimately name the retired spellings: it has
-- to name them in order to rename them.
--
-- Four "owner" names deliberately survive:
-- `imported_conversation_raw_record_owner_fk`,
-- `imported_transcript_entry_owner_fk`,
-- `imported_transcript_entry_owner_identity_key`, and
-- `review_pass_produced_finding_owner`. Their "owner" is the parent aggregate
-- that owns a child row -- the conversation that owns a transcript entry, the
-- pass that owns a finding -- and not the human principal. Renaming them to
-- "user" would assert something false about what the constraint enforces.
-- `docs/research/owner-user-rename-inventory-2026-07.md` classified exactly
-- these names the same way, as technical ownership rather than platform actor.

-- 1. The tool-approval decision column, and every object named after it.
--
-- Renaming the column rewrites the CHECK, foreign-key, and unique-index
-- expressions that mention it, because PostgreSQL stores those as parsed trees
-- and resolves them by column identity. It does not rewrite the objects' own
-- names, which are plain text, so each is renamed explicitly. Renaming a unique
-- constraint also renames the index backing it, so the index needs no separate
-- statement.

ALTER TABLE tool_approval_decision
    RENAME COLUMN owner_command_id TO user_command_id;

ALTER TABLE tool_approval_decision
    RENAME CONSTRAINT tool_approval_decision_owner_command_fk
                   TO tool_approval_decision_user_command_fk;

ALTER TABLE tool_approval_decision
    RENAME CONSTRAINT tool_approval_decision_owner_command_id_key
                   TO tool_approval_decision_user_command_id_key;

-- `owner_tool_approval_requires_command` is a CONSTRAINT TRIGGER, which is two
-- catalog rows under one name: a `pg_constraint` row and a `pg_trigger` row.
-- Neither rename statement touches both. `ALTER TABLE ... RENAME CONSTRAINT`
-- renames the constraint and leaves the trigger answering to the retired name;
-- `ALTER TRIGGER ... RENAME` does the exact reverse. Issuing only one of them
-- looks correct in whichever catalog you happen to inspect and leaves the old
-- spelling live in the other, so both are required, in this order.
ALTER TABLE tool_approval_decision
    RENAME CONSTRAINT owner_tool_approval_requires_command
                   TO user_tool_approval_requires_command;

ALTER TRIGGER owner_tool_approval_requires_command
    ON tool_approval_decision
    RENAME TO user_tool_approval_requires_command;

-- 2. Release the CHECK constraints that pin the retired stored values.
--
-- Every constraint below names at least one of `owner_initiated`,
-- `owner_command`, or the bare actor/issuer value `owner`. Each is dropped
-- here and recreated in section 4 over the new vocabulary; the rows themselves
-- are rewritten in between, which no constraint would permit while its old
-- literal is still the only admitted value.

ALTER TABLE session
    DROP CONSTRAINT session_creation_cause_closed,
    DROP CONSTRAINT session_delegated_cause_shape;

ALTER TABLE create_session_command
    DROP CONSTRAINT create_session_command_creation_cause_closed;

ALTER TABLE create_session_from_imported_frontier_command
    DROP CONSTRAINT create_session_from_imported_frontier_command_cause_closed;

ALTER TABLE session_metadata
    DROP CONSTRAINT session_metadata_actor_kind_closed,
    DROP CONSTRAINT session_metadata_actor_shape;

ALTER TABLE replace_session_metadata_command
    DROP CONSTRAINT replace_session_metadata_command_actor_kind_closed,
    DROP CONSTRAINT replace_session_metadata_command_actor_shape,
    DROP CONSTRAINT replace_session_metadata_command_issuer_shape,
    DROP CONSTRAINT replace_session_metadata_command_result_actor_kind_closed,
    DROP CONSTRAINT replace_session_metadata_command_result_actor_shape;

ALTER TABLE submit_input_command
    DROP CONSTRAINT submit_input_command_actor_kind_closed,
    DROP CONSTRAINT submit_input_command_actor_shape;

ALTER TABLE tool_approval_decision
    DROP CONSTRAINT tool_approval_decision_shape,
    DROP CONSTRAINT tool_approval_decision_source_closed,
    DROP CONSTRAINT tool_approval_decision_source_shape;

-- The two creation-provenance foreign keys carry `creation_cause` inside the
-- key itself, so rewriting the value changes the referenced key rather than
-- merely a column beside it. Both are `ON UPDATE RESTRICT`, and RESTRICT is
-- checked by an internal trigger that `DISABLE TRIGGER USER` does not touch and
-- that PostgreSQL never defers -- declaring the imported-frontier key
-- `DEFERRABLE INITIALLY DEFERRED` does not change that, because RESTRICT is
-- specified to fire immediately regardless. On a populated database the first
-- session that has a creation command would abort the rewrite before either
-- side could move. Releasing both keys is the only way to migrate the parent
-- and the child together; section 4 restores them, which revalidates every row
-- against the rewritten vocabulary.

ALTER TABLE create_session_command
    DROP CONSTRAINT create_session_command_provenance_fk;

ALTER TABLE create_session_from_imported_frontier_command
    DROP CONSTRAINT create_session_from_imported_frontier_command_provenance_fk;

-- 3. Rewrite the stored values.
--
-- Each of these tables is append-only, enforced by a BEFORE UPDATE OR DELETE
-- trigger that raises unconditionally, so the rewrite has to run with user
-- triggers disabled. `DISABLE TRIGGER USER` is deliberate in preference to
-- `session_replication_role = replica`: it needs only table ownership, which
-- the migration role already has, where the session setting needs superuser and
-- would silently suppress foreign-key enforcement across the whole statement.
--
-- Disabling covers the constraint triggers as well, which is what makes the
-- rewrite possible at all -- `user_tool_approval_requires_command` and the
-- session-creation guard would otherwise re-derive their invariants against
-- functions that section 5 has not yet replaced.
--
-- A fresh database reaches this point with no rows and every statement is a
-- no-op. A populated one is upgraded in place, which is the entire reason this
-- is a migration rather than an edit to the files that created the objects.

ALTER TABLE session DISABLE TRIGGER USER;
ALTER TABLE create_session_command DISABLE TRIGGER USER;
ALTER TABLE create_session_from_imported_frontier_command DISABLE TRIGGER USER;
ALTER TABLE session_metadata DISABLE TRIGGER USER;
ALTER TABLE replace_session_metadata_command DISABLE TRIGGER USER;
ALTER TABLE submit_input_command DISABLE TRIGGER USER;
ALTER TABLE tool_approval_decision DISABLE TRIGGER USER;

UPDATE session
   SET creation_cause = 'user_initiated'
 WHERE creation_cause = 'owner_initiated';

UPDATE create_session_command
   SET creation_cause = 'user_initiated'
 WHERE creation_cause = 'owner_initiated';

UPDATE create_session_from_imported_frontier_command
   SET creation_cause = 'user_initiated'
 WHERE creation_cause = 'owner_initiated';

UPDATE session_metadata
   SET actor_kind = 'user'
 WHERE actor_kind = 'owner';

-- `replace_session_metadata_command_result_shape` requires an applied receipt's
-- `result_actor_kind` to equal its `actor_kind`. That constraint names no
-- retired value, so section 2 does not release it and it stays in force here:
-- rewriting the two columns in separate statements would leave the row
-- disagreeing with itself between them and raise a check violation on the
-- first. One statement moves every correlated actor field at once. `issuer_kind`
-- joins them because it is the same row and the same rewrite, not because that
-- constraint reads it.

UPDATE replace_session_metadata_command
   SET actor_kind =
           CASE WHEN actor_kind = 'owner' THEN 'user' ELSE actor_kind END,
       result_actor_kind =
           CASE
               WHEN result_actor_kind = 'owner' THEN 'user'
               ELSE result_actor_kind
           END,
       issuer_kind =
           CASE WHEN issuer_kind = 'owner' THEN 'user' ELSE issuer_kind END
 WHERE actor_kind = 'owner'
    OR result_actor_kind = 'owner'
    OR issuer_kind = 'owner';

UPDATE submit_input_command
   SET actor_kind = 'user'
 WHERE actor_kind = 'owner';

UPDATE tool_approval_decision
   SET decision_source = 'user_command'
 WHERE decision_source = 'owner_command';

ALTER TABLE session ENABLE TRIGGER USER;
ALTER TABLE create_session_command ENABLE TRIGGER USER;
ALTER TABLE create_session_from_imported_frontier_command ENABLE TRIGGER USER;
ALTER TABLE session_metadata ENABLE TRIGGER USER;
ALTER TABLE replace_session_metadata_command ENABLE TRIGGER USER;
ALTER TABLE submit_input_command ENABLE TRIGGER USER;
ALTER TABLE tool_approval_decision ENABLE TRIGGER USER;

-- 4. Recreate the released constraints over the new vocabulary.
--
-- These are the section 2 constraints with their retired literals replaced and
-- nothing else changed. The admitted shapes, the null-column pairings, and the
-- byte bounds are all carried over exactly, so the only difference a reader
-- should find against the originating migrations is the spelling.

ALTER TABLE session
    ADD CONSTRAINT session_creation_cause_closed
        CHECK (creation_cause IN ('user_initiated', 'delegated')),
    ADD CONSTRAINT session_delegated_cause_shape
        CHECK (
            (
                creation_cause = 'user_initiated'
                AND spawning_tool_request_id IS NULL
            )
            OR (
                creation_cause = 'delegated'
                AND ancestry_kind = 'none'
                AND spawning_tool_request_id IS NOT NULL
            )
        );

ALTER TABLE create_session_command
    ADD CONSTRAINT create_session_command_creation_cause_closed
        CHECK (creation_cause = 'user_initiated');

ALTER TABLE create_session_from_imported_frontier_command
    ADD CONSTRAINT create_session_from_imported_frontier_command_cause_closed
        CHECK (creation_cause = 'user_initiated');

ALTER TABLE session_metadata
    ADD CONSTRAINT session_metadata_actor_kind_closed
        CHECK (actor_kind IN ('user', 'model', 'recovery', 'tool')),
    ADD CONSTRAINT session_metadata_actor_shape
        CHECK (
            (
                actor_kind IN ('user', 'recovery')
                AND actor_turn_id IS NULL
                AND actor_tool_request_id IS NULL
            )
            OR (
                actor_kind = 'model'
                AND actor_turn_id IS NOT NULL
                AND actor_tool_request_id IS NULL
            )
            OR (
                actor_kind = 'tool'
                AND actor_turn_id IS NULL
                AND actor_tool_request_id IS NOT NULL
            )
        );

ALTER TABLE replace_session_metadata_command
    ADD CONSTRAINT replace_session_metadata_command_actor_kind_closed
        CHECK (actor_kind IN ('user', 'model', 'recovery', 'tool')),
    ADD CONSTRAINT replace_session_metadata_command_actor_shape
        CHECK (
            (
                actor_kind IN ('user', 'recovery')
                AND actor_turn_id IS NULL
                AND actor_tool_request_id IS NULL
            )
            OR (
                actor_kind = 'model'
                AND actor_turn_id IS NOT NULL
                AND actor_tool_request_id IS NULL
            )
            OR (
                actor_kind = 'tool'
                AND actor_turn_id IS NULL
                AND actor_tool_request_id IS NOT NULL
            )
        ),
    ADD CONSTRAINT replace_session_metadata_command_issuer_shape
        CHECK (
            (
                issuer_kind = 'user'
                AND issuer_tool_request_id IS NULL
            )
            OR (
                issuer_kind = 'tool'
                AND issuer_tool_request_id IS NOT NULL
            )
        ),
    ADD CONSTRAINT replace_session_metadata_command_result_actor_kind_closed
        CHECK (
            result_actor_kind IS NULL
            OR result_actor_kind IN ('user', 'model', 'recovery', 'tool')
        ),
    ADD CONSTRAINT replace_session_metadata_command_result_actor_shape
        CHECK (
            (
                result_actor_kind IS NULL
                AND result_actor_turn_id IS NULL
                AND result_actor_tool_request_id IS NULL
            )
            OR (
                result_actor_kind IN ('user', 'recovery')
                AND result_actor_turn_id IS NULL
                AND result_actor_tool_request_id IS NULL
            )
            OR (
                result_actor_kind = 'model'
                AND result_actor_turn_id IS NOT NULL
                AND result_actor_tool_request_id IS NULL
            )
            OR (
                result_actor_kind = 'tool'
                AND result_actor_turn_id IS NULL
                AND result_actor_tool_request_id IS NOT NULL
            )
        );

ALTER TABLE submit_input_command
    ADD CONSTRAINT submit_input_command_actor_kind_closed
        CHECK (actor_kind IN ('user', 'model', 'recovery', 'tool')),
    ADD CONSTRAINT submit_input_command_actor_shape
        CHECK (
            (
                actor_kind IN ('user', 'recovery')
                AND actor_turn_id IS NULL
                AND actor_tool_request_id IS NULL
            )
            OR (
                actor_kind = 'model'
                AND actor_turn_id IS NOT NULL
                AND actor_tool_request_id IS NULL
            )
            OR (
                actor_kind = 'tool'
                AND actor_turn_id IS NULL
                AND actor_tool_request_id IS NOT NULL
            )
        );

ALTER TABLE tool_approval_decision
    ADD CONSTRAINT tool_approval_decision_shape
        CHECK (
            (
                decision_kind = 'approve'
                AND denial_reason IS NULL
            )
            OR (
                decision_kind = 'deny'
                AND decision_source = 'user_command'
                AND (
                    denial_reason IS NULL
                    OR (
                        octet_length(denial_reason) BETWEEN 1 AND 1024
                        AND denial_reason !~ '[[:cntrl:]]'
                        AND denial_reason !~ '^[[:space:]]'
                        AND denial_reason !~ '[[:space:]]$'
                    )
                )
            )
            OR (
                decision_kind = 'deny'
                AND decision_source = 'delegate'
                AND denial_reason IS NULL
            )
        ),
    ADD CONSTRAINT tool_approval_decision_source_closed
        CHECK (
            decision_source IN (
                'user_command',
                'policy_auto',
                'session_blanket',
                'delegate'
            )
        ),
    ADD CONSTRAINT tool_approval_decision_source_shape
        CHECK (
            (
                decision_source = 'user_command'
                AND user_command_id IS NOT NULL
                AND delegate_model_selection_id IS NULL
                AND delegate_model_call_id IS NULL
                AND rationale IS NULL
            )
            OR (
                decision_source IN ('policy_auto', 'session_blanket')
                AND decision_kind = 'approve'
                AND user_command_id IS NULL
                AND delegate_model_selection_id IS NULL
                AND delegate_model_call_id IS NULL
                AND rationale IS NULL
            )
            OR (
                decision_source = 'delegate'
                AND user_command_id IS NULL
                AND delegate_model_selection_id IS NOT NULL
                AND delegate_model_call_id IS NOT NULL
                AND rationale IS NOT NULL
                AND octet_length(rationale) BETWEEN 1 AND 4096
            )
        );

-- Restore the creation-provenance keys released in section 2, in their
-- originating shape. Adding a foreign key validates every existing row, so this
-- is also the proof that the parent and child vocabularies were rewritten
-- consistently: a session whose cause moved without its command, or the
-- reverse, fails here rather than surviving as a silently unreferenced row.

ALTER TABLE create_session_command
    ADD CONSTRAINT create_session_command_provenance_fk
        FOREIGN KEY (created_session_id, creation_cause, ancestry_kind)
        REFERENCES session (session_id, creation_cause, ancestry_kind)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT;

ALTER TABLE create_session_from_imported_frontier_command
    ADD CONSTRAINT create_session_from_imported_frontier_command_provenance_fk
        FOREIGN KEY (
            created_session_id,
            creation_cause,
            ancestry_kind,
            imported_conversation_id,
            imported_frontier_entry_id,
            imported_frontier_position,
            imported_relationship_kind
        )
        REFERENCES session (
            session_id,
            creation_cause,
            ancestry_kind,
            imported_conversation_id,
            imported_frontier_entry_id,
            imported_frontier_position,
            imported_relationship_kind
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

-- 5. Replace the trigger functions that name the retired vocabulary.
--
-- A PL/pgSQL body is stored as text and resolved when it runs, so the column
-- rename in section 1 does not reach into it: left alone, every one of these
-- would fail at runtime against a column that no longer exists, or would
-- silently compare `decision_source` against a value that can no longer be
-- stored. Each function below is its current definition with the retired
-- spellings replaced and no other change.

-- Reads the approval row by its renamed column and matches the renamed decision source.

CREATE OR REPLACE FUNCTION assert_tool_decision_command_final_state(
    checked_command_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    command_record decide_tool_request_command%ROWTYPE;
    approval_count bigint;
    earliest_correlation_count bigint;
BEGIN
    SELECT *
      INTO command_record
      FROM decide_tool_request_command
     WHERE command_id = checked_command_id;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT count(*)
      INTO approval_count
      FROM tool_approval_decision AS approval
     WHERE approval.user_command_id = checked_command_id
       AND approval.request_id = command_record.request_id
       AND approval.decision_source = 'user_command'
       AND approval.decision_kind = command_record.decision_kind
       AND approval.denial_reason
           IS NOT DISTINCT FROM command_record.denial_reason;

    SELECT count(*)
      INTO earliest_correlation_count
      FROM tool_request AS requested
      JOIN tool_request AS earliest
        ON earliest.request_id =
           command_record.result_earliest_undecided_request_id
       AND earliest.producing_model_call_id =
           requested.producing_model_call_id
       AND earliest.request_ordinal < requested.request_ordinal
     WHERE requested.request_id = command_record.request_id;

    IF command_record.rejection_kind = 'not_earliest_undecided'
       AND earliest_correlation_count <> 1
    THEN
        RAISE EXCEPTION
            'tool decision command names an uncorrelated earlier request'
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'decide_tool_request_command_earliest_correlation';
    END IF;

    IF (
        command_record.result_kind = 'applied'
        AND approval_count <> 1
    ) OR (
        command_record.result_kind = 'rejected'
        AND EXISTS (
            SELECT 1
              FROM tool_approval_decision
             WHERE user_command_id = checked_command_id
        )
    ) THEN
        RAISE EXCEPTION
            'tool decision command lacks its exact approval effect'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

-- Passes the renamed column through to the assertion above.

CREATE OR REPLACE FUNCTION require_tool_decision_command_final_state()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_TABLE_NAME = 'decide_tool_request_command' THEN
        PERFORM assert_tool_decision_command_final_state(NEW.command_id);
    ELSE
        PERFORM assert_tool_decision_command_final_state(NEW.user_command_id);
    END IF;
    RETURN NULL;
END;
$$;

-- Branches on the renamed decision source when checking approval authority.

CREATE OR REPLACE FUNCTION require_tool_approval_decision_authority()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    matched bigint;
BEGIN
    PERFORM 1
       FROM tool_request
      WHERE request_id = NEW.request_id
        FOR UPDATE;
    IF EXISTS (
        SELECT 1
          FROM tool_approval_judge_model_call AS judge
         WHERE judge.request_id = NEW.request_id
           AND judge.state_kind <> 'terminal'
    ) THEN
        RAISE EXCEPTION 'approval decision races an unfinished judge call'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      'tool_approval_decision_requires_terminal_judge';
    END IF;
    IF NEW.decision_source IN ('policy_auto', 'session_blanket') THEN
        SELECT count(*) INTO matched
          FROM tool_request
         WHERE request_id = NEW.request_id
           AND approval_posture = 'auto';
        IF matched <> 1 THEN
            RAISE EXCEPTION 'automatic decision exceeds frozen posture'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'tool_approval_automatic_requires_auto_posture';
        END IF;
        RETURN NULL;
    END IF;
    IF NEW.decision_source = 'user_command' THEN
        SELECT count(*) INTO matched
          FROM tool_request AS request
         WHERE request.request_id = NEW.request_id
           AND (
                request.approval_posture = 'human'
                OR (
                    request.approval_posture = 'delegated'
                    AND EXISTS (
                        SELECT 1
                          FROM tool_approval_judge_model_call AS judge
                         WHERE judge.request_id = request.request_id
                           AND judge.state_kind = 'terminal'
                           AND (
                                (
                                    judge.terminal_disposition_kind = 'completed'
                                    AND judge.recommendation_kind =
                                        'escalate_to_human'
                                )
                                OR judge.terminal_disposition_kind IN (
                                    'known_failed', 'refused', 'cancelled',
                                    'ambiguous'
                                )
                           )
                    )
                )
           );
        IF matched <> 1 THEN
            RAISE EXCEPTION 'user decision lacks human approval authority'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'tool_approval_user_requires_human_authority';
        END IF;
        RETURN NULL;
    END IF;
    IF NEW.decision_source <> 'delegate' THEN
        RETURN NULL;
    END IF;
    SELECT count(*) INTO matched
      FROM tool_request AS request
      JOIN tool_approval_judge_model_call AS judge
        ON judge.request_id = request.request_id
     WHERE request.request_id = NEW.request_id
       AND request.approval_posture = 'delegated'
       AND judge.model_call_id = NEW.delegate_model_call_id
       AND judge.direct_model_selection_id = NEW.delegate_model_selection_id
       AND judge.state_kind = 'terminal'
       AND judge.terminal_disposition_kind = 'completed'
       AND judge.recommendation_kind = NEW.decision_kind
       AND judge.rationale = NEW.rationale
       AND NOT EXISTS (
            SELECT 1 FROM tool_request AS earlier
            LEFT JOIN tool_approval_decision AS earlier_decision
              ON earlier_decision.request_id = earlier.request_id
           WHERE earlier.producing_model_call_id = request.producing_model_call_id
             AND earlier.request_ordinal < request.request_ordinal
             AND earlier_decision.request_id IS NULL
       );
    IF matched <> 1 THEN
        RAISE EXCEPTION 'delegate decision lacks matching delegated authority'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'tool_approval_delegate_requires_checked_judge';
    END IF;
    RETURN NULL;
END;
$$;

-- Admits the renamed decision source where a lease generation may be approved.

CREATE OR REPLACE FUNCTION guard_runner_lease_generation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    placement runner_session_placement_record%ROWTYPE;
    enrollment_state text;
    attempted_tool text;
    attempted_effect text;
    attempted_state text;
    attempted_request uuid;
    current_registration_revision numeric;
    current_registration_runner uuid;
    registered_effect text;
    registered_permission text;
    bound_lease uuid;
    bound_request_lease uuid;
    prior runner_lease_generation%ROWTYPE;
    prior_state text;
    prior_request uuid;
    grant_state text;
BEGIN
    SELECT record.* INTO placement
      FROM runner_current_session_placement AS current_placement
      JOIN runner_session_placement_record AS record
        ON record.session_id = current_placement.session_id
       AND record.event_ordinal = current_placement.event_ordinal
     WHERE current_placement.session_id = NEW.session_id
       FOR SHARE OF current_placement;
    SELECT state_kind INTO enrollment_state
      FROM runner_enrollment
     WHERE enrollment_id = NEW.registration_enrollment_id
       FOR SHARE;
    SELECT request.tool_name, attempt.effect_class, attempt.state_kind,
           attempt.request_id
      INTO attempted_tool, attempted_effect, attempted_state, attempted_request
      FROM tool_attempt AS attempt
      JOIN tool_request AS request
        ON request.request_id = attempt.request_id
     WHERE attempt.attempt_id = NEW.attempt_id
       AND attempt.session_id = NEW.session_id
       FOR UPDATE OF attempt;
    SELECT current_registration.registration_revision,
           registration.runner_id,
           registered.effect_class,
           registered.permission_kind
      INTO current_registration_revision,
           current_registration_runner,
           registered_effect,
           registered_permission
      FROM runner_current_registration AS current_registration
      JOIN runner_registration AS registration
        ON registration.enrollment_id =
            current_registration.enrollment_id
       AND registration.registration_revision =
            current_registration.registration_revision
      JOIN runner_registration_tool AS registered
        ON registered.enrollment_id =
            current_registration.enrollment_id
       AND registered.registration_revision =
            current_registration.registration_revision
     WHERE current_registration.enrollment_id =
            NEW.registration_enrollment_id
       AND registered.tool_name = NEW.tool_name
       FOR SHARE OF current_registration;
    IF NEW.credential_grant_revision IS NOT NULL THEN
        SELECT event_kind INTO grant_state
          FROM runner_current_credential_grant_audit
         WHERE session_id = NEW.session_id
           AND lineage_origin_event_ordinal =
                NEW.credential_grant_lineage_origin_ordinal
           AND runner_id = NEW.runner_id
           AND grant_revision = NEW.credential_grant_revision
         FOR SHARE;
    END IF;
    INSERT INTO runner_tool_request_lease_binding
        (request_id, lease_id)
    VALUES (attempted_request, NEW.lease_id)
    ON CONFLICT (request_id) DO NOTHING;
    SELECT lease_id INTO bound_request_lease
      FROM runner_tool_request_lease_binding
     WHERE request_id = attempted_request;
    INSERT INTO runner_physical_attempt_lease_binding
        (attempt_id, lease_id)
    VALUES (NEW.attempt_id, NEW.lease_id)
    ON CONFLICT (attempt_id) DO NOTHING;
    SELECT lease_id INTO bound_lease
      FROM runner_physical_attempt_lease_binding
     WHERE attempt_id = NEW.attempt_id;
    IF registered_effect IS NULL
       OR attempted_request IS NULL
       OR bound_request_lease IS DISTINCT FROM NEW.lease_id
       OR bound_lease IS DISTINCT FROM NEW.lease_id
       OR placement.state_kind IS DISTINCT FROM 'pinned'
       OR placement.event_ordinal IS DISTINCT FROM
            NEW.placement_event_ordinal
       OR placement.pinned_runner_id IS DISTINCT FROM NEW.runner_id
       OR placement.registration_enrollment_id IS DISTINCT FROM
            NEW.registration_enrollment_id
       OR placement.registration_revision IS DISTINCT FROM
            NEW.registration_revision
       OR placement.pinned_credential_profile_name IS DISTINCT FROM
            NEW.credential_profile_name
       OR (
            NEW.credential_profile_name IS NOT NULL
            AND (
                placement.credential_grant_lineage_origin_ordinal IS DISTINCT FROM
                    NEW.credential_grant_lineage_origin_ordinal
                OR placement.credential_grant_revision IS DISTINCT FROM
                    NEW.credential_grant_revision
            )
       )
       OR (
            NEW.credential_profile_name IS NULL
            AND NEW.credential_grant_revision IS NOT NULL
       )
       OR current_registration_runner IS DISTINCT FROM NEW.runner_id
       OR (
            placement.selector_kind = 'identity'
            AND placement.selector_runner_id IS DISTINCT FROM
                current_registration_runner
       )
       OR (
            placement.selector_kind = 'capability_class'
            AND NOT EXISTS (
                SELECT 1
                  FROM runner_registration_class
                 WHERE enrollment_id =
                    NEW.registration_enrollment_id
                   AND registration_revision =
                    current_registration_revision
                   AND capability_class =
                    placement.selector_capability_class
            )
       )
       OR EXISTS (
            SELECT 1
              FROM runner_session_placement_tool AS required
             WHERE required.session_id = placement.session_id
               AND required.event_ordinal = placement.event_ordinal
               AND required.runner_required
               AND NOT EXISTS (
                    SELECT 1
                      FROM runner_registration_tool AS available
                     WHERE available.enrollment_id =
                        NEW.registration_enrollment_id
                       AND available.registration_revision =
                        current_registration_revision
                       AND available.tool_name = required.tool_name
               )
       )
       OR (
            placement.pinned_credential_profile_name IS NOT NULL
            AND NOT EXISTS (
                SELECT 1
                  FROM runner_registration_profile
                 WHERE enrollment_id =
                    NEW.registration_enrollment_id
                   AND registration_revision =
                    current_registration_revision
                   AND credential_profile_name =
                    placement.pinned_credential_profile_name
            )
       )
       OR (
            placement.workspace_requirement_kind =
                'repository_worktree'
            AND NOT EXISTS (
                SELECT 1
                  FROM runner_registration_workspace
                 WHERE enrollment_id =
                    NEW.registration_enrollment_id
                   AND registration_revision =
                    current_registration_revision
                   AND workspace_kind = 'worktree_per_session'
            )
       )
       OR enrollment_state IS DISTINCT FROM 'active'
       OR attempted_tool IS DISTINCT FROM NEW.tool_name
       OR attempted_state IS DISTINCT FROM 'in_flight'
       OR registered_effect IS DISTINCT FROM NEW.effect_class
       OR (
            NEW.effect_class = 'pure'
            AND attempted_effect <> 'effect_free'
       )
       OR (
            NEW.effect_class IN ('idempotent', 'side_effecting')
            AND attempted_effect <> 'external_effect'
       )
    THEN
        RAISE EXCEPTION 'runner lease offer is not canonically authorized'
            USING ERRCODE = '23514';
    END IF;
    -- A session-policy tool/profile pair requires confirmation: only an
    -- user-command decision or the frozen session blanket may approve the
    -- request this lease dispatches. Policy-auto provenance would bypass the
    -- confirmation the pair posture records.
    IF NEW.credential_approval_kind = 'session_policy'
       AND NOT EXISTS (
            SELECT 1
              FROM tool_approval_decision AS approval
             WHERE approval.request_id = attempted_request
               AND approval.decision_kind = 'approve'
               AND approval.decision_source
                    IN ('user_command', 'session_blanket')
       )
    THEN
        RAISE EXCEPTION
            'session-policy lease admission requires confirmed approval provenance'
            USING ERRCODE = '23514';
    END IF;
    -- A profileless Confirm declaration accepts only a user-command
    -- decision or the frozen session blanket. Policy-auto provenance would
    -- bypass the confirmation the daemon-authoritative declaration records.
    IF NEW.credential_profile_name IS NULL
       AND registered_permission = 'confirm'
       AND NOT EXISTS (
            SELECT 1
              FROM tool_approval_decision AS approval
             WHERE approval.request_id = attempted_request
               AND approval.decision_kind = 'approve'
               AND approval.decision_source
                    IN ('user_command', 'session_blanket')
       )
    THEN
        RAISE EXCEPTION
            'profileless confirm lease admission requires confirmed approval provenance'
            USING ERRCODE = '23514';
    END IF;
    IF EXISTS (
        SELECT 1
          FROM runner_lease_generation AS previous
          JOIN runner_current_lease_event AS current_event
            ON current_event.lease_id = previous.lease_id
           AND current_event.generation = previous.generation
          JOIN runner_lease_event AS event
            ON event.lease_id = current_event.lease_id
           AND event.generation = current_event.generation
           AND event.event_ordinal = current_event.event_ordinal
         WHERE previous.lease_id = NEW.lease_id
           AND previous.generation < NEW.generation
           AND previous.attempt_id = NEW.attempt_id
           AND event.state_kind IN ('lost_execution_possible', 'lost_claimed', 'completed')
    ) THEN
        RAISE EXCEPTION 'claimed physical attempt cannot be reused'
            USING ERRCODE = '23514';
    END IF;
    IF EXISTS (
        SELECT 1
          FROM runner_lease_generation AS existing
         WHERE existing.attempt_id = NEW.attempt_id
           AND existing.lease_id <> NEW.lease_id
    ) THEN
        RAISE EXCEPTION 'physical attempt is already bound to another lease'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.credential_grant_revision IS NOT NULL
       AND grant_state NOT IN ('issued', 'replaced')
    THEN
        RAISE EXCEPTION 'revoked credential grant cannot authorize a lease'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.generation > 1 THEN
        SELECT * INTO prior
          FROM runner_lease_generation
         WHERE lease_id = NEW.lease_id
           AND generation = NEW.predecessor_generation;
        SELECT event.state_kind INTO prior_state
          FROM runner_current_lease_event AS current_event
          JOIN runner_lease_event AS event
            ON event.lease_id = current_event.lease_id
           AND event.generation = current_event.generation
           AND event.event_ordinal = current_event.event_ordinal
         WHERE current_event.lease_id = NEW.lease_id
           AND current_event.generation = NEW.predecessor_generation;
        SELECT attempt.request_id INTO prior_request
          FROM tool_attempt AS attempt
         WHERE attempt.attempt_id = prior.attempt_id;
        IF NOT FOUND
           OR prior_state IS NULL
           OR prior_state NOT IN ('lost_unclaimed', 'lost_execution_possible', 'lost_claimed')
           OR ROW(
                prior.session_id,
                prior.runner_id,
                prior.tool_name,
                prior.effect_class,
                prior.credential_profile_name,
                prior.credential_grant_lineage_origin_ordinal,
                prior.credential_grant_revision,
                prior.credential_approval_kind
           ) IS DISTINCT FROM ROW(
                NEW.session_id,
                NEW.runner_id,
                NEW.tool_name,
                NEW.effect_class,
                NEW.credential_profile_name,
                NEW.credential_grant_lineage_origin_ordinal,
                NEW.credential_grant_revision,
                NEW.credential_approval_kind
           )
           OR (
                prior_state = 'lost_unclaimed'
                AND prior.attempt_id <> NEW.attempt_id
           )
           OR (
                prior_state IN ('lost_execution_possible', 'lost_claimed')
                AND (
                    prior.effect_class = 'side_effecting'
                    OR prior.attempt_id = NEW.attempt_id
                    OR prior_request IS DISTINCT FROM attempted_request
                    OR NOT EXISTS (
                        SELECT 1
                          FROM runner_claimed_retry_attempt_authority AS authority
                         WHERE authority.source_lease_id = prior.lease_id
                           AND authority.source_generation = prior.generation
                           AND authority.replacement_attempt_id = NEW.attempt_id
                    )
                )
           )
        THEN
            RAISE EXCEPTION 'runner lease retry violates durable effect law'
                USING ERRCODE = '23514';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

-- Excludes the renamed decision source from wire-approved lease placement.

CREATE OR REPLACE FUNCTION guard_runner_wire_lease_approval()
RETURNS trigger
LANGUAGE plpgsql
AS $function$
DECLARE
    effective_approval text;
    decision_source text;
BEGIN
    SELECT
        CASE
            WHEN override_record.permission_kind = 'auto'
                THEN 'automatic'
            WHEN override_record.permission_kind = 'confirm'
                THEN 'session_policy'
            WHEN placement.requested_sandbox_profile = 'workspace_restricted'
                THEN 'automatic'
            WHEN registered.effect_class = 'pure'
                THEN 'automatic'
            ELSE 'session_policy'
        END,
        approval.decision_source
      INTO effective_approval, decision_source
      FROM runner_session_placement_record AS placement
      JOIN runner_registration_tool AS registered
        ON registered.enrollment_id = placement.registration_enrollment_id
       AND registered.registration_revision = placement.registration_revision
       AND registered.tool_name = NEW.tool_name
      JOIN tool_attempt AS attempt
        ON attempt.attempt_id = NEW.attempt_id
      LEFT JOIN runner_session_placement_permission_override AS override_record
        ON override_record.session_id = placement.session_id
       AND override_record.event_ordinal = placement.event_ordinal
       AND override_record.tool_name = NEW.tool_name
      LEFT JOIN tool_approval_decision AS approval
        ON approval.request_id = attempt.request_id
       AND approval.decision_kind = 'approve'
     WHERE placement.session_id = NEW.session_id
       AND placement.event_ordinal = NEW.placement_event_ordinal;
    IF NOT FOUND
       OR decision_source = 'session_blanket'
       OR (
            effective_approval = 'session_policy'
            AND decision_source IS DISTINCT FROM 'user_command'
       )
       OR (
            NEW.credential_profile_name IS NOT NULL
            AND NEW.credential_approval_kind IS DISTINCT FROM effective_approval
       )
    THEN
        RAISE EXCEPTION 'runner lease approval is not placement-authorized'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$function$;

-- Matches the renamed creation cause when correlating a session to its command.

CREATE OR REPLACE FUNCTION require_session_creation_command()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE native_count bigint; imported_count bigint; delegated_count bigint;
BEGIN
    SELECT count(*) INTO native_count FROM create_session_command
     WHERE created_session_id = NEW.session_id;
    SELECT count(*) INTO imported_count FROM create_session_from_imported_frontier_command
     WHERE created_session_id = NEW.session_id;
    SELECT count(*) INTO delegated_count FROM session_delegation
     WHERE child_session_id = NEW.session_id;
    IF (NEW.creation_cause = 'user_initiated' AND NEW.ancestry_kind = 'none'
            AND (native_count, imported_count, delegated_count) <> (1, 0, 0))
        OR (NEW.creation_cause = 'user_initiated' AND NEW.ancestry_kind = 'imported_conversation'
            AND (native_count, imported_count, delegated_count) <> (0, 1, 0))
        OR (NEW.creation_cause = 'delegated'
            AND (native_count, imported_count, delegated_count) <> (0, 0, 1)) THEN
        RAISE EXCEPTION 'session % requires exactly one matching creation family', NEW.session_id
            USING ERRCODE = '23503', CONSTRAINT = 'session_requires_creation_command';
    END IF;
    RETURN NULL;
END;
$$;
