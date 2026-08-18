-- A goal turn may be a turn a durable command already accepted.
--
-- Supersedes the `require_goal_turn_shape` and `require_accepted_input_source`
-- definitions installed by 202608020013_goal_mode. Both are restated in full
-- below, because a plpgsql function has no partial replacement.
--
-- Repository-watch dispatch submits its tagged context through submit_input and
-- then commissions the session's goal. Until now the commission could not claim
-- that submitted turn, because a goal turn's accepted input had to carry no
-- accepting command and had to restate the generation's statement verbatim. The
-- commission therefore minted a second queued turn holding the statement alone,
-- and once the first turn terminalized a pursuing goal made the second
-- runtime-relevant, so one dispatched event ran its template twice.
--
-- Two rules relax, each by exactly the one shape that admits a bound turn. Both
-- relaxations only widen: every shape admitted before this migration is still
-- admitted, so no stored row is invalidated and no data changes here.
--
-- First, `goal_turn_runtime_shape` no longer rejects a goal turn whose accepted
-- input names an accepting command. Every other clause of that check is
-- unchanged, so a bound turn must still be an ordinary queued turn origin at
-- its exact defaults epoch, with the session's default model, no active-turn
-- expectation, and no per-input replacement.
--
-- Second, `goal_turn_input_content` applies its verbatim-restatement rule only
-- to a goal turn whose accepted input has no accepting command. That is the
-- narrowest rule that admits the dispatch shape. A goal turn arises exactly two
-- ways: the goal machinery schedules it, minting a commandless accepted input
-- whose text the machinery itself authors from the generation's immutable
-- source; or an existing turn is bound to the generation, and that turn's input
-- was authored by whoever issued its command. The verbatim rule is what proves
-- the machinery did not invent text in the first case, and it can prove nothing
-- in the second, where the text is a dispatched session's tagged-context JSON
-- rather than the statement. So a goal turn either restates its statement or
-- carries a command, and never neither.
--
-- Arbitrary text still cannot claim to be a goal turn. `accepting_command_id`
-- is a foreign key to a recorded durable command receipt rather than a flag,
-- and every other proof a goal turn owes is untouched: it must name the current
-- pursuing generation, reverse-correlate one-to-one to a pursuing user event or
-- a completed predecessor in its own generation, and satisfy the full queued
-- shape above. The relaxation admits one further shape, not an unchecked one.
-- The `expected_content IS NULL` half of the rule also stays for both shapes, so
-- a bound turn still proves its generation has an immutable source to run under.

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
        OR accepted.content_kind <> 'text'
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
                AND queued.frozen_direct_model_selection_id
                    = queued.requested_direct_model_selection_id)
            OR (queued.requested_model_kind = 'alias'
                AND queued.frozen_model_kind = 'frozen_alias'
                AND queued.frozen_model_alias_id = queued.requested_model_alias_id)
        )
        OR queued.model_parameters <> 'provider_defaults'
        OR queued.known_provider_failure_retry <> 'disabled'
        OR queued.model_fallback <> 'disabled'
        OR queued.dangerous_tool_auto_approval
            <> defaults.dangerous_tool_auto_approval
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
            RAISE EXCEPTION 'first goal turn generation disagrees with its user event'
                USING ERRCODE = '23514', CONSTRAINT = 'goal_turn_source_generation';
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
            RAISE EXCEPTION 'goal continuation requires a successfully completed predecessor'
                USING ERRCODE = '23514', CONSTRAINT = 'goal_turn_completed_predecessor';
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
            RAISE EXCEPTION 'goal continuation requires the latest accepted goal turn'
                USING ERRCODE = '23514', CONSTRAINT = 'goal_turn_latest_predecessor';
        END IF;
        SELECT statement INTO expected_content FROM goal_event
         WHERE session_id = NEW.session_id
           AND event_kind IN ('commissioned', 'superseded')
         ORDER BY event_ordinal DESC LIMIT 1;
    END IF;

    IF expected_content IS NULL
        OR (
            accepted.accepting_command_id IS NULL
            AND accepted.content_text <> expected_content
        )
    THEN
        RAISE EXCEPTION 'goal turn input does not match its immutable source'
            USING ERRCODE = '23514', CONSTRAINT = 'goal_turn_input_content';
    END IF;
    RETURN NULL;
END;
$$;

-- An accepted input with no accepting command still requires exactly one goal
-- source, which is what admits the null command the goal machinery writes. The
-- converse — a commanded input having no goal source — is what a bound turn
-- contradicts, and it is dropped rather than loosened: `goal_turn` already
-- carries a UNIQUE on `accepted_input_id`, so no accepted input can name more
-- than one generation whether or not it carries a command.
CREATE OR REPLACE FUNCTION require_accepted_input_source()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE goal_sources bigint;
BEGIN
    SELECT count(*) INTO goal_sources FROM goal_turn
     WHERE accepted_input_id = NEW.accepted_input_id;
    IF NEW.accepting_command_id IS NULL AND goal_sources <> 1 THEN
        RAISE EXCEPTION 'accepted input without a command requires exactly one goal source'
            USING ERRCODE = '23514', CONSTRAINT = 'accepted_input_source_closed';
    END IF;
    RETURN NULL;
END;
$$;
