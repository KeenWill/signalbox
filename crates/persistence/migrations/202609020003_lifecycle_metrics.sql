--
-- Session lifecycle §12: the five metrics and the counts beside them, as
-- views over the durable columns §1–§6 landed.
--

--
-- §12's denominator keeps a supersession that closed a park holding a failure
-- cause, so the standing cause must outlive the park that raised it: through
-- terminalization, and through the resume a committed closure survives between
-- its decision and the turn's boundary. A state that owes neither still cannot
-- carry one.
--

-- Supersedes the definition in `202609020002_session_lifecycle_satellite.sql`.
ALTER TABLE session_lifecycle
    DROP CONSTRAINT session_lifecycle_parked_shape;

ALTER TABLE session_lifecycle
    ADD CONSTRAINT session_lifecycle_parked_shape CHECK (
        ((state_kind = 'parked'::text)
            = ((parked_cause IS NOT NULL)
               AND (parked_responder IS NOT NULL)
               AND (parked_since IS NOT NULL)))
        AND ((parked_responder IS NULL) OR (parked_responder = ANY (ARRAY[
            'operator'::text,
            'repo_watch'::text,
            'commissioned_dispatch'::text
        ])))
        -- The park itself never outlives the state: only the instant and the
        -- standing evidence carry, and only into a state that owes an outcome.
        AND ((state_kind = 'parked'::text)
             OR ((parked_cause IS NULL) AND (parked_responder IS NULL)))
        AND ((state_kind = 'parked'::text)
             OR (state_kind = 'terminal'::text)
             OR (pending_terminal_outcome_kind IS NOT NULL)
             OR ((parked_since IS NULL)
                 AND (parked_standing_cause_kind IS NULL)))
        AND ((parked_standing_cause_kind IS NULL)
             OR (parked_cause IS NOT NULL)
             OR (state_kind = 'terminal'::text)
             OR (pending_terminal_outcome_kind IS NOT NULL))
        -- The standing evidence is the evidence the cause names. A closure
        -- reads it to classify the outcome, so a park holding evidence its own
        -- cause contradicts -- or an exhaustion holding none at all -- closes
        -- under a classification the park never supported.
        AND ((parked_cause IS NULL)
             OR ((parked_cause = 'retry_budget_exhausted'::text)
                 AND (parked_standing_cause_kind = ANY (ARRAY[
                    'provider_transient'::text,
                    'provider_quota_exhausted'::text,
                    'provider_overloaded'::text,
                    'infrastructure_failure'::text,
                    'retry_budget_exhausted'::text
                 ])))
             OR ((parked_cause = 'structural_failure'::text)
                 AND (parked_standing_cause_kind = ANY (ARRAY[
                    'context_compaction_wall'::text,
                    'context_headroom_exhausted'::text,
                    'broken_toolchain'::text,
                    'moderation_block'::text
                 ])))
             OR ((parked_cause <> ALL (ARRAY[
                    'retry_budget_exhausted'::text,
                    'structural_failure'::text
                 ]))
                 AND (parked_standing_cause_kind IS NULL)))
    );

--
-- Weeks are UTC weeks: `date_trunc` on a `timestamptz` answers in the reader's
-- `TimeZone`, which would put one row in two different weeks.
--

CREATE FUNCTION session_lifecycle_metric_week(moment timestamp with time zone)
    RETURNS timestamp without time zone
    LANGUAGE sql
    IMMUTABLE
    AS $$
    SELECT date_trunc('week', moment AT TIME ZONE 'UTC');
$$;

--
-- §12's terminal cohort. Membership follows the journaled ownership record
-- rather than the current bit, so a release never removes a session from the
-- cohort. The trim and the numerator are recorded per session here rather
-- than restated at every reader.
--

