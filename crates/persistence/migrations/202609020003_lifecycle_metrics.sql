--
-- Session lifecycle §12: the five metrics, the two companion alarms, and the
-- gate, defined on durable columns rather than proxies.
--
-- Every definition below is a view over the columns §3, §4 and §1/§2/§6
-- landed: `session_lifecycle`, `session_deadline`, `session_ownership_event`,
-- `turn_lifecycle.terminal_cause_kind`, and `model_call`'s provider cause. No
-- metric reads a log line, an outbox payload, or a derived classifier, so the
-- operator surface and the Prometheus gauges are the same SQL and cannot
-- disagree about a number the gate turns on.
--

--
-- A supersession's standing failure cause has to survive the terminalization
-- that reads it.
--
-- §12's denominator keeps a `superseded{by}` that closed a park holding a
-- failure cause — otherwise every failure recovered by respawn-fresh vanishes
-- from the gate. The park's standing cause is that evidence, and the shape
-- constraint as landed erases it at terminalization, because `parked_cause`
-- must be null once the state is no longer `parked`. Widening the clause by
-- exactly the terminal state keeps the record and changes nothing else: a
-- non-terminal, non-parked state still cannot carry a standing cause.
--

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
        AND ((parked_standing_cause_kind IS NULL)
             OR (parked_cause IS NOT NULL)
             OR (state_kind = 'terminal'::text))
    );

--
-- Deployment policy for the metric bounds.
--
-- The same arrangement §1's deadline bounds use: the daemon writes its
-- `[numeric_bounds]` policy here at startup and the definitions read it, so a
-- view is a total statement of the metric rather than a fragment a caller
-- completes with a parameter. A row with both value columns null is the
-- explicit `none` marker the config idiom already spells.
--
-- Rate thresholds are integer parts per million, not floats: every rate below
-- is a pair of exact counts, and comparing exact counts against an exact
-- threshold is what keeps a gate verdict reproducible.
--

CREATE TABLE session_lifecycle_metric_bound (
    bound_kind text NOT NULL,
    interval_bound interval,
    count_bound bigint,
    updated_at timestamp with time zone DEFAULT statement_timestamp() NOT NULL,

    CONSTRAINT session_lifecycle_metric_bound_pkey PRIMARY KEY (bound_kind),

    CONSTRAINT session_lifecycle_metric_bound_kind_closed CHECK (
        bound_kind = ANY (ARRAY[
            'deadline_processing_grace'::text,
            'wall_cohort_maturation'::text,
            'gate_weeks'::text,
            'completion_failure_rate_threshold_ppm'::text,
            'wall_rate_threshold_ppm'::text,
            'failed_unknown_share_threshold_ppm'::text
        ])
    ),

    -- Each bound is one kind of number, and the row carries that kind or
    -- nothing at all.
    CONSTRAINT session_lifecycle_metric_bound_shape CHECK (
        ((bound_kind = ANY (ARRAY[
            'deadline_processing_grace'::text,
            'wall_cohort_maturation'::text
         ])) AND (count_bound IS NULL))
        OR ((bound_kind = ANY (ARRAY[
            'gate_weeks'::text,
            'completion_failure_rate_threshold_ppm'::text,
            'wall_rate_threshold_ppm'::text,
            'failed_unknown_share_threshold_ppm'::text
         ])) AND (interval_bound IS NULL))
    ),

    CONSTRAINT session_lifecycle_metric_bound_nonnegative CHECK (
        ((interval_bound IS NULL) OR (interval_bound > '0'::interval))
        AND ((count_bound IS NULL) OR (count_bound >= 0))
    )
);

INSERT INTO session_lifecycle_metric_bound (bound_kind)
SELECT kind
  FROM unnest(ARRAY[
        'deadline_processing_grace',
        'wall_cohort_maturation',
        'gate_weeks',
        'completion_failure_rate_threshold_ppm',
        'wall_rate_threshold_ppm',
        'failed_unknown_share_threshold_ppm'
       ]) AS kind;

CREATE FUNCTION session_lifecycle_metric_interval(kind text) RETURNS interval
    LANGUAGE sql
    STABLE
    AS $$
    SELECT interval_bound
      FROM session_lifecycle_metric_bound
     WHERE bound_kind = kind;
$$;

CREATE FUNCTION session_lifecycle_metric_count(kind text) RETURNS bigint
    LANGUAGE sql
    STABLE
    AS $$
    SELECT count_bound
      FROM session_lifecycle_metric_bound
     WHERE bound_kind = kind;
$$;

--
-- Calendar weeks are UTC weeks.
--
-- `date_trunc` on a `timestamptz` answers in the session's `TimeZone`, so the
-- same row would fall in different weeks for two readers. Truncating the UTC
-- instant makes a cohort a property of the data rather than of who asked.
--

CREATE FUNCTION session_lifecycle_metric_week(moment timestamp with time zone)
    RETURNS timestamp without time zone
    LANGUAGE sql
    IMMUTABLE
    AS $$
    SELECT date_trunc('week', moment AT TIME ZONE 'UTC');
$$;

