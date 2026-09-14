-- A checked takeover installs placement without projecting the unresolved round.
CREATE TABLE runner_recovery_takeover (
    command_id uuid PRIMARY KEY REFERENCES replace_lost_runner_command(command_id),
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    source_event_ordinal numeric(20, 0) NOT NULL,
    retained_loss_event_ordinal numeric(20, 0) NOT NULL,
    successor_event_ordinal numeric(20, 0) NOT NULL,
    yielded_turn_attempt_id uuid NOT NULL,
    producing_model_call_id uuid REFERENCES tool_round(producing_model_call_id),
    interrupted_tool_attempt_id uuid REFERENCES tool_attempt(attempt_id),
    source_lease_id uuid,
    source_generation numeric(20, 0),
    UNIQUE (session_id, source_event_ordinal),
    UNIQUE (session_id, successor_event_ordinal),
    FOREIGN KEY (session_id, source_event_ordinal)
        REFERENCES runner_session_placement_record(session_id, event_ordinal),
    FOREIGN KEY (session_id, retained_loss_event_ordinal)
        REFERENCES runner_session_placement_record(session_id, event_ordinal),
    FOREIGN KEY (session_id, successor_event_ordinal)
        REFERENCES runner_session_placement_record(session_id, event_ordinal),
    FOREIGN KEY (yielded_turn_attempt_id, turn_id, session_id)
        REFERENCES turn_attempt(turn_attempt_id, turn_id, session_id),
    FOREIGN KEY (source_lease_id, source_generation)
        REFERENCES runner_lease_generation(lease_id, generation),
    CHECK (successor_event_ordinal = source_event_ordinal + 1),
    CHECK ((interrupted_tool_attempt_id IS NULL AND source_lease_id IS NULL AND source_generation IS NULL)
        OR (interrupted_tool_attempt_id IS NOT NULL AND source_lease_id IS NOT NULL AND source_generation IS NOT NULL))
);

CREATE TRIGGER runner_recovery_takeover_is_append_only
    BEFORE UPDATE OR DELETE ON runner_recovery_takeover
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

-- The installed placement can be lost before it receives the retained request.
CREATE FUNCTION runner_recovery_loss_ordinal(checked_session uuid, checked_turn uuid, head_ordinal numeric)
RETURNS numeric LANGUAGE sql STABLE AS $$
    SELECT COALESCE((
        SELECT takeover.retained_loss_event_ordinal
        FROM runner_recovery_takeover AS takeover
        JOIN runner_session_placement_record AS head ON head.session_id = takeover.session_id
          AND head.event_ordinal = head_ordinal
        JOIN runner_session_placement_record AS retained ON retained.session_id = takeover.session_id
          AND retained.event_ordinal = takeover.retained_loss_event_ordinal
        JOIN turn_lifecycle AS turn ON turn.session_id = takeover.session_id AND turn.turn_id = takeover.turn_id
        WHERE takeover.session_id = checked_session AND takeover.turn_id = checked_turn
          AND retained.interrupted_tool_attempt_id IS NOT DISTINCT FROM turn.runner_recovery_tool_attempt_id
          AND retained.lost_runner_id = turn.runner_recovery_runner_id
          AND retained.placement_revision = turn.runner_recovery_placement_revision
          AND (takeover.successor_event_ordinal = head_ordinal OR
              (takeover.successor_event_ordinal + 1 = head_ordinal AND head.event_kind = 'runner_lost'
               AND head.interrupted_tool_attempt_id IS NULL))
        ORDER BY takeover.successor_event_ordinal DESC LIMIT 1
    ), head_ordinal)
$$;