CREATE VIEW session_lifecycle_terminal_cohort AS
SELECT lifecycle.session_id,
       session_lifecycle_metric_week(lifecycle.ended_at) AS cohort_week,
       lifecycle.ended_at,
       lifecycle.terminal_outcome_kind,
       lifecycle.terminal_cause_kind,
       lifecycle.parked_standing_cause_kind,
       ((lifecycle.terminal_outcome_kind = 'superseded'::text)
        AND (lifecycle.parked_standing_cause_kind IS NOT NULL))
           AS failure_driven_supersession,
       ((lifecycle.terminal_outcome_kind <> 'stopped'::text)
        AND ((lifecycle.terminal_outcome_kind <> 'superseded'::text)
             OR (lifecycle.parked_standing_cause_kind IS NOT NULL)))
           AS counts_in_denominator,
       ((lifecycle.terminal_outcome_kind = ANY (ARRAY[
            'failed_retryable'::text,
            'failed_structural'::text,
            'failed_unknown'::text,
            'abandoned'::text,
            'retired'::text
        ]))
        OR ((lifecycle.terminal_outcome_kind = 'superseded'::text)
            AND (lifecycle.parked_standing_cause_kind IS NOT NULL)))
           AS counts_in_numerator
  FROM session_lifecycle AS lifecycle
 WHERE lifecycle.state_kind = 'terminal'::text
   AND EXISTS (
           SELECT 1
             FROM session_ownership_event AS journal
            WHERE journal.session_id = lifecycle.session_id
              AND journal.owned_after
              -- Owned "at any point in their life", so an adoption recorded
              -- after the closure would write a session into a week that had
              -- already been reported without it.
              AND journal.recorded_at <= lifecycle.ended_at
       );

--
-- Overflow is a turn cause (§12: "on any turn"). A wall has three durable
-- spellings — the turn cause, the session's structural terminal cause, and a
-- park's standing cause — and any of them is the session recording one.
--

CREATE VIEW session_lifecycle_cause_incidence AS
SELECT session_row.session_id,
       EXISTS (
           SELECT 1
             FROM turn_lifecycle AS turn
            WHERE turn.session_id = session_row.session_id
              AND turn.terminal_cause_kind = 'context_headroom_exhausted'::text
       ) AS recorded_context_headroom_exhausted,
       (EXISTS (
            SELECT 1
              FROM turn_lifecycle AS turn
             WHERE turn.session_id = session_row.session_id
               AND turn.terminal_cause_kind = 'context_compaction_wall'::text
        )
        OR EXISTS (
            SELECT 1
              FROM session_lifecycle AS lifecycle
             WHERE lifecycle.session_id = session_row.session_id
               AND 'context_compaction_wall'::text = ANY (ARRAY[
                       lifecycle.terminal_cause_kind,
                       lifecycle.parked_standing_cause_kind
                   ])
        )) AS recorded_context_compaction_wall
  FROM session AS session_row;

--
-- §12's dispatch cohort, for `wall_rate`. A session enters `dispatched` when
-- its first turn is queued (§1), and a turn row's write time is immutable, so
-- the earliest one is the durable dispatch instant; `state_entered_at` moves
-- with every later transition and is not.
--

CREATE VIEW session_lifecycle_dispatch_cohort AS
SELECT dispatched.session_id,
       session_lifecycle_metric_week(dispatched.dispatched_at) AS dispatch_week,
       dispatched.dispatched_at,
       (lifecycle.state_kind = 'terminal'::text) AS terminal,
       incidence.recorded_context_compaction_wall AS wall
  FROM (
        SELECT turn.session_id, min(turn.recorded_at) AS dispatched_at
          FROM turn_lifecycle AS turn
         GROUP BY turn.session_id
       ) AS dispatched
  JOIN session_lifecycle AS lifecycle
    ON lifecycle.session_id = dispatched.session_id
  JOIN session_lifecycle_cause_incidence AS incidence
    ON incidence.session_id = dispatched.session_id;

--
-- §12's `cause_completeness`, both axes. The turn axis measures causes outside
-- the catch-all set, whose sole member is `unclassified_failure`. The
-- model-call axis measures over `known_failed` calls only — the one
-- disposition the schema admits a cause on — with `unrecognized` and a null
-- cause as its catch-all; an attachment-preparation cause counts as typed.
--
-- §12 defines both over their whole population rather than over a cohort. The
-- week here is the row's own write week, since §3 stamps no terminalization
-- instant, so a row's week says when the work was written and not when it
-- settled.
--

CREATE VIEW session_lifecycle_terminal_turn_cause AS
SELECT turn.session_id,
       turn.turn_id,
       session_lifecycle_metric_week(turn.recorded_at) AS recorded_week,
       (turn.terminal_cause_kind IS NOT NULL
        AND turn.terminal_cause_kind <> 'unclassified_failure'::text) AS classified
  FROM turn_lifecycle AS turn
 WHERE turn.state_kind = 'terminal'::text;