--
-- §12's terminal cohort: sessions that reached `terminal` in a calendar week
-- and were owned at any point in their life.
--
-- Membership follows the journaled ownership record, not the current bit, so
-- releasing a troubled session never removes it from the gate. `owned_after`
-- is true exactly on `created_owned` and `adopted`, which is what "owned at
-- any point" means in that journal.
--
-- The trim and the numerator are recorded per session here rather than
-- restated at every reader: `stopped` leaves the denominator, a supersession
-- leaves it only when it closed no failure, and a supersession that closed a
-- park holding a failure cause stays in both under that standing cause.
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
       );

--
-- The two cause facts a session records, read from the turns that recorded
-- them.
--
-- `overflow_incidence` is defined on the turn cause (§12 says "on any turn").
-- A wall is a session-level fact with three durable spellings — the turn cause
-- §4 landed, the session's own structural terminal cause, and the standing
-- cause a park carried — and any of them is the session recording a wall.
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
-- §12's dispatch cohort, for `wall_rate`.
--
-- A session enters `dispatched` when its first turn is queued (§1), and
-- `turn_lifecycle.recorded_at` is that turn row's immutable write time, so the
-- earliest one is the durable instant the session was dispatched. The
-- satellite's `state_entered_at` is not that instant: it moves with every
-- later transition. A session that never held a turn was never dispatched and
-- joins no dispatch cohort.
--
-- F9's maturation: a weekly cohort is gate-evaluable once no member is both
-- non-terminal and still inside the configured maturation window. With the
-- bound configured `none` a non-terminal member is never past its window, so
-- only an all-terminal cohort matures — which is the conservative reading.
--

CREATE VIEW session_lifecycle_dispatch_cohort AS
SELECT dispatched.session_id,
       session_lifecycle_metric_week(dispatched.dispatched_at) AS dispatch_week,
       dispatched.dispatched_at,
       (lifecycle.state_kind = 'terminal'::text) AS terminal,
       ((lifecycle.state_kind = 'terminal'::text)
        OR ((session_lifecycle_metric_interval('wall_cohort_maturation') IS NOT NULL)
            AND ((dispatched.dispatched_at
                  + session_lifecycle_metric_interval('wall_cohort_maturation'))
                 <= clock_timestamp()))) AS matured,
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
-- §12's `cause_completeness`, both axes.
--
-- Turn axis: every terminal turn carries a cause by §4's mandate, so the
-- measured quantity is the share carrying one outside the catch-all set.
-- `unclassified_failure` is that set's sole member — widening it is what would
-- silently weaken the criterion, which is why the vocabulary keeps exactly one
-- such spelling.
--
-- Model-call axis: the denominator is the calls whose disposition admits a
-- cause — `known_failed`, the only disposition the schema allows a provider
-- cause on — never all terminal calls. `unrecognized` is that axis's
-- catch-all, and a null cause is the absent case §12 names. A known failure
-- classified by attachment preparation rather than by the provider carries a
-- typed cause too, and the schema forbids it from carrying both.
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
-- The weekly report.
--
-- One row per calendar week any of the four cohorts has a member in, carrying
-- each metric as the exact pair of counts it is defined as. Rates are left to
-- the reader so a week with an empty denominator reports an absent rate rather
-- than a fabricated zero.
--

CREATE VIEW session_lifecycle_weekly_metric AS
WITH weeks AS (
    SELECT cohort_week AS week FROM session_lifecycle_terminal_cohort
     UNION
    SELECT dispatch_week AS week FROM session_lifecycle_dispatch_cohort
     UNION
    SELECT recorded_week AS week FROM session_lifecycle_terminal_turn_cause
     UNION
    SELECT recorded_week AS week FROM session_lifecycle_known_failed_call_cause
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
           count(*) FILTER (WHERE cohort.wall) AS wall,
           bool_and(cohort.matured) AS matured
      FROM session_lifecycle_dispatch_cohort AS cohort
     GROUP BY cohort.dispatch_week
), walls_recorded AS (
    -- F9's immediate half: walls attributed to the week they were recorded in,
    -- so a breach pages without waiting for a cohort to mature. §3 stamps a
    -- turn row's write time and no terminalization instant, so the recorded
    -- week is the turn's own write week — the closest durable answer to when
    -- the wall happened, and the same stamp the cause-completeness axes use.
    SELECT session_lifecycle_metric_week(turn.recorded_at) AS week,
           count(*) AS occurrences
      FROM turn_lifecycle AS turn
     WHERE turn.terminal_cause_kind = 'context_compaction_wall'::text
     GROUP BY session_lifecycle_metric_week(turn.recorded_at)
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
       COALESCE(dispatched.matured, true) AS wall_cohort_matured,
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
-- §12's first companion alarm, target zero.
--
-- An owned non-terminal session either holds an armed deadline whose expiry
-- has not passed, or it is a violation. A deadline explicitly configured
-- unbounded — `expires_at` null, the journaled `none` marker — is never
-- counted; a missing record always is.
--
-- F8's grace: an expiry counts only once the configured processing grace has
-- also passed, so ordinary timer and commit latency never trips a zero-target
-- alarm. A grace configured `none` is unbounded exactly as every other `none`
-- bound is: no expiry is ever late, and the alarm reduces to its missing-record
-- half, which is the §1 invariant violation proper.
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
            AND (session_lifecycle_metric_interval('deadline_processing_grace') IS NOT NULL)
            AND ((deadline.expires_at
                  + session_lifecycle_metric_interval('deadline_processing_grace'))
                 < clock_timestamp())));
