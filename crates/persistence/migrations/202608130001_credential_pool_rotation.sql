-- Durable credential-pool policy snapshots and availability-successor state.

CREATE TABLE model_call_credential_pool_policy (
    model_call_id uuid PRIMARY KEY REFERENCES model_call(model_call_id),
    pool_name text NOT NULL,
    on_pool_exhausted text NOT NULL CHECK (on_pool_exhausted IN ('park', 'fail')),
    on_quota_exhausted text NOT NULL CHECK (
        on_quota_exhausted IN (
            'stay', 'switch_next_turn', 'switch_now', 'avoid_new_sessions', 'quarantine'
        )
    ),
    on_rate_limited text NOT NULL CHECK (
        on_rate_limited IN (
            'stay', 'switch_next_turn', 'switch_now', 'avoid_new_sessions', 'quarantine'
        )
    ),
    on_overloaded text NOT NULL CHECK (
        on_overloaded IN (
            'stay', 'switch_next_turn', 'switch_now', 'avoid_new_sessions', 'quarantine'
        )
    ),
    on_credential_rejected text NOT NULL CHECK (
        on_credential_rejected IN (
            'stay', 'switch_next_turn', 'switch_now', 'avoid_new_sessions', 'quarantine'
        )
    )
);

CREATE TABLE model_call_credential_pool_member (
    model_call_id uuid NOT NULL REFERENCES model_call_credential_pool_policy(model_call_id),
    member_ordinal integer NOT NULL CHECK (member_ordinal >= 0),
    credential_reference text NOT NULL,
    priority bigint NOT NULL CHECK (priority > 0),
    PRIMARY KEY (model_call_id, member_ordinal),
    UNIQUE (model_call_id, credential_reference)
);

CREATE TABLE credential_pool_chain_exclusion (
    session_id uuid NOT NULL REFERENCES session(session_id),
    turn_id uuid NOT NULL,
    credential_reference text NOT NULL,
    predecessor_model_call_id uuid NOT NULL REFERENCES model_call(model_call_id),
    cause_kind text NOT NULL CHECK (
        cause_kind IN ('rate_limited', 'quota_exhausted', 'overloaded')
    ),
    PRIMARY KEY (session_id, turn_id, credential_reference),
    UNIQUE (predecessor_model_call_id),
    FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id)
);

CREATE TABLE credential_pool_member_action (
    action_id bigserial PRIMARY KEY,
    pool_name text NOT NULL,
    credential_reference text NOT NULL,
    action_kind text NOT NULL CHECK (
        action_kind IN ('switch_next_turn', 'avoid_new_sessions', 'quarantine')
    ),
    observed_session_id uuid NOT NULL REFERENCES session(session_id),
    observed_turn_id uuid NOT NULL,
    observation_model_call_id uuid NOT NULL UNIQUE REFERENCES model_call(model_call_id),
    consumed_turn_id uuid,
    -- A rejected credential is an ordinary durable exclusion cause: it admits
    -- every action except switch_now, which needs a substitutable successor.
    cause_kind text NOT NULL CHECK (
        cause_kind IN (
            'rate_limited', 'quota_exhausted', 'overloaded', 'credential_rejected'
        )
    ),
    FOREIGN KEY (observed_turn_id, observed_session_id)
        REFERENCES turn_lifecycle(turn_id, session_id),
    CHECK (
        (action_kind = 'switch_next_turn') OR consumed_turn_id IS NULL
    )
);

CREATE INDEX credential_pool_member_action_selection
    ON credential_pool_member_action (pool_name, credential_reference, action_kind)
    WHERE consumed_turn_id IS NULL;

CREATE TABLE credential_pool_availability_successor (
    predecessor_model_call_id uuid PRIMARY KEY REFERENCES model_call(model_call_id),
    successor_turn_attempt_id uuid NOT NULL UNIQUE REFERENCES turn_attempt(turn_attempt_id),
    cause_kind text NOT NULL CHECK (
        cause_kind IN ('rate_limited', 'quota_exhausted', 'overloaded')
    ),
    retry_backoff_milliseconds bigint NOT NULL CHECK (retry_backoff_milliseconds >= 0),
    retry_not_before timestamptz NOT NULL
);

