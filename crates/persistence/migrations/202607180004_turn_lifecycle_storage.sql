-- Relational storage foundation for accepted-input turn lifecycle.
--
-- This migration deliberately adds no activation procedure. Existing and new
-- accepted origin work is represented as queued. Future eligibility handling
-- may move one row to active only by committing its semantic origin entry,
-- complete immutable starting frontier, fixed lineage, and initial prepared
-- attempt together.

CREATE TABLE session_scheduler (
    session_id uuid PRIMARY KEY,

    CONSTRAINT session_scheduler_session_fk
        FOREIGN KEY (session_id)
        REFERENCES session (session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

INSERT INTO session_scheduler (session_id)
SELECT session_id
  FROM session;

ALTER TABLE session
    ADD CONSTRAINT session_scheduler_row_fk
    FOREIGN KEY (session_id)
    REFERENCES session_scheduler (session_id)
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;

CREATE TRIGGER session_scheduler_is_append_only
BEFORE UPDATE OR DELETE ON session_scheduler
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

-- Semantic-entry source correlation needs the accepted input and its session
-- as one foreign-key target. The accepted-input identity remains globally
-- unique; this redundant key makes the ownership check declarative.
ALTER TABLE accepted_input
    ADD CONSTRAINT accepted_input_source_session_key
    UNIQUE (accepted_input_id, session_id);

CREATE TABLE turn_lifecycle (
    turn_id uuid PRIMARY KEY,
    session_id uuid NOT NULL,
    origin_accepted_input_id uuid NOT NULL UNIQUE,
    acceptance_position numeric(20, 0) NOT NULL,
    attempt_history_present boolean NOT NULL DEFAULT false,
    state_kind text NOT NULL,
    start_lineage_kind text,
    immediate_predecessor_turn_id uuid,
    starting_frontier_id uuid,
    terminal_frontier_id uuid,
    active_phase_kind text,
    current_attempt_id uuid,
    terminal_disposition_kind text,

    CONSTRAINT turn_lifecycle_position_positive_u64
        CHECK (
            acceptance_position >= 1
            AND acceptance_position <= 18446744073709551615
        ),
    CONSTRAINT turn_lifecycle_state_kind_closed
        CHECK (state_kind IN ('queued', 'active', 'terminal')),
    CONSTRAINT turn_lifecycle_lineage_kind_closed
        CHECK (
            start_lineage_kind IS NULL
            OR start_lineage_kind IN ('first_in_session', 'after')
        ),
    CONSTRAINT turn_lifecycle_lineage_shape
        CHECK (
            (
                start_lineage_kind IS NULL
                AND immediate_predecessor_turn_id IS NULL
            )
            OR
            (
                start_lineage_kind = 'first_in_session'
                AND immediate_predecessor_turn_id IS NULL
            )
            OR
            (
                start_lineage_kind = 'after'
                AND immediate_predecessor_turn_id IS NOT NULL
            )
        ),
    CONSTRAINT turn_lifecycle_active_phase_closed
        CHECK (
            active_phase_kind IS NULL
            OR active_phase_kind = 'running'
        ),
    CONSTRAINT turn_lifecycle_terminal_disposition_closed
        CHECK (
            terminal_disposition_kind IS NULL
            OR terminal_disposition_kind = 'failed'
        ),
    CONSTRAINT turn_lifecycle_state_payload_shape
        CHECK (
            (
                state_kind = 'queued'
                AND start_lineage_kind IS NULL
                AND immediate_predecessor_turn_id IS NULL
                AND starting_frontier_id IS NULL
                AND terminal_frontier_id IS NULL
                AND active_phase_kind IS NULL
                AND current_attempt_id IS NULL
                AND terminal_disposition_kind IS NULL
            )
            OR
            (
                state_kind = 'active'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NULL
                AND active_phase_kind = 'running'
                AND current_attempt_id IS NOT NULL
                AND terminal_disposition_kind IS NULL
            )
            OR
            (
                state_kind = 'terminal'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NOT NULL
                AND active_phase_kind IS NULL
                AND current_attempt_id IS NULL
                AND terminal_disposition_kind = 'failed'
            )
        ),
    CONSTRAINT turn_lifecycle_turn_session_key
        UNIQUE (turn_id, session_id),
    CONSTRAINT turn_lifecycle_origin_correlation_key
        UNIQUE (
            origin_accepted_input_id,
            session_id,
            acceptance_position,
            turn_id
        ),
    CONSTRAINT turn_lifecycle_session_fk
        FOREIGN KEY (session_id)
        REFERENCES session (session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT turn_lifecycle_queued_origin_fk
        FOREIGN KEY (
            origin_accepted_input_id,
            session_id,
            acceptance_position,
            turn_id
        )
        REFERENCES queued_input_origin (
            accepted_input_id,
            session_id,
            acceptance_position,
            turn_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_lifecycle_predecessor_fk
        FOREIGN KEY (immediate_predecessor_turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE UNIQUE INDEX turn_lifecycle_one_active_per_session
    ON turn_lifecycle (session_id)
    WHERE state_kind = 'active';

CREATE INDEX turn_lifecycle_by_session_position
    ON turn_lifecycle (session_id, acceptance_position);

INSERT INTO turn_lifecycle (
    turn_id,
    session_id,
    origin_accepted_input_id,
    acceptance_position,
    state_kind
)
SELECT
    turn_id,
    session_id,
    accepted_input_id,
    acceptance_position,
    'queued'
  FROM queued_input_origin;

-- Clear the backfill's deferred foreign-key events before later ALTER TABLE
-- statements touch the lifecycle table in this same migration transaction.
SET CONSTRAINTS ALL IMMEDIATE;
SET CONSTRAINTS ALL DEFERRED;

ALTER TABLE queued_input_origin
    ADD CONSTRAINT queued_input_origin_turn_lifecycle_fk
    FOREIGN KEY (
        accepted_input_id,
        session_id,
        acceptance_position,
        turn_id
    )
    REFERENCES turn_lifecycle (
        origin_accepted_input_id,
        session_id,
        acceptance_position,
        turn_id
    )
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;

CREATE TABLE semantic_transcript_entry (
    source_session_id uuid NOT NULL,
    semantic_entry_id uuid NOT NULL,
    payload_kind text NOT NULL,
    origin_accepted_input_id uuid,
    failed_turn_id uuid,

    CONSTRAINT semantic_transcript_entry_pk
        PRIMARY KEY (source_session_id, semantic_entry_id),
    CONSTRAINT semantic_transcript_entry_id_global
        UNIQUE (semantic_entry_id),
    CONSTRAINT semantic_transcript_entry_payload_kind_closed
        CHECK (payload_kind IN ('origin_accepted_input', 'turn_failed')),
    CONSTRAINT semantic_transcript_entry_payload_shape
        CHECK (
            (
                payload_kind = 'origin_accepted_input'
                AND origin_accepted_input_id IS NOT NULL
                AND failed_turn_id IS NULL
            )
            OR
            (
                payload_kind = 'turn_failed'
                AND origin_accepted_input_id IS NULL
                AND failed_turn_id IS NOT NULL
            )
        ),
    CONSTRAINT semantic_transcript_entry_origin_once
        UNIQUE (origin_accepted_input_id),
    CONSTRAINT semantic_transcript_entry_turn_failed_once
        UNIQUE (failed_turn_id),
    CONSTRAINT semantic_transcript_entry_source_session_fk
        FOREIGN KEY (source_session_id)
        REFERENCES session (session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT semantic_transcript_entry_origin_fk
        FOREIGN KEY (origin_accepted_input_id, source_session_id)
        REFERENCES accepted_input (accepted_input_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT semantic_transcript_entry_failed_turn_fk
        FOREIGN KEY (failed_turn_id, source_session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER semantic_transcript_entry_is_append_only
BEFORE UPDATE OR DELETE ON semantic_transcript_entry
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE context_frontier (
    owning_session_id uuid NOT NULL,
    context_frontier_id uuid NOT NULL,
    member_count numeric(20, 0) NOT NULL,

    CONSTRAINT context_frontier_pk
        PRIMARY KEY (owning_session_id, context_frontier_id),
    CONSTRAINT context_frontier_id_global
        UNIQUE (context_frontier_id),
    CONSTRAINT context_frontier_member_count_u64
        CHECK (
            member_count >= 0
            AND member_count <= 18446744073709551615
        ),
    CONSTRAINT context_frontier_owning_session_fk
        FOREIGN KEY (owning_session_id)
        REFERENCES session (session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE TABLE context_frontier_member (
    owning_session_id uuid NOT NULL,
    context_frontier_id uuid NOT NULL,
    member_position numeric(20, 0) NOT NULL,
    source_session_id uuid NOT NULL,
    semantic_entry_id uuid NOT NULL,

    CONSTRAINT context_frontier_member_pk
        PRIMARY KEY (
            owning_session_id,
            context_frontier_id,
            member_position
        ),
    CONSTRAINT context_frontier_member_position_positive_u64
        CHECK (
            member_position >= 1
            AND member_position <= 18446744073709551615
        ),
    CONSTRAINT context_frontier_member_entry_once
        UNIQUE (
            owning_session_id,
            context_frontier_id,
            source_session_id,
            semantic_entry_id
        ),
    CONSTRAINT context_frontier_member_frontier_fk
        FOREIGN KEY (owning_session_id, context_frontier_id)
        REFERENCES context_frontier (
            owning_session_id,
            context_frontier_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT context_frontier_member_entry_fk
        FOREIGN KEY (source_session_id, semantic_entry_id)
        REFERENCES semantic_transcript_entry (
            source_session_id,
            semantic_entry_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER context_frontier_is_append_only
BEFORE UPDATE OR DELETE ON context_frontier
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER context_frontier_member_is_append_only
BEFORE UPDATE OR DELETE ON context_frontier_member
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION reject_context_frontier_member_out_of_bounds()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    declared_member_count numeric(20, 0);
BEGIN
    SELECT member_count
      INTO declared_member_count
      FROM context_frontier
     WHERE owning_session_id = NEW.owning_session_id
       AND context_frontier_id = NEW.context_frontier_id;

    -- Members may precede their deferred-FK header in one transaction. The
    -- header's single deferred completeness check validates that ordering.
    IF FOUND AND NEW.member_position > declared_member_count THEN
        RAISE EXCEPTION
            'context frontier member position % exceeds declared count %',
            NEW.member_position,
            declared_member_count
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'context_frontier_member_within_declared_count';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER context_frontier_member_stays_within_declared_count
BEFORE INSERT ON context_frontier_member
FOR EACH ROW
EXECUTE FUNCTION reject_context_frontier_member_out_of_bounds();

CREATE FUNCTION require_context_frontier_member_within_declared_count()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    declared_member_count numeric(20, 0);
BEGIN
    SELECT member_count
      INTO declared_member_count
      FROM context_frontier
     WHERE owning_session_id = NEW.owning_session_id
       AND context_frontier_id = NEW.context_frontier_id;

    IF NOT FOUND THEN
        RAISE EXCEPTION
            'context frontier header is unavailable for deferred member validation'
            USING
                ERRCODE = '23503',
                CONSTRAINT = 'context_frontier_member_requires_visible_header';
    END IF;

    IF NEW.member_position > declared_member_count THEN
        RAISE EXCEPTION
            'context frontier member position % exceeds declared count %',
            NEW.member_position,
            declared_member_count
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'context_frontier_member_within_declared_count';
    END IF;

    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER context_frontier_member_rechecks_declared_count
AFTER INSERT ON context_frontier_member
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_context_frontier_member_within_declared_count();

ALTER TABLE turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_starting_frontier_fk
    FOREIGN KEY (session_id, starting_frontier_id)
    REFERENCES context_frontier (owning_session_id, context_frontier_id)
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_terminal_frontier_fk
    FOREIGN KEY (session_id, terminal_frontier_id)
    REFERENCES context_frontier (owning_session_id, context_frontier_id)
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;

CREATE TABLE turn_attempt (
    turn_attempt_id uuid PRIMARY KEY,
    turn_id uuid NOT NULL,
    session_id uuid NOT NULL,
    continued_from_attempt_id uuid,
    state_kind text NOT NULL,
    end_variant text,
    end_disposition text,

    CONSTRAINT turn_attempt_state_kind_closed
        CHECK (state_kind IN ('prepared', 'running', 'ended')),
    CONSTRAINT turn_attempt_end_variant_closed
        CHECK (
            end_variant IS NULL
            OR end_variant = 'without_stop'
        ),
    CONSTRAINT turn_attempt_end_disposition_closed
        CHECK (
            end_disposition IS NULL
            OR end_disposition IN (
                'turn_completed',
                'turn_refused',
                'yielded_to_durable_wait',
                'known_failure',
                'lost',
                'ambiguous'
            )
        ),
    CONSTRAINT turn_attempt_state_payload_shape
        CHECK (
            (
                state_kind IN ('prepared', 'running')
                AND end_variant IS NULL
                AND end_disposition IS NULL
            )
            OR
            (
                state_kind = 'ended'
                AND end_variant = 'without_stop'
                AND end_disposition IS NOT NULL
            )
        ),
    CONSTRAINT turn_attempt_turn_correlation_key
        UNIQUE (turn_attempt_id, turn_id, session_id),
    CONSTRAINT turn_attempt_turn_fk
        FOREIGN KEY (turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT turn_attempt_continued_from_fk
        FOREIGN KEY (continued_from_attempt_id, turn_id, session_id)
        REFERENCES turn_attempt (turn_attempt_id, turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_attempt_one_successor_per_predecessor
        UNIQUE (continued_from_attempt_id)
);

CREATE UNIQUE INDEX turn_attempt_one_initial_per_turn
    ON turn_attempt (turn_id)
    WHERE continued_from_attempt_id IS NULL;

CREATE INDEX turn_attempt_by_turn_session
    ON turn_attempt (turn_id, session_id);

CREATE UNIQUE INDEX turn_attempt_one_live_per_turn
    ON turn_attempt (turn_id)
    WHERE state_kind <> 'ended';

ALTER TABLE turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_current_attempt_fk
    FOREIGN KEY (current_attempt_id, turn_id, session_id)
    REFERENCES turn_attempt (turn_attempt_id, turn_id, session_id)
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION reject_turn_lifecycle_invalid_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.state_kind <> 'queued' THEN
            RAISE EXCEPTION 'turn lifecycle must be inserted as queued'
                USING
                    ERRCODE = '23514',
                    CONSTRAINT = 'turn_lifecycle_inserted_queued';
        END IF;

        IF NEW.attempt_history_present THEN
            RAISE EXCEPTION 'turn lifecycle must be inserted without attempt history'
                USING ERRCODE = '23514';
        END IF;

        RETURN NEW;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'turn_lifecycle is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF ROW(
        OLD.turn_id,
        OLD.session_id,
        OLD.origin_accepted_input_id,
        OLD.acceptance_position
    ) IS DISTINCT FROM ROW(
        NEW.turn_id,
        NEW.session_id,
        NEW.origin_accepted_input_id,
        NEW.acceptance_position
    ) THEN
        RAISE EXCEPTION 'turn lifecycle identity, ownership, origin, and order are immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.start_lineage_kind IS NOT NULL
       AND ROW(
           OLD.start_lineage_kind,
           OLD.immediate_predecessor_turn_id,
           OLD.starting_frontier_id
       ) IS DISTINCT FROM ROW(
           NEW.start_lineage_kind,
           NEW.immediate_predecessor_turn_id,
           NEW.starting_frontier_id
       )
    THEN
        RAISE EXCEPTION 'turn start is write-once'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'terminal' THEN
        RAISE EXCEPTION 'terminal turn lifecycle is immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.attempt_history_present AND NOT NEW.attempt_history_present THEN
        RAISE EXCEPTION 'turn attempt history marker is write-once'
            USING ERRCODE = '23514';
    END IF;

    IF NOT (
        OLD.state_kind = NEW.state_kind
        OR (OLD.state_kind = 'queued' AND NEW.state_kind IN ('active', 'terminal'))
        OR (OLD.state_kind = 'active' AND NEW.state_kind = 'terminal')
    ) THEN
        RAISE EXCEPTION 'turn lifecycle transition is not monotonic'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'queued'
       AND NEW.state_kind = 'terminal'
       AND NEW.attempt_history_present
    THEN
        RAISE EXCEPTION
            'a queued turn must terminalize without attempt history'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'turn_lifecycle_queued_failure_without_attempt';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER turn_lifecycle_changes_are_guarded
BEFORE INSERT OR UPDATE OR DELETE ON turn_lifecycle
FOR EACH ROW
EXECUTE FUNCTION reject_turn_lifecycle_invalid_change();

CREATE FUNCTION reject_turn_attempt_invalid_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    owning_turn_state text;
BEGIN
    IF TG_OP = 'INSERT' THEN
        UPDATE turn_lifecycle
           SET attempt_history_present = true
         WHERE turn_id = NEW.turn_id
           AND session_id = NEW.session_id
           AND state_kind <> 'terminal'
        RETURNING state_kind
          INTO owning_turn_state
        ;

        IF NOT FOUND THEN
            RAISE EXCEPTION 'a terminal turn cannot acquire another attempt'
                USING ERRCODE = '23514';
        END IF;

        IF NEW.state_kind <> 'prepared' THEN
            RAISE EXCEPTION 'turn attempt must be inserted as prepared'
                USING
                    ERRCODE = '23514',
                    CONSTRAINT = 'turn_attempt_inserted_prepared';
        END IF;

        RETURN NEW;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'turn_attempt is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF ROW(
        OLD.turn_attempt_id,
        OLD.turn_id,
        OLD.session_id,
        OLD.continued_from_attempt_id
    ) IS DISTINCT FROM ROW(
        NEW.turn_attempt_id,
        NEW.turn_id,
        NEW.session_id,
        NEW.continued_from_attempt_id
    ) THEN
        RAISE EXCEPTION 'turn attempt identity, ownership, and predecessor are immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'ended' THEN
        RAISE EXCEPTION 'ended turn attempt is immutable'
            USING ERRCODE = '23514';
    END IF;

    IF NOT (
        OLD.state_kind = NEW.state_kind
        OR (OLD.state_kind = 'prepared' AND NEW.state_kind IN ('running', 'ended'))
        OR (OLD.state_kind = 'running' AND NEW.state_kind = 'ended')
    ) THEN
        RAISE EXCEPTION 'turn attempt transition is not monotonic'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER turn_attempt_changes_are_guarded
BEFORE INSERT OR UPDATE OR DELETE ON turn_attempt
FOR EACH ROW
EXECUTE FUNCTION reject_turn_attempt_invalid_change();

CREATE FUNCTION assert_context_frontier_complete_membership(
    checked_owning_session_id uuid,
    checked_context_frontier_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    expected_count numeric(20, 0);
    actual_count numeric(20, 0);
    first_position numeric(20, 0);
    last_position numeric(20, 0);
BEGIN
    SELECT member_count
      INTO expected_count
      FROM context_frontier
     WHERE owning_session_id = checked_owning_session_id
       AND context_frontier_id = checked_context_frontier_id;

    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT count(*)::numeric(20, 0),
           min(member_position),
           max(member_position)
      INTO actual_count, first_position, last_position
      FROM context_frontier_member
     WHERE owning_session_id = checked_owning_session_id
       AND context_frontier_id = checked_context_frontier_id;

    IF actual_count <> expected_count
       OR (
           expected_count > 0
           AND (
               first_position <> 1
               OR last_position <> expected_count
           )
       )
    THEN
        RAISE EXCEPTION
            'context frontier (%, %) does not have complete contiguous membership',
            checked_owning_session_id,
            checked_context_frontier_id
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION require_context_frontier_complete_membership()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM assert_context_frontier_complete_membership(
        CASE WHEN TG_OP = 'DELETE' THEN OLD.owning_session_id ELSE NEW.owning_session_id END,
        CASE WHEN TG_OP = 'DELETE' THEN OLD.context_frontier_id ELSE NEW.context_frontier_id END
    );

    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER context_frontier_requires_complete_membership
AFTER INSERT OR UPDATE OR DELETE ON context_frontier
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_context_frontier_complete_membership();

CREATE FUNCTION assert_turn_attempt_final_state(
    checked_turn_attempt_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    predecessor_id uuid;
BEGIN
    SELECT continued_from_attempt_id
      INTO predecessor_id
      FROM turn_attempt
     WHERE turn_attempt_id = checked_turn_attempt_id;

    IF NOT FOUND OR predecessor_id IS NULL THEN
        RETURN;
    END IF;

    -- Continuation requires durable wait/closure facts that this migration
    -- does not yet represent. Its owning migration must deliberately replace
    -- this guard before admitting a successor.
    RAISE EXCEPTION
        'turn attempt continuation is unavailable until durable wait/closure storage exists'
        USING
            ERRCODE = '23514',
            CONSTRAINT = 'turn_attempt_continuation_unavailable';
END;
$$;

CREATE FUNCTION assert_turn_lifecycle_final_state(
    checked_turn_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    checked_session_id uuid;
    checked_origin_input_id uuid;
    checked_position numeric(20, 0);
    checked_attempt_history_present boolean;
    checked_state text;
    checked_lineage text;
    checked_predecessor uuid;
    checked_starting_frontier uuid;
    checked_terminal_frontier uuid;
    checked_current_attempt uuid;
    attempt_count bigint;
    live_attempt_count bigint;
    current_live_attempt_count bigint;
    contradictory_failed_attempt_count bigint;
    origin_entry_count bigint;
    origin_entry_id uuid;
    failure_entry_count bigint;
    origin_member_count bigint;
    origin_member_position numeric(20, 0);
    last_member_position numeric(20, 0);
    failure_member_count bigint;
    failure_entry_id uuid;
    starting_member_count numeric(20, 0);
    terminal_member_count numeric(20, 0);
    predecessor_terminal_frontier uuid;
    predecessor_terminal_member_count numeric(20, 0);
    prefix_mismatch_count bigint;
    predecessor_state text;
    predecessor_position numeric(20, 0);
    expected_predecessor_position numeric(20, 0);
BEGIN
    SELECT
        session_id,
        origin_accepted_input_id,
        acceptance_position,
        attempt_history_present,
        state_kind,
        start_lineage_kind,
        immediate_predecessor_turn_id,
        starting_frontier_id,
        terminal_frontier_id,
        current_attempt_id
      INTO
        checked_session_id,
        checked_origin_input_id,
        checked_position,
        checked_attempt_history_present,
        checked_state,
        checked_lineage,
        checked_predecessor,
        checked_starting_frontier,
        checked_terminal_frontier,
        checked_current_attempt
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id;

    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT
        count(*),
        count(*) FILTER (WHERE state_kind <> 'ended'),
        count(*) FILTER (
            WHERE state_kind <> 'ended'
              AND turn_attempt_id = checked_current_attempt
        ),
        count(*) FILTER (
            WHERE state_kind <> 'ended'
               OR end_disposition NOT IN ('known_failure', 'lost')
        )
      INTO
        attempt_count,
        live_attempt_count,
        current_live_attempt_count,
        contradictory_failed_attempt_count
      FROM turn_attempt
     WHERE turn_id = checked_turn_id
       AND session_id = checked_session_id;

    IF checked_attempt_history_present IS DISTINCT FROM (attempt_count > 0) THEN
        RAISE EXCEPTION
            'turn % attempt history marker disagrees with durable attempts',
            checked_turn_id
            USING ERRCODE = '23514';
    END IF;

    SELECT count(*)
      INTO origin_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'origin_accepted_input'
       AND origin_accepted_input_id = checked_origin_input_id;

    SELECT count(*)
      INTO failure_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'turn_failed'
       AND failed_turn_id = checked_turn_id;

    IF checked_state = 'queued' THEN
        IF attempt_count <> 0
           OR origin_entry_count <> 0
           OR failure_entry_count <> 0
        THEN
            RAISE EXCEPTION
                'queued turn % carries attempt or semantic-start facts',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;
        RETURN;
    END IF;

    IF origin_entry_count <> 1 THEN
        RAISE EXCEPTION
            'started turn % requires exactly one correlated origin entry',
            checked_turn_id
            USING ERRCODE = '23503';
    END IF;

    SELECT semantic_entry_id
      INTO origin_entry_id
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'origin_accepted_input'
       AND origin_accepted_input_id = checked_origin_input_id;

    SELECT max(member_position)
      INTO last_member_position
     FROM context_frontier_member
     WHERE owning_session_id = checked_session_id
       AND context_frontier_id = checked_starting_frontier;

    SELECT member_count
      INTO starting_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session_id
       AND context_frontier_id = checked_starting_frontier;

    SELECT count(*), max(member_position)
      INTO origin_member_count, origin_member_position
      FROM context_frontier_member
     WHERE owning_session_id = checked_session_id
       AND context_frontier_id = checked_starting_frontier
       AND source_session_id = checked_session_id
       AND semantic_entry_id = origin_entry_id;

    IF origin_member_count <> 1
       OR origin_member_position IS DISTINCT FROM last_member_position
    THEN
        RAISE EXCEPTION
            'turn % starting frontier does not end with its exact origin entry',
            checked_turn_id
            USING ERRCODE = '23503';
    END IF;

    IF checked_lineage = 'first_in_session' THEN
        -- The baseline session schema admits only `none` ancestry, so the
        -- first start is exactly its origin. A migration opening fork
        -- ancestry must replace this count check with the source transcript
        -- frontier prefix plus the origin.
        IF starting_member_count IS DISTINCT FROM 1
           OR EXISTS (
            SELECT 1
              FROM turn_lifecycle AS earlier
             WHERE earlier.session_id = checked_session_id
               AND earlier.turn_id <> checked_turn_id
               AND earlier.acceptance_position < checked_position
        ) THEN
            RAISE EXCEPTION
                'turn % claims first lineage after earlier accepted work',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    ELSE
        -- Migration 003 admits only ordinary priority, so acceptance position
        -- is the complete scheduling order for this schema version. A migration
        -- that opens another priority must replace this positional selection
        -- with the domain-derived total order and correlation.
        SELECT state_kind, acceptance_position, terminal_frontier_id
          INTO
            predecessor_state,
            predecessor_position,
            predecessor_terminal_frontier
          FROM turn_lifecycle
         WHERE turn_id = checked_predecessor
           AND session_id = checked_session_id;

        SELECT max(acceptance_position)
          INTO expected_predecessor_position
          FROM turn_lifecycle
         WHERE session_id = checked_session_id
           AND acceptance_position < checked_position;

        IF predecessor_state IS DISTINCT FROM 'terminal'
           OR predecessor_position IS DISTINCT FROM expected_predecessor_position
        THEN
            RAISE EXCEPTION
                'turn % does not follow its immediate terminal predecessor',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;

        SELECT member_count
          INTO predecessor_terminal_member_count
          FROM context_frontier
         WHERE owning_session_id = checked_session_id
           AND context_frontier_id = predecessor_terminal_frontier;

        SELECT count(*)
          INTO prefix_mismatch_count
          FROM context_frontier_member AS predecessor_member
          LEFT JOIN context_frontier_member AS starting_member
            ON starting_member.owning_session_id = checked_session_id
           AND starting_member.context_frontier_id = checked_starting_frontier
           AND starting_member.member_position = predecessor_member.member_position
           AND starting_member.source_session_id = predecessor_member.source_session_id
           AND starting_member.semantic_entry_id = predecessor_member.semantic_entry_id
         WHERE predecessor_member.owning_session_id = checked_session_id
           AND predecessor_member.context_frontier_id = predecessor_terminal_frontier
           AND starting_member.member_position IS NULL;

        IF starting_member_count
               IS DISTINCT FROM predecessor_terminal_member_count + 1
           OR prefix_mismatch_count <> 0
        THEN
            RAISE EXCEPTION
                'turn % starting frontier is not its predecessor terminal frontier followed by its origin',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    END IF;

    IF checked_state = 'active' THEN
        IF live_attempt_count <> 1 OR current_live_attempt_count <> 1 THEN
            RAISE EXCEPTION
                'active turn % requires exactly its named live attempt',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;
        IF failure_entry_count <> 0 THEN
            RAISE EXCEPTION
                'active turn % cannot carry a terminal failure entry',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    ELSE
        IF live_attempt_count <> 0 OR failure_entry_count <> 1 THEN
            RAISE EXCEPTION
                'failed terminal turn % requires no live attempt and one failure entry',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;

        IF contradictory_failed_attempt_count <> 0 THEN
            RAISE EXCEPTION
                'failed terminal turn % permits only known_failure or lost ended attempts',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;

        SELECT semantic_entry_id
          INTO failure_entry_id
          FROM semantic_transcript_entry
         WHERE source_session_id = checked_session_id
           AND payload_kind = 'turn_failed'
           AND failed_turn_id = checked_turn_id;

        SELECT count(*)
          INTO failure_member_count
          FROM context_frontier_member AS member
          JOIN semantic_transcript_entry AS entry
            ON entry.source_session_id = member.source_session_id
           AND entry.semantic_entry_id = member.semantic_entry_id
         WHERE member.owning_session_id = checked_session_id
           AND member.context_frontier_id = checked_starting_frontier
           AND entry.payload_kind = 'turn_failed'
           AND entry.failed_turn_id = checked_turn_id;

        IF failure_member_count <> 0 THEN
            RAISE EXCEPTION
                'failed turn % starting frontier contains its later failure entry',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;

        SELECT member_count
          INTO terminal_member_count
          FROM context_frontier
         WHERE owning_session_id = checked_session_id
           AND context_frontier_id = checked_terminal_frontier;

        SELECT count(*)
          INTO prefix_mismatch_count
          FROM context_frontier_member AS starting_member
          LEFT JOIN context_frontier_member AS terminal_member
            ON terminal_member.owning_session_id = checked_session_id
           AND terminal_member.context_frontier_id = checked_terminal_frontier
           AND terminal_member.member_position = starting_member.member_position
           AND terminal_member.source_session_id = starting_member.source_session_id
           AND terminal_member.semantic_entry_id = starting_member.semantic_entry_id
         WHERE starting_member.owning_session_id = checked_session_id
           AND starting_member.context_frontier_id = checked_starting_frontier
           AND terminal_member.member_position IS NULL;

        IF terminal_member_count IS DISTINCT FROM starting_member_count + 1
           OR prefix_mismatch_count <> 0
           OR NOT EXISTS (
               SELECT 1
                 FROM context_frontier_member
                WHERE owning_session_id = checked_session_id
                  AND context_frontier_id = checked_terminal_frontier
                  AND member_position = terminal_member_count
                  AND source_session_id = checked_session_id
                  AND semantic_entry_id = failure_entry_id
           )
        THEN
            RAISE EXCEPTION
                'failed turn % terminal frontier is not its starting frontier followed by its exact failure entry',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    END IF;
END;
$$;

CREATE FUNCTION require_turn_lifecycle_final_state()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM assert_turn_lifecycle_final_state(
        CASE WHEN TG_OP = 'DELETE' THEN OLD.turn_id ELSE NEW.turn_id END
    );
    RETURN NULL;
END;
$$;

CREATE FUNCTION require_turn_attempt_final_state()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    checked_attempt_id uuid;
    checked_turn_id uuid;
BEGIN
    checked_attempt_id :=
        CASE WHEN TG_OP = 'DELETE' THEN OLD.turn_attempt_id ELSE NEW.turn_attempt_id END;
    checked_turn_id :=
        CASE WHEN TG_OP = 'DELETE' THEN OLD.turn_id ELSE NEW.turn_id END;

    PERFORM assert_turn_attempt_final_state(checked_attempt_id);
    PERFORM assert_turn_lifecycle_final_state(checked_turn_id);
    RETURN NULL;
END;
$$;

CREATE FUNCTION require_semantic_entry_turn_state()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    checked_payload_kind text;
    checked_origin_input_id uuid;
    checked_failed_turn_id uuid;
    checked_turn_id uuid;
BEGIN
    IF TG_OP = 'DELETE' THEN
        checked_payload_kind := OLD.payload_kind;
        checked_origin_input_id := OLD.origin_accepted_input_id;
        checked_failed_turn_id := OLD.failed_turn_id;
    ELSE
        checked_payload_kind := NEW.payload_kind;
        checked_origin_input_id := NEW.origin_accepted_input_id;
        checked_failed_turn_id := NEW.failed_turn_id;
    END IF;

    IF checked_payload_kind = 'origin_accepted_input' THEN
        SELECT origin_turn_id
          INTO checked_turn_id
          FROM accepted_input
         WHERE accepted_input_id = checked_origin_input_id;
    ELSE
        checked_turn_id := checked_failed_turn_id;
    END IF;

    PERFORM assert_turn_lifecycle_final_state(checked_turn_id);
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER turn_lifecycle_requires_complete_final_state
AFTER INSERT OR UPDATE OR DELETE ON turn_lifecycle
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_turn_lifecycle_final_state();

CREATE CONSTRAINT TRIGGER turn_attempt_requires_complete_final_state
AFTER INSERT OR UPDATE OR DELETE ON turn_attempt
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_turn_attempt_final_state();

CREATE CONSTRAINT TRIGGER semantic_entry_requires_matching_turn_state
AFTER INSERT OR UPDATE OR DELETE ON semantic_transcript_entry
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_semantic_entry_turn_state();