CREATE VIEW session_lifecycle_known_failed_call_cause AS
SELECT call.session_id,
       call.model_call_id,
       session_lifecycle_metric_week(call.recorded_at) AS recorded_week,
       ((call.terminal_provider_failure_cause IS NOT NULL
         AND call.terminal_provider_failure_cause <> 'unrecognized'::text)
        OR (call.terminal_attachment_preparation_failure_cause IS NOT NULL))
           AS classified
  FROM model_call AS call
 WHERE call.state_kind = 'terminal'::text
   AND call.terminal_disposition_kind = 'known_failed'::text;

--
-- The weekly report: one row per calendar week any cohort has a member in,
-- each metric as its exact pair of counts. Rates are left to the reader, so an
-- empty denominator reports no rate rather than a zero.
--

CREATE VIEW session_lifecycle_weekly_metric AS
WITH wall_occurrence AS (
    -- F9's immediate half: a wall belongs to the week it happened in. §2 parks
    -- a session on a wall and suspends its turn, so the park is the evidence
    -- and `parked_since` the instant; terminalization carries both forward.
    -- The park therefore dates the occurrence wherever it exists, and a later
    -- terminal turn naming the same wall never moves it. A turn cause is the
    -- next evidence, for a wall that ended a turn without parking the session,
    -- at that row's write week; a session closed on a wall its turn never
    -- named is the last, at its closure. The sources are the ones the
    -- numerator counts, so a walled session always has an occurrence to show.
    -- One session's wall is one occurrence.
    SELECT session_row.session_id,
           COALESCE(
               (SELECT lifecycle.parked_since
                  FROM session_lifecycle AS lifecycle
                 WHERE lifecycle.session_id = session_row.session_id
                   AND lifecycle.parked_standing_cause_kind
                       = 'context_compaction_wall'::text
                   AND lifecycle.parked_since IS NOT NULL),
               (SELECT min(turn.recorded_at)
                  FROM turn_lifecycle AS turn
                 WHERE turn.session_id = session_row.session_id
                   AND turn.terminal_cause_kind = 'context_compaction_wall'::text),
               (SELECT lifecycle.ended_at
                  FROM session_lifecycle AS lifecycle
                 WHERE lifecycle.session_id = session_row.session_id
                   AND lifecycle.terminal_cause_kind
                       = 'context_compaction_wall'::text)
           ) AS occurred_at
      FROM session AS session_row
), weeks AS (
    SELECT cohort_week AS week FROM session_lifecycle_terminal_cohort
     UNION
    SELECT dispatch_week AS week FROM session_lifecycle_dispatch_cohort
     UNION
    SELECT recorded_week AS week FROM session_lifecycle_terminal_turn_cause
     UNION
    SELECT recorded_week AS week FROM session_lifecycle_known_failed_call_cause
     UNION
    SELECT session_lifecycle_metric_week(occurred_at) AS week
      FROM wall_occurrence
     WHERE occurred_at IS NOT NULL
), terminal AS (
    SELECT cohort.cohort_week AS week,
           count(*) AS cohort_size,
           count(*) FILTER (WHERE cohort.counts_in_denominator)
               AS completion_failure_denominator,
           count(*) FILTER (WHERE cohort.counts_in_numerator)
               AS completion_failure_numerator,
           count(*) FILTER (WHERE cohort.terminal_outcome_kind = 'failed_unknown'::text)
               AS failed_unknown,
           count(*) FILTER (WHERE incidence.recorded_context_headroom_exhausted)
               AS overflow,
           count(*) FILTER (WHERE incidence.recorded_context_headroom_exhausted
                              AND cohort.terminal_outcome_kind = 'achieved_verified'::text)
               AS overflow_finished
      FROM session_lifecycle_terminal_cohort AS cohort
      JOIN session_lifecycle_cause_incidence AS incidence
        ON incidence.session_id = cohort.session_id
     GROUP BY cohort.cohort_week
), dispatched AS (
    SELECT cohort.dispatch_week AS week,
           count(*) AS cohort_size,
           count(*) FILTER (WHERE cohort.wall) AS wall
      FROM session_lifecycle_dispatch_cohort AS cohort
     GROUP BY cohort.dispatch_week
), walls_recorded AS (
    SELECT session_lifecycle_metric_week(occurrence.occurred_at) AS week,
           count(*) AS occurrences
      FROM wall_occurrence AS occurrence
     WHERE occurrence.occurred_at IS NOT NULL
     GROUP BY session_lifecycle_metric_week(occurrence.occurred_at)
), turn_causes AS (
    SELECT cause.recorded_week AS week,
           count(*) AS terminal_turns,
           count(*) FILTER (WHERE cause.classified) AS classified_turns
      FROM session_lifecycle_terminal_turn_cause AS cause
     GROUP BY cause.recorded_week
), call_causes AS (
    SELECT cause.recorded_week AS week,
           count(*) AS known_failed_calls,
           count(*) FILTER (WHERE cause.classified) AS classified_calls
      FROM session_lifecycle_known_failed_call_cause AS cause
     GROUP BY cause.recorded_week
)
SELECT weeks.week,
       COALESCE(terminal.cohort_size, 0) AS terminal_cohort_size,
       COALESCE(terminal.completion_failure_denominator, 0)
           AS completion_failure_denominator,
       COALESCE(terminal.completion_failure_numerator, 0)
           AS completion_failure_numerator,
       COALESCE(terminal.failed_unknown, 0) AS failed_unknown_count,
       COALESCE(terminal.overflow, 0) AS overflow_count,
       COALESCE(terminal.overflow_finished, 0) AS overflow_finished_count,
       COALESCE(dispatched.cohort_size, 0) AS dispatch_cohort_size,
       COALESCE(dispatched.wall, 0) AS wall_count,
       COALESCE(walls_recorded.occurrences, 0) AS wall_occurrence_count,
       COALESCE(turn_causes.terminal_turns, 0) AS terminal_turn_count,
       COALESCE(turn_causes.classified_turns, 0) AS classified_terminal_turn_count,
       COALESCE(call_causes.known_failed_calls, 0) AS known_failed_call_count,
       COALESCE(call_causes.classified_calls, 0) AS classified_known_failed_call_count
  FROM weeks
  LEFT JOIN terminal ON terminal.week = weeks.week
  LEFT JOIN dispatched ON dispatched.week = weeks.week
  LEFT JOIN walls_recorded ON walls_recorded.week = weeks.week
  LEFT JOIN turn_causes ON turn_causes.week = weeks.week
  LEFT JOIN call_causes ON call_causes.week = weeks.week;