CREATE FUNCTION guard_runner_recovery_takeover() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM runner_replacement_stage AS stage
        JOIN turn_lifecycle AS turn ON turn.session_id = stage.session_id
        JOIN runner_session_placement_record AS source
          ON source.session_id = stage.session_id AND source.event_ordinal = stage.source_event_ordinal
        JOIN runner_session_placement_record AS retained
          ON retained.session_id = source.session_id AND retained.event_ordinal = NEW.retained_loss_event_ordinal
        JOIN runner_session_placement_record AS successor
          ON successor.session_id = source.session_id AND successor.event_ordinal = NEW.successor_event_ordinal
        JOIN turn_attempt AS yielded ON yielded.turn_attempt_id = NEW.yielded_turn_attempt_id
        WHERE stage.command_id = NEW.command_id AND stage.session_id = NEW.session_id
          AND stage.source_event_ordinal = NEW.source_event_ordinal
          AND turn.turn_id = NEW.turn_id AND turn.state_kind = 'active'
          AND turn.active_phase_kind = 'awaiting_runner_recovery'
          AND NOT turn.delegation_runtime_terminal
          AND turn.active_tool_round_call_id IS NOT DISTINCT FROM NEW.producing_model_call_id
          AND turn.runner_recovery_tool_attempt_id IS NOT DISTINCT FROM NEW.interrupted_tool_attempt_id
          AND turn.runner_recovery_runner_id = retained.lost_runner_id
          AND turn.runner_recovery_placement_revision = retained.placement_revision
          AND source.state_kind IN ('runner_lost', 'runner_lost_before_pin')
          AND retained.state_kind IN ('runner_lost', 'runner_lost_before_pin')
          AND retained.interrupted_tool_attempt_id IS NOT DISTINCT FROM NEW.interrupted_tool_attempt_id
          AND (NEW.retained_loss_event_ordinal = NEW.source_event_ordinal OR EXISTS (
              SELECT 1 FROM runner_recovery_takeover AS prior
              WHERE prior.session_id = NEW.session_id AND prior.turn_id = NEW.turn_id
                AND prior.successor_event_ordinal + 1 = NEW.source_event_ordinal
                AND prior.retained_loss_event_ordinal = NEW.retained_loss_event_ordinal
                AND prior.source_lease_id = NEW.source_lease_id AND prior.source_generation = NEW.source_generation
                AND source.interrupted_tool_attempt_id IS NULL
          ))
          AND successor.event_kind IN ('runner_replaced', 'pre_pin_replaced')
          AND successor.placement_revision = source.placement_revision + 1
          AND ((successor.registration_enrollment_id = stage.successor_enrollment_id
                AND successor.registration_revision = stage.successor_registration_revision)
            OR (successor.event_kind = 'pre_pin_replaced' AND successor.selector_runner_id = (
                SELECT runner_id FROM runner_enrollment WHERE enrollment_id = stage.successor_enrollment_id)))
          AND yielded.session_id = turn.session_id AND yielded.turn_id = turn.turn_id
          AND yielded.state_kind = 'ended' AND yielded.end_variant = 'without_stop'
          AND yielded.end_disposition = 'yielded_to_durable_wait'
          AND NOT EXISTS (SELECT 1 FROM turn_attempt WHERE continued_from_attempt_id = yielded.turn_attempt_id)
          AND (NEW.interrupted_tool_attempt_id IS NULL OR EXISTS (
              SELECT 1 FROM runner_lease_generation AS lease
              JOIN runner_current_lease_event AS head USING (lease_id, generation)
              JOIN runner_lease_event AS event ON event.lease_id = head.lease_id AND event.generation = head.generation AND event.event_ordinal = head.event_ordinal
              JOIN tool_attempt AS attempt ON attempt.attempt_id = lease.attempt_id
              JOIN tool_request AS request ON request.request_id = attempt.request_id
              WHERE lease.lease_id = NEW.source_lease_id AND lease.generation = NEW.source_generation
                AND lease.attempt_id = NEW.interrupted_tool_attempt_id
                AND lease.session_id = NEW.session_id AND lease.runner_id = retained.lost_runner_id
                AND request.producing_model_call_id = NEW.producing_model_call_id
                AND ((attempt.state_kind = 'in_flight'
                    AND (event.state_kind = 'lost_unclaimed' OR
                        (event.state_kind IN ('lost_claimed', 'lost_execution_possible') AND lease.effect_class IN ('pure', 'idempotent'))))
                    OR (event.state_kind = 'refused' AND attempt.state_kind = 'terminal'
                        AND attempt.terminal_disposition_kind = 'known_failed' AND attempt.error_kind = 'execution_failed'))
          ))
    ) THEN
        RAISE EXCEPTION 'runner takeover lacks its exact staged replacement and recovery wait' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_recovery_takeover_has_authority
    BEFORE INSERT ON runner_recovery_takeover
    FOR EACH ROW EXECUTE FUNCTION guard_runner_recovery_takeover();

-- A waiting turn continues to authenticate its retained loss after installation.
DO $$
DECLARE definition text; before text; after text;
BEGIN
    SELECT pg_get_functiondef('assert_turn_runner_recovery_complete(uuid, uuid)'::regprocedure) INTO definition;
    before := 'record.event_ordinal = current_placement.event_ordinal';
    after := 'record.event_ordinal = runner_recovery_loss_ordinal(checked_session_id, checked_turn_id, current_placement.event_ordinal)';
    IF strpos(definition, before) = 0 THEN RAISE EXCEPTION 'runner recovery placement lookup missing'; END IF;
    EXECUTE replace(definition, before, after);
END;
$$;