CREATE TABLE credential_pool_terminal_exhaustion (
    terminal_attempt_id uuid PRIMARY KEY REFERENCES turn_attempt(turn_attempt_id),
    terminal_model_call_id uuid UNIQUE REFERENCES model_call(model_call_id),
    session_id uuid NOT NULL REFERENCES session(session_id),
    turn_id uuid NOT NULL,
    pool_name text NOT NULL,
    cause_kind text CHECK (
        cause_kind IN ('rate_limited', 'quota_exhausted', 'overloaded')
    ),
    FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id)
);

-- The lifecycle guard admits exactly the second intra-turn successor shape:
-- tool-continuation was already admitted; availability substitution is now
-- admitted only when the durable predecessor/successor relation proves it.
CREATE OR REPLACE FUNCTION reject_turn_lifecycle_invalid_change()
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
        IF NEW.pinned_provider_model_identity_id IS NOT NULL THEN
            RAISE EXCEPTION 'queued turn lifecycle cannot begin with a provider target pin'
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

    IF OLD.pinned_provider_model_identity_id IS NOT NULL
       AND NEW.pinned_provider_model_identity_id
           IS DISTINCT FROM OLD.pinned_provider_model_identity_id
    THEN
        RAISE EXCEPTION 'turn-level provider target pin is immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.pinned_provider_model_identity_id IS NULL
       AND NEW.pinned_provider_model_identity_id IS NOT NULL
       AND (
            OLD.state_kind IS DISTINCT FROM 'active'
            OR NEW.state_kind IS DISTINCT FROM 'active'
            OR OLD.active_phase_kind IS DISTINCT FROM 'running'
            OR NEW.active_phase_kind IS DISTINCT FROM 'running'
            OR OLD.current_attempt_id IS NULL
            OR NEW.current_attempt_id IS DISTINCT FROM OLD.current_attempt_id
       )
    THEN
        RAISE EXCEPTION 'provider target can be pinned only for the current running attempt'
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

    IF OLD.state_kind = 'active'
       AND OLD.active_phase_kind IN (
            'awaiting_model_call_recovery',
            'awaiting_tool_recovery'
       )
       AND NEW.state_kind = 'active'
    THEN
        RAISE EXCEPTION 'recovery wait cannot reopen without a recovery decision'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'active'
       AND OLD.active_phase_kind = 'running'
       AND NEW.state_kind = 'active'
       AND NEW.active_phase_kind = 'running'
       AND OLD.current_attempt_id IS DISTINCT FROM NEW.current_attempt_id
       AND NOT EXISTS (
            SELECT 1
              FROM credential_pool_availability_successor AS successor
              JOIN model_call AS predecessor
                ON predecessor.model_call_id = successor.predecessor_model_call_id
             WHERE successor.successor_turn_attempt_id = NEW.current_attempt_id
               AND predecessor.turn_attempt_id = OLD.current_attempt_id
               AND predecessor.turn_id = OLD.turn_id
               AND predecessor.session_id = OLD.session_id
               AND predecessor.state_kind = 'terminal'
               AND predecessor.terminal_disposition_kind = 'known_failed'
       )
       AND (
            NEW.active_tool_round_call_id IS NULL
            OR NOT EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE turn_attempt_id = OLD.current_attempt_id
                   AND turn_id = OLD.turn_id
                   AND session_id = OLD.session_id
                   AND state_kind = 'ended'
                   AND end_disposition = 'yielded_to_durable_wait'
            )
       )
    THEN
        RAISE EXCEPTION 'running turn cannot replace its current attempt'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'queued'
       AND NEW.state_kind = 'terminal'
       AND NEW.attempt_history_present
    THEN
        RAISE EXCEPTION 'a queued turn must terminalize without attempt history'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'turn_lifecycle_queued_failure_without_attempt';
    END IF;

    RETURN NEW;
END;
$$;