--
-- §12's first companion count, target zero. An unbounded deadline —
-- `expires_at` null — is never counted; a missing record always is.
--

CREATE VIEW session_lifecycle_deadline_violation AS
SELECT lifecycle.session_id,
       lifecycle.state_kind,
       lifecycle.state_entered_at,
       deadline.deadline_kind,
       deadline.expires_at,
       (deadline.session_id IS NULL) AS deadline_missing
  FROM session_lifecycle AS lifecycle
  LEFT JOIN session_deadline AS deadline
    ON deadline.session_id = lifecycle.session_id
 WHERE lifecycle.state_kind <> 'terminal'::text
   AND lifecycle.owned
   AND ((deadline.session_id IS NULL)
        OR ((deadline.expires_at IS NOT NULL)
            AND (deadline.expires_at < clock_timestamp())));


--
-- A park, a resume, a closure, and an ownership flip change the attention
-- state without writing a turn, goal, runner, or metadata fact, so a client
-- following a cursor would keep the state it last read. The mapped
-- transitions are excluded because the turn or goal write that produced them
-- journals its own change.
--
-- The kind is its own: `session` means the fleet's membership moved and sends
-- every follower back for the whole catalog, which a park is not.
--

-- Supersedes the definition in `202609010013_operator_attention.sql`.
ALTER TABLE operator_attention_change
    DROP CONSTRAINT operator_attention_change_fact_kind_check;

ALTER TABLE operator_attention_change
    ADD CONSTRAINT operator_attention_change_fact_kind_check CHECK (
        fact_kind = ANY (ARRAY[
            'session'::text,
            'lifecycle'::text,
            'turn'::text,
            'goal'::text,
            'approval_judge'::text,
            'runner'::text
        ])
    );

CREATE FUNCTION record_operator_attention_lifecycle_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO operator_attention_change (session_id, fact_kind)
    VALUES (NEW.session_id, 'lifecycle');
    RETURN NULL;
END;
$$;

CREATE TRIGGER session_lifecycle_records_operator_attention_change
    AFTER UPDATE OF state_kind, owned ON session_lifecycle
    FOR EACH ROW
    WHEN (
        (OLD.owned IS DISTINCT FROM NEW.owned)
        OR (OLD.state_kind = 'parked'::text)
        OR (NEW.state_kind = ANY (ARRAY['parked'::text, 'terminal'::text]))
    )
    EXECUTE FUNCTION record_operator_attention_lifecycle_change();