DO $patch$
DECLARE definition text;
BEGIN
    SELECT pg_get_functiondef('guard_runner_lease_generation()'::regprocedure) INTO definition;
    IF strpos(definition, $before$           OR ROW(
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
           )$before$) = 0 THEN
        RAISE EXCEPTION 'runner takeover guard pattern missing: guard_runner_lease_generation()';
    END IF;
    EXECUTE replace(definition, $before$           OR ROW(
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
           )$before$, $after$           OR ROW(prior.session_id, prior.tool_name, prior.effect_class)
                IS DISTINCT FROM ROW(NEW.session_id, NEW.tool_name, NEW.effect_class)
           OR (ROW(
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
               AND NOT EXISTS (
                   SELECT 1 FROM runner_recovery_takeover AS takeover
                   WHERE takeover.session_id = NEW.session_id
                     AND takeover.source_lease_id = prior.lease_id
                     AND takeover.source_generation = prior.generation
                     AND takeover.successor_event_ordinal = NEW.placement_event_ordinal
               )
           )$after$);
END;
$patch$;

DO $patch$
DECLARE definition text;
BEGIN
    SELECT pg_get_functiondef('reject_runner_recovery_reopen()'::regprocedure) INTO definition;
    IF strpos(definition, $before$    IF OLD.state_kind = 'active'
       AND OLD.active_phase_kind = 'awaiting_runner_recovery'
       AND NEW.state_kind = 'active'
       AND NOT (
            NOT OLD.delegation_runtime_terminal
            AND NEW.delegation_runtime_terminal
            AND (to_jsonb(OLD) - 'delegation_runtime_terminal') =
                (to_jsonb(NEW) - 'delegation_runtime_terminal')
       )
$before$) = 0 THEN
        RAISE EXCEPTION 'runner takeover guard pattern missing: reject_runner_recovery_reopen()';
    END IF;
    EXECUTE replace(definition, $before$    IF OLD.state_kind = 'active'
       AND OLD.active_phase_kind = 'awaiting_runner_recovery'
       AND NEW.state_kind = 'active'
       AND NOT (
            NOT OLD.delegation_runtime_terminal
            AND NEW.delegation_runtime_terminal
            AND (to_jsonb(OLD) - 'delegation_runtime_terminal') =
                (to_jsonb(NEW) - 'delegation_runtime_terminal')
       )
$before$, $after$    IF NEW IS DISTINCT FROM OLD
       AND OLD.state_kind = 'active'
       AND OLD.active_phase_kind = 'awaiting_runner_recovery'
       AND NEW.state_kind = 'active'
       AND NOT (
            NOT OLD.delegation_runtime_terminal
            AND NEW.delegation_runtime_terminal
            AND (to_jsonb(OLD) - 'delegation_runtime_terminal') =
                (to_jsonb(NEW) - 'delegation_runtime_terminal')
       )

       AND NOT (
            NEW.active_phase_kind = 'awaiting_runner_recovery'
            AND (to_jsonb(OLD) - ARRAY['runner_recovery_runner_id', 'runner_recovery_placement_revision', 'runner_recovery_tool_attempt_id'])
                = (to_jsonb(NEW) - ARRAY['runner_recovery_runner_id', 'runner_recovery_placement_revision', 'runner_recovery_tool_attempt_id'])
            AND EXISTS (
                SELECT 1 FROM runner_recovery_takeover AS takeover
                JOIN runner_session_placement_record AS retained ON retained.session_id = takeover.session_id
                  AND retained.event_ordinal = takeover.retained_loss_event_ordinal
                JOIN runner_lease_generation AS retry ON retry.lease_id = takeover.source_lease_id
                  AND retry.predecessor_generation = takeover.source_generation
                  AND retry.placement_event_ordinal = takeover.successor_event_ordinal
                JOIN runner_current_lease_event AS head USING (lease_id, generation)
                JOIN runner_lease_event AS event ON event.lease_id = head.lease_id AND event.generation = head.generation AND event.event_ordinal = head.event_ordinal
                JOIN runner_current_session_placement AS current_placement ON current_placement.session_id = takeover.session_id
                JOIN runner_session_placement_record AS lost ON lost.session_id = current_placement.session_id
                  AND lost.event_ordinal = current_placement.event_ordinal
                WHERE takeover.session_id = OLD.session_id AND takeover.turn_id = OLD.turn_id
                  AND takeover.interrupted_tool_attempt_id = OLD.runner_recovery_tool_attempt_id
                  AND retained.lost_runner_id = OLD.runner_recovery_runner_id
                  AND retained.placement_revision = OLD.runner_recovery_placement_revision
                  AND lost.state_kind = 'runner_lost' AND lost.lost_runner_id = retry.runner_id
                  AND lost.interrupted_tool_attempt_id = retry.attempt_id
                  AND NEW.runner_recovery_tool_attempt_id = retry.attempt_id
                  AND NEW.runner_recovery_runner_id = lost.lost_runner_id
                  AND NEW.runner_recovery_placement_revision = lost.placement_revision
                  AND event.state_kind IN ('lost_unclaimed', 'lost_claimed', 'lost_execution_possible')
            )
       )
       AND NOT (
            NEW.active_phase_kind = 'awaiting_tool_recovery'
            AND NEW.runner_recovery_runner_id IS NULL AND NEW.runner_recovery_placement_revision IS NULL
            AND NEW.runner_recovery_tool_attempt_id IS NULL
            AND (to_jsonb(OLD) - ARRAY['active_phase_kind', 'current_attempt_id', 'recovery_tool_attempt_id',
                'runner_recovery_runner_id', 'runner_recovery_placement_revision', 'runner_recovery_tool_attempt_id'])
                = (to_jsonb(NEW) - ARRAY['active_phase_kind', 'current_attempt_id', 'recovery_tool_attempt_id',
                'runner_recovery_runner_id', 'runner_recovery_placement_revision', 'runner_recovery_tool_attempt_id'])
            AND EXISTS (
                SELECT 1 FROM runner_recovery_takeover AS takeover
                JOIN runner_lease_generation AS retry ON retry.lease_id = takeover.source_lease_id
                  AND retry.predecessor_generation = takeover.source_generation
                  AND retry.placement_event_ordinal = takeover.successor_event_ordinal
                JOIN runner_current_lease_event AS head USING (lease_id, generation)
                JOIN runner_lease_event AS event USING (lease_id, generation, event_ordinal)
                JOIN tool_attempt AS attempt ON attempt.attempt_id = retry.attempt_id
                WHERE takeover.session_id = OLD.session_id AND takeover.turn_id = OLD.turn_id
                  AND takeover.interrupted_tool_attempt_id = OLD.runner_recovery_tool_attempt_id
                  AND takeover.yielded_turn_attempt_id = NEW.current_attempt_id
                  AND attempt.attempt_id = NEW.recovery_tool_attempt_id
                  AND attempt.issuing_turn_attempt_id = NEW.current_attempt_id
                  AND attempt.state_kind = 'terminal' AND attempt.terminal_disposition_kind = 'ambiguous'
                  AND event.state_kind = 'completed'
            )
       )
       AND NOT (
            NEW.active_phase_kind = 'running'
            AND NEW.runner_recovery_runner_id IS NULL
            AND NEW.runner_recovery_placement_revision IS NULL
            AND NEW.runner_recovery_tool_attempt_id IS NULL
            AND (to_jsonb(OLD) - ARRAY['active_phase_kind', 'current_attempt_id',
                'runner_recovery_runner_id', 'runner_recovery_placement_revision', 'runner_recovery_tool_attempt_id'])
                = (to_jsonb(NEW) - ARRAY['active_phase_kind', 'current_attempt_id',
                'runner_recovery_runner_id', 'runner_recovery_placement_revision', 'runner_recovery_tool_attempt_id'])
            AND EXISTS (
                SELECT 1 FROM runner_recovery_takeover AS takeover
                JOIN runner_current_session_placement AS head USING (session_id)
                JOIN turn_attempt AS continuation
                  ON continuation.continued_from_attempt_id = takeover.yielded_turn_attempt_id
                WHERE takeover.session_id = OLD.session_id AND takeover.turn_id = OLD.turn_id
                  AND takeover.successor_event_ordinal = head.event_ordinal
                  AND takeover.interrupted_tool_attempt_id IS NOT DISTINCT FROM OLD.runner_recovery_tool_attempt_id
                  AND continuation.turn_attempt_id = NEW.current_attempt_id
                  AND continuation.session_id = NEW.session_id AND continuation.turn_id = NEW.turn_id
                  AND continuation.state_kind = 'prepared'
                  AND (takeover.source_lease_id IS NULL OR EXISTS (
                      SELECT 1 FROM runner_current_lease_event AS source_head
                      JOIN runner_lease_event AS source_event USING (lease_id, generation, event_ordinal)
                      WHERE source_head.lease_id = takeover.source_lease_id
                        AND source_head.generation = takeover.source_generation AND source_event.state_kind = 'refused'
                        AND NOT EXISTS (SELECT 1 FROM runner_lease_generation AS later WHERE later.lease_id = takeover.source_lease_id AND later.generation > takeover.source_generation)
                  ) OR EXISTS (
                      SELECT 1 FROM runner_lease_generation AS retry
                      JOIN runner_current_lease_event AS current_lease USING (lease_id, generation)
                      JOIN runner_lease_event AS event ON event.lease_id = current_lease.lease_id AND event.generation = current_lease.generation AND event.event_ordinal = current_lease.event_ordinal
                      WHERE retry.lease_id = takeover.source_lease_id
                        AND retry.predecessor_generation = takeover.source_generation
                        AND retry.placement_event_ordinal = takeover.successor_event_ordinal
                        AND event.state_kind IN ('completed', 'refused')
                  ))
            )
       )
$after$);
END;
$patch$;

DO $patch$
DECLARE definition text;
BEGIN
    SELECT pg_get_functiondef('assert_runner_placement_interrupted_attempt_complete(uuid, numeric)'::regprocedure) INTO definition;
    IF strpos(definition, $before$    IF placement.event_kind <> 'runner_lost'$before$) = 0 THEN
        RAISE EXCEPTION 'runner takeover guard pattern missing: assert_runner_placement_interrupted_attempt_complete(uuid, numeric)';
    END IF;
    EXECUTE replace(definition, $before$    IF placement.event_kind <> 'runner_lost'$before$, $after$
    IF EXISTS (
        SELECT 1 FROM runner_recovery_takeover AS takeover
        JOIN runner_lease_generation AS retry ON retry.lease_id = takeover.source_lease_id
          AND retry.predecessor_generation = takeover.source_generation
          AND retry.placement_event_ordinal = takeover.successor_event_ordinal
        WHERE takeover.session_id = checked_session_id
          AND takeover.retained_loss_event_ordinal = checked_event_ordinal
          AND takeover.interrupted_tool_attempt_id = placement.interrupted_tool_attempt_id
    ) THEN RETURN; END IF;
    IF placement.event_kind <> 'runner_lost'$after$);
END;
$patch$;

-- Retry and preceding attempts retain their issuing identity across the yield.
DO $patch$
DECLARE definition text; before text;
BEGIN
    SELECT pg_get_functiondef('assert_tool_loop_turn_final_state_pre_delegation(uuid)'::regprocedure) INTO definition;
    before := '                               AND attempt.issuing_turn_attempt_id
                                   <> lifecycle.current_attempt_id';
    IF strpos(definition, before) = 0 THEN RAISE EXCEPTION 'runner takeover batch issuing guard missing'; END IF;
    EXECUTE replace(definition, before, before || $after$
                               AND NOT EXISTS (
                                   SELECT 1 FROM runner_recovery_takeover AS takeover
                                   LEFT JOIN tool_attempt AS source ON source.attempt_id = takeover.interrupted_tool_attempt_id
                                   LEFT JOIN tool_request AS source_request ON source_request.request_id = source.request_id
                                   JOIN turn_attempt AS resumed ON resumed.continued_from_attempt_id = takeover.yielded_turn_attempt_id
                                   WHERE takeover.session_id = lifecycle.session_id AND takeover.turn_id = lifecycle.turn_id
                                     AND takeover.producing_model_call_id = lifecycle.active_tool_round_call_id
                                     AND takeover.yielded_turn_attempt_id = attempt.issuing_turn_attempt_id
                                     AND resumed.turn_attempt_id IN (
                                         WITH RECURSIVE ancestry AS (
                                             SELECT turn_attempt_id, continued_from_attempt_id FROM turn_attempt
                                             WHERE turn_attempt_id = lifecycle.current_attempt_id
                                             UNION ALL
                                             SELECT parent.turn_attempt_id, parent.continued_from_attempt_id
                                             FROM turn_attempt AS parent JOIN ancestry ON ancestry.continued_from_attempt_id = parent.turn_attempt_id
                                         ) SELECT turn_attempt_id FROM ancestry
                                     )
                                     AND attempt.state_kind = 'terminal'
                                     AND (takeover.interrupted_tool_attempt_id IS NULL OR request.request_ordinal <= source_request.request_ordinal)
                               )$after$);
END;
$patch$;

-- A stop before retry dispatch authenticates the retained loss, not the new pin.
DO $patch$
DECLARE definition text; before text;
BEGIN
    SELECT pg_get_functiondef('guard_turn_runner_recovery_interrupt_effect()'::regprocedure) INTO definition;
    before := 'placement.event_ordinal = head.event_ordinal';
    IF strpos(definition, before) = 0 THEN RAISE EXCEPTION 'runner interrupt placement guard missing'; END IF;
    definition := replace(definition, before, $after$placement.event_ordinal =
        runner_recovery_loss_ordinal(lifecycle.session_id, lifecycle.turn_id, head.event_ordinal)$after$);
    definition := replace(definition, 'head.event_ordinal = NEW.placement_event_ordinal',
        'placement.event_ordinal = NEW.placement_event_ordinal');
    EXECUTE definition;
END;
$patch$;

-- Cancelling an idempotent retry retains the old ambiguity as physical history.
DO $patch$
DECLARE definition text; before text;
BEGIN
    SELECT pg_get_functiondef('require_interrupt_submit_input_effect_correlation()'::regprocedure) INTO definition;
    before := $before$                           AND stopped_tool.terminal_disposition_kind = 'known_failed'
                           AND stopped_tool.error_kind = 'crash_lost'
                           AND stopped_tool.error_detail IS NULL$before$;
    IF strpos(definition, before) = 0 THEN RAISE EXCEPTION 'runner retry cancellation evidence guard missing'; END IF;
    EXECUTE replace(definition, before, $after$                           AND (
                               (stopped_tool.terminal_disposition_kind = 'known_failed'
                                AND stopped_tool.error_kind = 'crash_lost' AND stopped_tool.error_detail IS NULL)
                               OR (stopped_tool.terminal_disposition_kind = 'ambiguous' AND EXISTS (
                                   SELECT 1 FROM runner_recovery_takeover AS takeover
                                   JOIN runner_lease_generation AS lease ON lease.lease_id = takeover.source_lease_id
                                     AND lease.generation = takeover.source_generation
                                   JOIN semantic_transcript_entry AS entry ON entry.source_session_id = takeover.session_id
                                     AND entry.tool_result_request_id = stopped_tool.request_id
                                     AND entry.payload_kind = 'tool_closed_by_turn_end'
                                   WHERE takeover.session_id = effect.session_id AND takeover.turn_id = effect.turn_id
                                     AND takeover.interrupted_tool_attempt_id = stopped_tool.attempt_id
                                     AND lease.effect_class = 'idempotent'
                                     AND NOT EXISTS (SELECT 1 FROM runner_lease_generation AS retry
                                         WHERE retry.lease_id = takeover.source_lease_id AND retry.generation > takeover.source_generation)
                               ))
                           )$after$);
END;
$patch$;

DO $patch$
DECLARE definition text; before text;
BEGIN
    SELECT pg_get_functiondef('assert_turn_runner_recovery_complete(uuid, uuid)'::regprocedure) INTO definition;
    before := '    IF lifecycle.runner_recovery_tool_attempt_id IS NOT NULL';
    IF strpos(definition, before) = 0 THEN RAISE EXCEPTION 'runner recovery interrupted check missing'; END IF;
    EXECUTE replace(definition, before, $after$
    IF EXISTS (
        SELECT 1 FROM runner_recovery_takeover AS takeover
        JOIN runner_current_session_placement AS placement_head USING (session_id)
        JOIN runner_lease_generation AS retry ON retry.lease_id = takeover.source_lease_id
          AND retry.predecessor_generation = takeover.source_generation
          AND retry.placement_event_ordinal = takeover.successor_event_ordinal
        JOIN runner_current_lease_event AS head USING (lease_id, generation)
        JOIN runner_lease_event AS event ON event.lease_id = head.lease_id AND event.generation = head.generation AND event.event_ordinal = head.event_ordinal
        JOIN runner_current_tool_attempt AS attempt ON attempt.attempt_id = retry.attempt_id
        JOIN tool_request AS request ON request.request_id = attempt.request_id
        WHERE takeover.session_id = checked_session_id AND takeover.turn_id = checked_turn_id
          AND takeover.successor_event_ordinal = placement_head.event_ordinal
          AND takeover.interrupted_tool_attempt_id = lifecycle.runner_recovery_tool_attempt_id
          AND takeover.retained_loss_event_ordinal = placement.event_ordinal
          AND request.producing_model_call_id = lifecycle.active_tool_round_call_id
          AND attempt.issuing_turn_attempt_id = takeover.yielded_turn_attempt_id
          AND attempt.state_kind = 'in_flight' AND event.state_kind IN ('offered', 'claimed')
          AND NOT EXISTS (
              SELECT 1 FROM runner_current_tool_attempt AS other JOIN tool_request AS other_request USING (request_id)
              WHERE other_request.producing_model_call_id = request.producing_model_call_id
                AND other.attempt_id <> attempt.attempt_id
                AND (other.state_kind IN ('prepared', 'in_flight') OR other.terminal_disposition_kind = 'ambiguous')
          )
    ) THEN RETURN; END IF;
$after$ || before);
END;
$patch$;

-- A runner boundary without a tool round still owns its checked continuation.
DO $patch$
DECLARE definition text; before text;
BEGIN
    SELECT pg_get_functiondef('assert_turn_attempt_final_state_before_credential_pools(uuid)'::regprocedure) INTO definition;
    before := '    IF NOT EXISTS (
        SELECT 1
          FROM turn_attempt AS predecessor';
    IF strpos(definition, before) = 0 THEN RAISE EXCEPTION 'turn continuation yield guard missing'; END IF;
    EXECUTE replace(definition, before, $after$    IF NOT EXISTS (
        SELECT 1 FROM runner_recovery_takeover AS takeover
        WHERE takeover.session_id = attempt_record.session_id AND takeover.turn_id = attempt_record.turn_id
          AND takeover.yielded_turn_attempt_id = attempt_record.continued_from_attempt_id
    ) AND NOT EXISTS (
        SELECT 1
          FROM turn_attempt AS predecessor$after$);
END;
$patch$;

DO $patch$
DECLARE definition text; before text;
BEGIN
    SELECT pg_get_functiondef('assert_tool_loop_turn_final_state_pre_delegation(uuid)'::regprocedure) INTO definition;
    before := $before$AND end_disposition IN ('ambiguous', 'lost')$before$;
    IF strpos(definition, before) = 0 THEN RAISE EXCEPTION 'tool recovery issuing end guard missing'; END IF;
    EXECUTE replace(definition, before, $after$AND (end_disposition IN ('ambiguous', 'lost') OR
        (end_disposition = 'yielded_to_durable_wait' AND EXISTS (
            SELECT 1 FROM runner_recovery_takeover AS takeover
            JOIN runner_lease_generation AS retry ON retry.lease_id = takeover.source_lease_id
              AND retry.predecessor_generation = takeover.source_generation
              AND retry.placement_event_ordinal = takeover.successor_event_ordinal
            JOIN runner_current_lease_event AS head USING (lease_id, generation)
            JOIN runner_lease_event AS event USING (lease_id, generation, event_ordinal)
            WHERE takeover.session_id = lifecycle.session_id AND takeover.turn_id = lifecycle.turn_id
              AND takeover.yielded_turn_attempt_id = lifecycle.current_attempt_id
              AND retry.attempt_id = lifecycle.recovery_tool_attempt_id AND event.state_kind = 'completed'
        )))$after$);
END;
$patch$;

-- Retry ambiguity can settle after its issuing turn attempt already yielded.
DO $patch$
DECLARE definition text; before text;
BEGIN
    SELECT pg_get_functiondef('require_interrupt_submit_input_effect_correlation()'::regprocedure) INTO definition;
    before := $before$stopped_attempt.end_disposition IN ('ambiguous', 'lost')$before$;
    IF strpos(definition, before) = 0 THEN RAISE EXCEPTION 'interrupted ambiguity end guard missing'; END IF;
    EXECUTE replace(definition, before, $after$(stopped_attempt.end_disposition IN ('ambiguous', 'lost') OR
        (stopped_attempt.end_disposition = 'yielded_to_durable_wait' AND EXISTS (
            SELECT 1 FROM runner_recovery_takeover AS takeover
            JOIN runner_lease_generation AS retry ON retry.lease_id = takeover.source_lease_id
              AND retry.predecessor_generation = takeover.source_generation
              AND retry.placement_event_ordinal = takeover.successor_event_ordinal
            JOIN runner_current_lease_event AS head USING (lease_id, generation)
            JOIN runner_lease_event AS event USING (lease_id, generation, event_ordinal)
            JOIN turn_lifecycle AS terminal ON terminal.session_id = takeover.session_id AND terminal.turn_id = takeover.turn_id
            JOIN tool_attempt AS attempt ON attempt.attempt_id = retry.attempt_id
            WHERE takeover.session_id = stopped_attempt.session_id AND takeover.turn_id = stopped_attempt.turn_id
              AND takeover.yielded_turn_attempt_id = stopped_attempt.turn_attempt_id
              AND event.state_kind = 'completed' AND attempt.state_kind = 'terminal'
              AND attempt.terminal_disposition_kind = 'ambiguous'
              AND terminal.terminal_tool_attempt_id = attempt.attempt_id
        )))$after$);
END;
$patch$;

-- Terminal takeover relocations follow the ordered results inside the terminal frontier.
DO $patch$
DECLARE definition text; before text;
BEGIN
    SELECT pg_get_functiondef('require_runner_placement_boundary()'::regprocedure) INTO definition;
    before := $before$    THEN
        RAISE EXCEPTION 'placement boundary precedes complete batch results'$before$;
    IF strpos(definition, before) = 0 THEN RAISE EXCEPTION 'terminal placement result guard missing'; END IF;
    definition := replace(definition, before, $after$       AND NOT EXISTS (
            SELECT 1 FROM runner_recovery_takeover AS takeover
            JOIN turn_lifecycle AS terminal USING (session_id, turn_id)
            JOIN tool_round AS round ON round.producing_model_call_id = takeover.producing_model_call_id
            JOIN context_frontier AS source ON source.owning_session_id = round.session_id
              AND source.context_frontier_id = round.boundary_frontier_id
            WHERE takeover.command_id = NEW.command_id AND takeover.session_id = NEW.session_id
              AND terminal.state_kind = 'terminal'
              AND context_frontier_preserves_prefix(NEW.session_id, NEW.context_frontier_id, terminal.terminal_frontier_id)
              AND NOT EXISTS (
                  SELECT 1 FROM tool_request AS request
                  WHERE request.producing_model_call_id = round.producing_model_call_id
                    AND NOT EXISTS (
                        SELECT 1 FROM resolve_context_frontier_members(NEW.session_id, NEW.context_frontier_id) AS member
                        JOIN semantic_transcript_entry AS result USING (source_session_id, semantic_entry_id)
                        LEFT JOIN tool_attempt AS attempt ON attempt.attempt_id = result.tool_result_attempt_id
                        WHERE member.member_position = source.member_count + request.request_ordinal + 1
                          AND COALESCE(result.tool_result_request_id, attempt.request_id) = request.request_id
                          AND result.payload_kind IN ('tool_execution_result', 'tool_closed_by_turn_end', 'tool_denied', 'tool_inadmissible', 'delegation_result')
                    )
              )
       )
    THEN
        RAISE EXCEPTION 'placement boundary precedes complete batch results'$after$);
    before := '        UNION SELECT seed_context_frontier_id FROM imported_session_seed WHERE session_id = NEW.session_id';
    IF strpos(definition, before) = 0 THEN RAISE EXCEPTION 'terminal placement prefix guard missing'; END IF;
    definition := replace(definition, before, $after$        UNION SELECT boundary.prefix_context_frontier_id FROM runner_recovery_takeover AS takeover
            JOIN turn_lifecycle AS terminal USING (session_id, turn_id)
            JOIN context_frontier AS boundary ON boundary.owning_session_id = takeover.session_id
              AND boundary.context_frontier_id = NEW.context_frontier_id
            WHERE takeover.command_id = NEW.command_id AND takeover.session_id = NEW.session_id
              AND terminal.state_kind = 'terminal'
              AND context_frontier_preserves_prefix(NEW.session_id, NEW.context_frontier_id, terminal.terminal_frontier_id)
$after$ || before);
    EXECUTE definition;
END;
$patch$;

-- A takeover continuation retains the producing round across its resumed attempt.
CREATE OR REPLACE FUNCTION tool_round_call_from_predecessor(checked_predecessor uuid, checked_turn uuid, checked_session uuid)
RETURNS uuid LANGUAGE sql STABLE AS $$
    WITH RECURSIVE history AS (
        SELECT turn_attempt_id, continued_from_attempt_id
          FROM turn_attempt
         WHERE turn_attempt_id = checked_predecessor
           AND turn_id = checked_turn AND session_id = checked_session
        UNION
        SELECT predecessor.turn_attempt_id, predecessor.continued_from_attempt_id
          FROM turn_attempt AS predecessor
          JOIN history AS successor
            ON predecessor.turn_attempt_id = successor.continued_from_attempt_id
         WHERE predecessor.turn_id = checked_turn AND predecessor.session_id = checked_session
           AND NOT EXISTS (SELECT 1 FROM model_call WHERE turn_attempt_id = successor.turn_attempt_id)
           AND (EXISTS (
                SELECT 1 FROM tool_attempt AS waiting
                 WHERE waiting.issuing_turn_attempt_id = successor.turn_attempt_id
                   AND waiting.turn_id = checked_turn AND waiting.session_id = checked_session
                   AND waiting.state_kind = 'terminal'
                   AND waiting.terminal_disposition_kind = 'awaiting_child'
           ) OR EXISTS (
                SELECT 1 FROM runner_recovery_takeover AS takeover
                WHERE takeover.session_id = checked_session AND takeover.turn_id = checked_turn
                  AND takeover.yielded_turn_attempt_id = successor.turn_attempt_id
                  AND takeover.producing_model_call_id IS NOT NULL
           ))
    )
    SELECT call.model_call_id
      FROM history JOIN model_call AS call USING (turn_attempt_id)
      JOIN tool_round AS round ON round.producing_model_call_id = call.model_call_id
     WHERE call.turn_id = checked_turn AND call.session_id = checked_session
       AND call.state_kind = 'terminal' AND call.terminal_disposition_kind = 'completed'
       AND round.boundary_kind = 'continuing';
$$;

-- A no-tool takeover resumes the first call without inventing a predecessor round.
DO $patch$
DECLARE definition text; before text;
BEGIN
    SELECT pg_get_functiondef('assert_model_call_steering_final_state(uuid)'::regprocedure) INTO definition;
    before := '    SELECT member_count
      INTO starting_count';
    IF strpos(definition, before) = 0 THEN RAISE EXCEPTION 'initial model frontier guard missing'; END IF;
    definition := replace(definition, before, $after$    IF EXISTS (
        SELECT 1 FROM model_call AS call
        JOIN turn_attempt AS attempt ON attempt.turn_attempt_id = call.turn_attempt_id
        JOIN runner_recovery_takeover AS takeover
          ON takeover.yielded_turn_attempt_id = attempt.continued_from_attempt_id
        WHERE call.model_call_id = checked_model_call_id
          AND takeover.session_id = checked_session AND takeover.turn_id = checked_turn
          AND takeover.producing_model_call_id IS NULL
    ) THEN
        predecessor_attempt := NULL;
    END IF;
$after$ || before);
    EXECUTE definition;

    SELECT pg_get_functiondef('require_runner_placement_boundary()'::regprocedure) INTO definition;
    before := '        UNION SELECT seed_context_frontier_id FROM imported_session_seed WHERE session_id = NEW.session_id';
    IF strpos(definition, before) = 0 THEN RAISE EXCEPTION 'initial runner boundary prefix guard missing'; END IF;
    definition := replace(definition, before, $after$        UNION SELECT turn.starting_frontier_id
            FROM runner_recovery_takeover AS takeover
            JOIN turn_lifecycle AS turn USING (session_id, turn_id)
            WHERE takeover.command_id = NEW.command_id AND takeover.session_id = NEW.session_id
              AND takeover.producing_model_call_id IS NULL
$after$ || before);
    EXECUTE definition;
END;
$patch$;

-- No-tool recovery retains only its yielded predecessors and exact terminal successor.
CREATE FUNCTION runner_takeover_terminal_history_is_valid(subject uuid)
RETURNS boolean LANGUAGE sql STABLE AS $$
    WITH RECURSIVE history AS (
        SELECT attempt.*, lifecycle.terminal_attempt_id FROM turn_attempt AS attempt
        JOIN turn_lifecycle AS lifecycle ON lifecycle.terminal_attempt_id = attempt.turn_attempt_id
          AND lifecycle.turn_id = attempt.turn_id AND lifecycle.session_id = attempt.session_id
        WHERE lifecycle.turn_id = subject AND lifecycle.state_kind = 'terminal'
        UNION
        SELECT predecessor.*, successor.terminal_attempt_id FROM turn_attempt AS predecessor
        JOIN history AS successor ON predecessor.turn_attempt_id = successor.continued_from_attempt_id
          AND predecessor.turn_id = successor.turn_id AND predecessor.session_id = successor.session_id
    )
    SELECT count(*) = (SELECT count(*) FROM turn_attempt WHERE turn_id = subject)
      AND count(*) FILTER (WHERE continued_from_attempt_id IS NULL) = 1
      AND count(*) > 1
      AND bool_and(state_kind = 'ended' AND (turn_attempt_id = terminal_attempt_id OR (
          end_variant = 'without_stop' AND end_disposition = 'yielded_to_durable_wait'
          AND NOT EXISTS (SELECT 1 FROM model_call WHERE turn_attempt_id = history.turn_attempt_id)
          AND EXISTS (SELECT 1 FROM runner_recovery_takeover AS takeover
              WHERE takeover.session_id = history.session_id AND takeover.turn_id = history.turn_id
                AND takeover.yielded_turn_attempt_id = history.turn_attempt_id
                AND takeover.producing_model_call_id IS NULL)
      )))
    FROM history
$$;

DO $patch$
DECLARE definition text; before text;
BEGIN
    SELECT pg_get_functiondef('assert_terminal_started_turn_common_final_state(uuid)'::regprocedure) INTO definition;
    before := 'NOT credential_wait_terminal_history_is_valid(checked_turn_id)';
    IF strpos(definition, before) = 0 THEN RAISE EXCEPTION 'terminal attempt history guard missing'; END IF;
    EXECUTE replace(definition, before, before || ' AND NOT runner_takeover_terminal_history_is_valid(checked_turn_id)');
END;
$patch$;

DO $patch$
DECLARE definition text; before text;
BEGIN
    SELECT pg_get_functiondef('assert_cancelled_turn_final_state(uuid)'::regprocedure) INTO definition;
    before := '    ELSIF checked_terminal_call IS NULL THEN';
    IF strpos(definition, before) = 0 THEN RAISE EXCEPTION 'direct cancellation frontier guard missing'; END IF;
    EXECUTE replace(definition, before, $after$    ELSIF checked_terminal_call IS NULL AND call_count = 0
        AND runner_takeover_terminal_history_is_valid(checked_turn_id) THEN
        SELECT COALESCE(boundary.context_frontier_id, checked_starting_frontier) INTO base_frontier
        FROM turn_attempt AS terminal
        JOIN runner_recovery_takeover AS takeover ON takeover.yielded_turn_attempt_id = terminal.continued_from_attempt_id
          AND takeover.session_id = terminal.session_id AND takeover.turn_id = terminal.turn_id
        LEFT JOIN runner_placement_boundary AS boundary ON boundary.command_id = takeover.command_id
        WHERE terminal.turn_attempt_id = checked_terminal_attempt AND takeover.producing_model_call_id IS NULL;
$after$ || before);
END;
$patch$;