ALTER FUNCTION assert_model_call_final_state(uuid)
    RENAME TO assert_model_call_final_state_before_credential_pools;

CREATE FUNCTION assert_model_call_final_state(checked_model_call_id uuid)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    successor_attempt_id uuid;
    predecessor_turn_id uuid;
    predecessor_session_id uuid;
    predecessor_attempt_id uuid;
    predecessor_state text;
    predecessor_disposition text;
    predecessor_attempt_state text;
    predecessor_attempt_disposition text;
    successor_state text;
    successor_continuation uuid;
    lifecycle_state text;
    lifecycle_phase text;
    lifecycle_attempt uuid;
BEGIN
    SELECT
        successor.successor_turn_attempt_id,
        predecessor.turn_id,
        predecessor.session_id,
        predecessor.turn_attempt_id,
        predecessor.state_kind,
        predecessor.terminal_disposition_kind,
        predecessor_attempt.state_kind,
        predecessor_attempt.end_disposition,
        successor_attempt.state_kind,
        successor_attempt.continued_from_attempt_id,
        lifecycle.state_kind,
        lifecycle.active_phase_kind,
        lifecycle.current_attempt_id
      INTO
        successor_attempt_id,
        predecessor_turn_id,
        predecessor_session_id,
        predecessor_attempt_id,
        predecessor_state,
        predecessor_disposition,
        predecessor_attempt_state,
        predecessor_attempt_disposition,
        successor_state,
        successor_continuation,
        lifecycle_state,
        lifecycle_phase,
        lifecycle_attempt
      FROM credential_pool_availability_successor AS successor
      JOIN model_call AS predecessor
        ON predecessor.model_call_id = successor.predecessor_model_call_id
      JOIN turn_attempt AS predecessor_attempt
        ON predecessor_attempt.turn_attempt_id = predecessor.turn_attempt_id
       AND predecessor_attempt.turn_id = predecessor.turn_id
       AND predecessor_attempt.session_id = predecessor.session_id
      JOIN turn_attempt AS successor_attempt
        ON successor_attempt.turn_attempt_id = successor.successor_turn_attempt_id
       AND successor_attempt.turn_id = predecessor.turn_id
       AND successor_attempt.session_id = predecessor.session_id
      JOIN turn_lifecycle AS lifecycle
        ON lifecycle.turn_id = predecessor.turn_id
       AND lifecycle.session_id = predecessor.session_id
     WHERE successor.predecessor_model_call_id = checked_model_call_id;

    IF FOUND THEN
        IF predecessor_state IS DISTINCT FROM 'terminal'
           OR predecessor_disposition IS DISTINCT FROM 'known_failed'
           OR predecessor_attempt_state IS DISTINCT FROM 'ended'
           OR predecessor_attempt_disposition IS DISTINCT FROM 'known_failure'
           OR successor_state IS DISTINCT FROM 'prepared'
           OR successor_continuation IS DISTINCT FROM predecessor_attempt_id
           OR lifecycle_state IS DISTINCT FROM 'active'
           OR lifecycle_phase IS DISTINCT FROM 'running'
           OR lifecycle_attempt IS DISTINCT FROM successor_attempt_id
        THEN
            RAISE EXCEPTION 'availability predecessor lacks its exact successor state'
                USING ERRCODE = '23514';
        END IF;
        RETURN;
    END IF;

    IF EXISTS (
        SELECT 1
          FROM model_call AS call
          JOIN credential_pool_availability_successor AS successor
            ON successor.successor_turn_attempt_id = call.turn_attempt_id
         WHERE call.model_call_id = checked_model_call_id
    ) THEN
        -- A terminal availability successor with no later availability
        -- successor may have yielded into the ordinary tool-round lifecycle,
        -- or ended ambiguously and parked its still-active turn for model-call
        -- recovery. Preserve the availability lineage checks here, then
        -- delegate the terminal lifecycle shape to the validator that owns
        -- both of those active shapes.
        IF EXISTS (
            SELECT 1
              FROM model_call AS call
              JOIN credential_pool_availability_successor AS successor
                ON successor.successor_turn_attempt_id = call.turn_attempt_id
              JOIN model_call AS predecessor
                ON predecessor.model_call_id = successor.predecessor_model_call_id
             WHERE call.model_call_id = checked_model_call_id
               AND call.turn_id = predecessor.turn_id
               AND call.session_id = predecessor.session_id
               AND call.resolved_provider_model_identity_id =
                   predecessor.resolved_provider_model_identity_id
               AND ROW(
                    call.selection_kind,
                    call.direct_model_selection_id,
                    call.frozen_model_alias_id,
                    call.frozen_alias_selected_direct_id
               ) IS NOT DISTINCT FROM ROW(
                    predecessor.selection_kind,
                    predecessor.direct_model_selection_id,
                    predecessor.frozen_model_alias_id,
                    predecessor.frozen_alias_selected_direct_id
               )
               AND call.state_kind = 'terminal'
               AND (
                    EXISTS (
                        SELECT 1
                          FROM tool_round AS round
                         WHERE round.producing_model_call_id = call.model_call_id
                    )
                    OR EXISTS (
                        SELECT 1
                          FROM turn_lifecycle AS waiting
                         WHERE waiting.turn_id = call.turn_id
                           AND waiting.session_id = call.session_id
                           AND waiting.state_kind = 'active'
                           AND waiting.active_phase_kind =
                               'awaiting_model_call_recovery'
                           AND waiting.recovery_model_call_id = call.model_call_id
                    )
               )
               AND NOT EXISTS (
                    SELECT 1
                      FROM credential_pool_availability_successor AS later
                     WHERE later.predecessor_model_call_id = call.model_call_id
               )
        ) THEN
            PERFORM assert_model_call_final_state_before_credential_pools(
                checked_model_call_id
            );
            RETURN;
        END IF;

        IF NOT EXISTS (
            SELECT 1
              FROM model_call AS call
              JOIN credential_pool_availability_successor AS successor
                ON successor.successor_turn_attempt_id = call.turn_attempt_id
              JOIN model_call AS predecessor
                ON predecessor.model_call_id = successor.predecessor_model_call_id
              JOIN turn_attempt AS attempt
                ON attempt.turn_attempt_id = call.turn_attempt_id
               AND attempt.turn_id = call.turn_id
               AND attempt.session_id = call.session_id
              JOIN turn_lifecycle AS lifecycle
                ON lifecycle.turn_id = call.turn_id
               AND lifecycle.session_id = call.session_id
             WHERE call.model_call_id = checked_model_call_id
               AND call.turn_id = predecessor.turn_id
               AND call.session_id = predecessor.session_id
               AND call.resolved_provider_model_identity_id =
                   predecessor.resolved_provider_model_identity_id
               AND ROW(
                    call.selection_kind,
                    call.direct_model_selection_id,
                    call.frozen_model_alias_id,
                    call.frozen_alias_selected_direct_id
               ) IS NOT DISTINCT FROM ROW(
                    predecessor.selection_kind,
                    predecessor.direct_model_selection_id,
                    predecessor.frozen_model_alias_id,
                    predecessor.frozen_alias_selected_direct_id
               )
               AND (
                    (
                        call.state_kind = 'prepared'
                        AND attempt.state_kind = 'prepared'
                        AND lifecycle.state_kind = 'active'
                        AND lifecycle.active_phase_kind = 'running'
                        AND lifecycle.current_attempt_id = call.turn_attempt_id
                    )
                    OR (
                        call.state_kind = 'in_flight'
                        AND attempt.state_kind = 'running'
                        AND lifecycle.state_kind = 'active'
                        AND lifecycle.active_phase_kind = 'running'
                        AND lifecycle.current_attempt_id = call.turn_attempt_id
                    )
                    -- An interrupt on a rotated in-flight call moves the call
                    -- to cancellation_requested and its attempt to
                    -- stop_requested together, exactly as the pre-pool
                    -- validator admits. Requiring a still-running attempt here
                    -- rejected the submit-input transaction, so a provider call
                    -- made by a substituted credential could not be cancelled.
                    OR (
                        call.state_kind = 'cancellation_requested'
                        AND attempt.state_kind IN ('running', 'stop_requested')
                        AND lifecycle.state_kind = 'active'
                        AND lifecycle.active_phase_kind = 'running'
                        AND lifecycle.current_attempt_id = call.turn_attempt_id
                    )
                    OR (
                        call.state_kind = 'terminal'
                        AND attempt.state_kind = 'ended'
                        AND (
                            lifecycle.state_kind = 'terminal'
                            OR EXISTS (
                                SELECT 1
                                  FROM credential_pool_availability_successor AS later
                                 WHERE later.predecessor_model_call_id = call.model_call_id
                            )
                        )
                    )
               )
        ) THEN
            RAISE EXCEPTION 'availability successor call lacks exact lifecycle state'
                USING ERRCODE = '23514';
        END IF;
        RETURN;
    END IF;

    PERFORM assert_model_call_final_state_before_credential_pools(
        checked_model_call_id
    );
END;
$$;

ALTER FUNCTION assert_turn_attempt_final_state(uuid)
    RENAME TO assert_turn_attempt_final_state_before_credential_pools;

CREATE FUNCTION assert_turn_attempt_final_state(checked_attempt_id uuid)
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM credential_pool_availability_successor AS successor
          JOIN turn_attempt AS successor_attempt
            ON successor_attempt.turn_attempt_id =
               successor.successor_turn_attempt_id
          JOIN model_call AS predecessor
            ON predecessor.model_call_id = successor.predecessor_model_call_id
         WHERE successor.successor_turn_attempt_id = checked_attempt_id
           AND successor_attempt.turn_id = predecessor.turn_id
           AND successor_attempt.session_id = predecessor.session_id
           AND successor_attempt.continued_from_attempt_id =
               predecessor.turn_attempt_id
           AND predecessor.state_kind = 'terminal'
           AND predecessor.terminal_disposition_kind = 'known_failed'
    ) THEN
        RETURN;
    END IF;

    PERFORM assert_turn_attempt_final_state_before_credential_pools(
        checked_attempt_id
    );
END;
$$;

ALTER FUNCTION assert_failed_terminal_execution_final_state(uuid)
    RENAME TO assert_failed_terminal_execution_before_credential_pools;

CREATE FUNCTION assert_failed_terminal_execution_final_state(checked_turn_id uuid)
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM credential_pool_terminal_exhaustion AS exhausted
          JOIN turn_lifecycle AS lifecycle
            ON lifecycle.turn_id = exhausted.turn_id
           AND lifecycle.session_id = exhausted.session_id
          JOIN turn_attempt AS attempt
            ON attempt.turn_attempt_id = exhausted.terminal_attempt_id
           AND attempt.turn_id = exhausted.turn_id
           AND attempt.session_id = exhausted.session_id
          LEFT JOIN model_call AS call
            ON call.model_call_id = exhausted.terminal_model_call_id
           AND call.turn_attempt_id = exhausted.terminal_attempt_id
           AND call.turn_id = exhausted.turn_id
           AND call.session_id = exhausted.session_id
         WHERE exhausted.turn_id = checked_turn_id
           AND lifecycle.state_kind = 'terminal'
           AND lifecycle.terminal_disposition_kind = 'failed'
           AND lifecycle.terminal_attempt_id = exhausted.terminal_attempt_id
           AND lifecycle.terminal_model_call_id IS NOT DISTINCT FROM
               exhausted.terminal_model_call_id
           AND attempt.state_kind = 'ended'
           AND attempt.end_disposition = 'known_failure'
           AND (
                exhausted.terminal_model_call_id IS NULL
                OR (
                    call.state_kind = 'terminal'
                    AND call.terminal_disposition_kind = 'known_failed'
                )
           )
    ) THEN
        RETURN;
    END IF;

    PERFORM assert_failed_terminal_execution_before_credential_pools(
        checked_turn_id
    );
END;
$$;
