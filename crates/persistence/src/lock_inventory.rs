//! Reviewed SQL statements that acquire explicit persistence row locks.
//!
//! Session/scheduler pair order: every transaction that locks both a
//! `session` row and a `session_scheduler` row acquires the `session` row
//! first. Submit-input, applied goal commands (fresh dispatch commissioning
//! included), the delegated endpoint prefixes, and approval-judge completion
//! all take that order; goal system transitions — model declarations and
//! scheduler-failure blocking — hold only the session row, the remaining
//! scheduler-family transactions hold only the scheduler row, and so no two
//! transactions wait on this pair in opposite orders. A scheduler-first
//! acquisition of the pair would deadlock against every path above and must
//! not be introduced.
//!
//! The `session_lifecycle` satellite sits inside that prefix, between the
//! `session` row and the `session_scheduler` row, and is never acquired after
//! the scheduler row. Every statement below that locks a scheduler row locks
//! the satellite first, in a common table expression the scheduler predicate
//! reads, so the order is the statement's own structure rather than a
//! convention a caller could reorder. Turn-lifecycle writers inherit it: the
//! standing rule that every turn-lifecycle writer acquires the scheduler lock
//! before touching a turn row means every transaction whose turn write
//! projects a new session state already holds the satellite when the
//! projection runs.
//!
//! Paths that acquire the satellite outside a scheduler statement hold no
//! scheduler row in the same transaction. Session creation inserts it. The
//! lifecycle store's own park, closure, and ownership writes take the `session`
//! row first. Repository-watch terminal goal commands lock the complete
//! unreleased dispatch cohort in session-identity order before taking their
//! triggering session's scheduler/lifecycle lock; other terminal goal
//! projections take the same ordered cohort in the projection trigger.
//! Blocker replacement and park release serialize on the stable obligation
//! identity, then take the projected subjects in session-identity order before
//! changing the obligation row.

use signalbox_domain::SessionId;

pub(crate) const fn ordered_session_pair(
    first: SessionId,
    second: SessionId,
) -> (SessionId, SessionId) {
    if first.as_uuid().as_u128() <= second.as_uuid().as_u128() {
        (first, second)
    } else {
        (second, first)
    }
}

pub(crate) const PROGRAM_JOURNAL_SEQUENCE: &str = "SELECT
        last_position, last_request_ordinal, last_delivery_ordinal
   FROM program_run_journal_sequence_state
  WHERE run_id = $1
  FOR UPDATE";

pub(crate) const REPO_WATCH_DISPATCH_OBLIGATION: &str =
    "SELECT latest_event_id, settled_kind, settled_dispatch_id,
            parked_at IS NOT NULL AS parked
       FROM repo_watch_dispatch_obligation
      WHERE obligation_id = $1
      FOR UPDATE";

pub(crate) const REPO_WATCH_DISPATCH_OBLIGATION_IDENTITY: &str = "SELECT pg_advisory_xact_lock(
                hashtextextended(repo_watch_dispatch_obligation_lock_key($1), 0)
            )";

pub(crate) const REPO_WATCH_TERMINAL_GOAL_COHORT: &str = "SELECT lifecycle.session_id
       FROM session_lifecycle AS lifecycle
       JOIN (
            SELECT cohort.session_id
              FROM repo_watch_dispatch_action AS subject
              JOIN repo_watch_dispatch_action AS cohort
                ON cohort.dispatch_id = subject.dispatch_id
             WHERE subject.session_id = $1
               AND NOT EXISTS (
                    SELECT 1
                      FROM repo_watch_dispatch_release AS released
                     WHERE released.dispatch_id = subject.dispatch_id
               )
       ) AS dispatch_subject USING (session_id)
      ORDER BY lifecycle.session_id
        FOR UPDATE OF lifecycle";

pub(crate) const REPO_WATCH_OBLIGATION_BLOCKER_SUBJECTS: &str = "SELECT lifecycle.session_id
       FROM session_lifecycle AS lifecycle
       JOIN (
            SELECT current.external_blocking_session_id AS session_id
              FROM repo_watch_dispatch_obligation AS current
             WHERE current.obligation_id = $1
               AND current.external_blocking_session_id IS NOT NULL
            UNION
            SELECT action.session_id
              FROM repo_watch_dispatch_obligation AS current
              JOIN repo_watch_dispatch_action AS action
                ON action.dispatch_id = current.blocking_dispatch_id
             WHERE current.obligation_id = $1
            UNION
            SELECT $2::uuid WHERE $2::uuid IS NOT NULL
            UNION
            SELECT action.session_id
              FROM repo_watch_dispatch_action AS action
             WHERE action.dispatch_id = $3
       ) AS subject USING (session_id)
      ORDER BY lifecycle.session_id
        FOR UPDATE OF lifecycle";

pub(crate) const REPO_WATCH_TERMINAL_TARGET_OBLIGATIONS: &str = "SELECT obligation.obligation_id
           FROM repo_watch_dispatch_obligation AS obligation
          WHERE obligation.settled_kind IS NULL
            AND obligation.parked_state_event_id IS DISTINCT FROM $3
            AND (
                EXISTS (
                    SELECT 1
                      FROM repo_watch_event AS event
                     WHERE event.event_id = obligation.latest_event_id
                       AND event.repository = $1
                       AND event.pull_request_number = $2
                       AND event.event_id <> $3
                )
                OR EXISTS (
                    SELECT 1
                      FROM repo_watch_event AS parked_state
                     WHERE parked_state.event_id = obligation.parked_state_event_id
                       AND parked_state.repository = $1
                       AND parked_state.pull_request_number = $2
                )
            )
          ORDER BY obligation.obligation_id
            FOR UPDATE";

pub(crate) const REPO_WATCH_WEBHOOK_DELIVERY: &str = "SELECT receipt_sequence
       FROM repo_watch_webhook_delivery
      WHERE hook_id = $1 AND delivery_id = $2
      FOR UPDATE";

pub(crate) const START_ELIGIBLE_TURN: &str = "WITH satellite AS (
                SELECT session_id
                  FROM session_lifecycle
                 WHERE session_id = $1
                 FOR NO KEY UPDATE
            )
            SELECT
            EXISTS (
                SELECT 1
                  FROM session
                 WHERE session_id = $1
            ),
            (
                SELECT session_id
                  FROM session_scheduler
                 WHERE session_id = (SELECT session_id FROM satellite)
                 FOR UPDATE
            )";

pub(crate) const EXPIRED_DISPATCH_START_LEASE: &str = "SELECT EXISTS (
        SELECT 1
          FROM repo_watch_dispatch_start_lease AS lease
         WHERE lease.session_id = $1
           AND lease.expires_at <= clock_timestamp()
           AND NOT EXISTS (
                SELECT 1
                  FROM model_call AS call
                 WHERE call.session_id = lease.session_id
           )
           AND (
                (
                    NOT EXISTS (
                        SELECT 1
                          FROM repo_watch_dispatch_start_lease_expiration AS expired
                         WHERE expired.dispatch_id = lease.dispatch_id
                           AND expired.action_ordinal = lease.action_ordinal
                    )
                    AND NOT EXISTS (
                        SELECT 1
                          FROM repo_watch_dispatch_release AS released
                         WHERE released.dispatch_id = lease.dispatch_id
                    )
                )
                OR EXISTS (
                    SELECT 1
                      FROM turn_lifecycle AS lifecycle
                      JOIN goal_turn AS goal
                        ON goal.session_id = lifecycle.session_id
                       AND goal.turn_id = lifecycle.turn_id
                      JOIN repo_watch_dispatch_start_lease_expiration AS expired
                        ON expired.dispatch_id = lease.dispatch_id
                       AND expired.action_ordinal = lease.action_ordinal
                       AND expired.goal_command_id IS NOT NULL
                     WHERE lifecycle.session_id = lease.session_id
                       AND lifecycle.state_kind = 'active'
                       AND goal.goal_generation = 1
                       AND NOT EXISTS (
                            SELECT 1
                              FROM goal_turn AS successor_goal
                             WHERE successor_goal.session_id = lease.session_id
                               AND successor_goal.goal_generation >
                                   goal.goal_generation
                       )
                )
           )
    )";

pub(crate) const STARTUP_RECOVERY: &str = "WITH satellite AS (
                SELECT session_id
                  FROM session_lifecycle
                 WHERE session_id = $1
                 FOR NO KEY UPDATE
            )
            SELECT
            EXISTS (
                SELECT 1
                  FROM session
                 WHERE session_id = $1
            ),
            (
                SELECT session_id
                  FROM session_scheduler
                 WHERE session_id = (SELECT session_id FROM satellite)
                 FOR UPDATE
            ),
            (
                SELECT turn_id
                  FROM turn_lifecycle
                 WHERE session_id = $1
                   AND state_kind = 'active'
                   AND NOT delegation_runtime_terminal
            )";

/// Enrolls newly parked recovery-waiting turns, one bounded page per lap.
///
/// The page locks each turn row `FOR NO KEY UPDATE`, at the same strength an
/// accepting operator interrupt's terminalizing `UPDATE` takes. Without that
/// lock nothing made the two contend: under `READ COMMITTED` discovery's
/// snapshot could still read a turn as recovery-waiting while an interrupt's
/// terminalization sat uncommitted, and the interrupt's own supersession could
/// not see discovery's uncommitted insert, so both committed and left a
/// terminal turn beside a live `scheduled` recovery row — the shape
/// `process_read` rejects as corruption. Contending settles it either way:
/// the interrupt commits first and this statement's re-check drops the row, or
/// discovery commits first and the interrupt's supersession sees the row it
/// inserted.
pub(crate) const AUTOMATIC_RECONCILIATION_DISCOVERY: &str = "WITH discovery AS (
            SELECT after_turn_id, high_turn_id
              FROM automatic_reconciliation_discovery_state
             WHERE singleton
             FOR UPDATE
         ), bounds AS MATERIALIZED (
            SELECT after_turn_id,
                   CASE
                       WHEN after_turn_id IS NULL THEN (
                           SELECT turn_id
                             FROM turn_lifecycle
                            WHERE state_kind = 'active'
                              AND active_phase_kind IN (
                                  'awaiting_model_call_recovery',
                                  'awaiting_tool_recovery'
                              )
                              AND NOT delegation_runtime_terminal
                              AND num_nonnulls(
                                  recovery_model_call_id,
                                  recovery_tool_attempt_id
                              ) = 1
                            ORDER BY turn_id DESC
                            LIMIT 1
                       )
                       ELSE high_turn_id
                   END AS high_turn_id
              FROM discovery
         ), page AS (
            SELECT turn_id, session_id, recovery_model_call_id,
                   recovery_tool_attempt_id
              FROM turn_lifecycle, bounds
             WHERE state_kind = 'active'
               AND active_phase_kind IN (
                   'awaiting_model_call_recovery',
                   'awaiting_tool_recovery'
               )
               AND NOT delegation_runtime_terminal
               AND num_nonnulls(
                   recovery_model_call_id,
                   recovery_tool_attempt_id
               ) = 1
               AND (bounds.after_turn_id IS NULL OR turn_id > bounds.after_turn_id)
               AND turn_id <= bounds.high_turn_id
             ORDER BY turn_id
             LIMIT $1
             FOR NO KEY UPDATE OF turn_lifecycle
         ), inserted AS (
            INSERT INTO automatic_reconciliation
                (turn_id, session_id, model_call_id, tool_attempt_id)
            SELECT turn_id, session_id, recovery_model_call_id,
                   recovery_tool_attempt_id FROM page
            ON CONFLICT (turn_id) DO NOTHING
            RETURNING turn_id
         )
         UPDATE automatic_reconciliation_discovery_state
            SET after_turn_id = CASE
                    WHEN (SELECT count(*) FROM page) = $1 THEN (
                        SELECT turn_id FROM page ORDER BY turn_id DESC LIMIT 1
                    )
                    ELSE NULL
                END,
                high_turn_id = CASE
                    WHEN (SELECT count(*) FROM page) = $1 THEN (
                        SELECT high_turn_id FROM bounds
                    )
                    ELSE NULL
                END
          WHERE singleton";

/// Retires the recoveries whose durable wait no longer exists, one bounded lap
/// at a time.
///
/// Supersession is the only scan here that must reinspect rows it already
/// passed: a recovery becomes superseded by a change to `turn_lifecycle`, not
/// by anything this statement wrote, so a row left behind the cursor can
/// acquire that disposition afterwards — an operator resolving an exhausted
/// wait, or a delegation cascade making the turn runtime-terminal. Advancing
/// the cursor alone does not guarantee it is ever reread: while at least one
/// window of higher-id recoveries keeps arriving between scans, the page never
/// empties, so the cursor never wraps to `NULL` and the older rows starve.
///
/// So each lap is bounded the way the discovery cursor's is. The first page of
/// a lap fixes a high-water mark over the same predicate, and the lap walks
/// only up to it; rows inserted after it belong to the next lap and cannot
/// defer the wrap. A row *below* the mark still enters its page on the state it
/// holds when that page is read, so a disposition acquired mid-lap is not
/// missed.
pub(crate) const AUTOMATIC_RECONCILIATION_SUPERSESSION: &str = "WITH cursor AS (
            SELECT after_turn_id, high_turn_id
              FROM automatic_reconciliation_supersession_state
             WHERE singleton
             FOR UPDATE
         ), bounds AS MATERIALIZED (
            SELECT after_turn_id,
                   CASE
                       WHEN after_turn_id IS NULL THEN (
                           SELECT recovery.turn_id
                             FROM automatic_reconciliation AS recovery
                            WHERE recovery.state_kind
                                  IN ('scheduled', 'attempting', 'exhausted')
                            ORDER BY recovery.turn_id DESC
                            LIMIT 1
                       )
                       ELSE high_turn_id
                   END AS high_turn_id
              FROM cursor
         ), page AS (
            SELECT recovery.turn_id, recovery.session_id, recovery.model_call_id,
                   recovery.tool_attempt_id,
                   recovery.state_kind, recovery.attempt_count
              FROM automatic_reconciliation AS recovery, bounds
             WHERE recovery.state_kind IN ('scheduled', 'attempting', 'exhausted')
               AND (bounds.after_turn_id IS NULL OR recovery.turn_id > bounds.after_turn_id)
               AND recovery.turn_id <= bounds.high_turn_id
             ORDER BY recovery.turn_id
             LIMIT $1
         ), superseded AS (
            SELECT page.*
              FROM page
             WHERE NOT EXISTS (
                SELECT 1 FROM turn_lifecycle AS lifecycle
                 WHERE lifecycle.turn_id = page.turn_id
                   AND lifecycle.session_id = page.session_id
                   AND lifecycle.state_kind = 'active'
                   AND NOT lifecycle.delegation_runtime_terminal
                   AND (
                        lifecycle.active_phase_kind = 'awaiting_model_call_recovery'
                        AND lifecycle.recovery_model_call_id = page.model_call_id
                        AND page.tool_attempt_id IS NULL
                     OR lifecycle.active_phase_kind = 'awaiting_tool_recovery'
                        AND lifecycle.recovery_tool_attempt_id = page.tool_attempt_id
                        AND page.model_call_id IS NULL
                   )
            )
         ), attempts AS (
            UPDATE automatic_reconciliation_attempt AS attempt
               SET outcome_kind = 'superseded',
                   finished_at = statement_timestamp()
              FROM superseded
             WHERE superseded.state_kind = 'attempting'
               AND attempt.turn_id = superseded.turn_id
               AND attempt.attempt_ordinal = superseded.attempt_count
               AND attempt.outcome_kind = 'attempting'
         ), recoveries AS (
            UPDATE automatic_reconciliation AS recovery
               SET state_kind = 'superseded', exhausted_at = NULL
              FROM superseded
             WHERE recovery.turn_id = superseded.turn_id
         )
         UPDATE automatic_reconciliation_supersession_state
            SET after_turn_id = CASE
                    WHEN (SELECT count(*) FROM page) = $1 THEN (
                        SELECT turn_id FROM page ORDER BY turn_id DESC LIMIT 1
                    )
                    ELSE NULL
                END,
                high_turn_id = CASE
                    WHEN (SELECT count(*) FROM page) = $1 THEN (
                        SELECT high_turn_id FROM bounds
                    )
                    ELSE NULL
                END
          WHERE singleton";

/// Claims one due window of automatic reconciliations.
///
/// The attempt budget (`$2`) and the retry ladder (`$3`..`$7`, in milliseconds)
/// are bound by the caller from `AutomaticReconciliationAttempt` rather than
/// written here. They were literals, which meant the schedule this daemon
/// actually enforces lived only in this string: the Rust ladder had no
/// production reader, so the two could diverge in either direction with nothing
/// failing.
///
/// Milliseconds rather than seconds because the failure path schedules its own
/// retry in milliseconds. Seconds here truncated every sub-second configured
/// backoff to zero, which is not a short abandonment deadline but an immediate
/// one, so the two paths disagreed for exactly the policies a second cannot
/// express.
///
/// The CASE has one arm per admitted attempt, so its arity is part of the
/// contract: a configured budget above it would reach the `ELSE` arm and reuse
/// the last deadline while the failure path kept computing the true schedule.
/// The daemon refuses such a budget at configuration admission.
pub(crate) const AUTOMATIC_RECONCILIATION_CLAIM: &str = "WITH due AS (
                SELECT turn_id
                  FROM automatic_reconciliation
                 WHERE state_kind = 'scheduled'
                   AND attempt_count < $2
                   AND next_attempt_at <= statement_timestamp()
                 ORDER BY next_attempt_at, turn_id
                 LIMIT $1
                 FOR UPDATE SKIP LOCKED
             ), claimed AS (
                UPDATE automatic_reconciliation AS recovery
                   SET attempt_count = recovery.attempt_count + 1,
                       state_kind = 'attempting',
                       next_attempt_at = CASE
                           WHEN $3::bigint IS NULL THEN 'infinity'::timestamptz
                           ELSE statement_timestamp()
                               + (CASE recovery.attempt_count + 1
                                    WHEN 1 THEN $3::bigint
                                    WHEN 2 THEN $4::bigint
                                    WHEN 3 THEN $5::bigint
                                    WHEN 4 THEN $6::bigint
                                    ELSE $7::bigint
                                  END * interval '1 millisecond')
                       END
                  FROM due
                 WHERE recovery.turn_id = due.turn_id
             RETURNING recovery.session_id, recovery.turn_id,
                       recovery.model_call_id, recovery.tool_attempt_id,
                       recovery.attempt_count
             ), recorded AS (
                INSERT INTO automatic_reconciliation_attempt
                    (turn_id, attempt_ordinal)
                SELECT turn_id, attempt_count FROM claimed
                RETURNING turn_id
             )
             SELECT claimed.session_id, claimed.turn_id,
                    claimed.model_call_id, claimed.tool_attempt_id,
                    claimed.attempt_count
               FROM claimed
               JOIN recorded USING (turn_id)
              ORDER BY claimed.turn_id";

pub(crate) const CONTEXT_COMPACTION_SCHEDULER: &str = "WITH satellite AS (
                SELECT session_id
                  FROM session_lifecycle
                 WHERE session_id = $1
                 FOR NO KEY UPDATE
            )
            SELECT
            EXISTS (
                SELECT 1
                  FROM session
                 WHERE session_id = $1
            ),
            (
                SELECT session_id
                  FROM session_scheduler
                 WHERE session_id = (SELECT session_id FROM satellite)
                 FOR UPDATE
            )";

pub(crate) const CONTEXT_COMPACTION_DEFAULTS: &str = "SELECT current_version
           FROM session_current_defaults
          WHERE session_id = $1
          FOR UPDATE";

pub(crate) const REPLACE_SESSION_DEFAULTS_CURRENT: &str = "SELECT current_version
           FROM session_current_defaults
          WHERE session_id = $1
          FOR UPDATE";

pub(crate) const CONTEXT_COMPACTION_LIFECYCLE_SESSION: &str =
    "SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE";

/// Serializes one session's own lifecycle writes — park, closure, ownership.
///
/// Taken after the session row and before any scheduler row, which is the
/// satellite's declared place in the order. These transactions hold no
/// scheduler row at all, so the pair they take is session then satellite.
pub(crate) const SESSION_LIFECYCLE_SESSION: &str =
    "SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE";

pub(crate) const SESSION_LIFECYCLE_SATELLITE: &str =
    "SELECT session_id FROM session_lifecycle WHERE session_id = $1 FOR NO KEY UPDATE";

pub(crate) const SUBMIT_INPUT_SESSION: &str =
    "SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE";

pub(crate) const SUBMIT_INPUT_SCHEDULER: &str = "WITH satellite AS (
            SELECT session_id
              FROM session_lifecycle
             WHERE session_id = $1
             FOR NO KEY UPDATE
        )
        SELECT session_id
           FROM session_scheduler
          WHERE session_id = (SELECT session_id FROM satellite)
          FOR UPDATE";

pub(crate) const SUBMIT_INPUT_DEFAULTS: &str = "SELECT current_version
           FROM session_current_defaults
          WHERE session_id = $1
          FOR UPDATE";

pub(crate) const SUBMIT_INPUT_RUNNER_RECOVERY_ATTEMPT: &str =
    "SELECT tool_attempt.state_kind, tool_attempt.terminal_disposition_kind,
            lease.effect_class AS lease_effect_class,
            lease_event.state_kind AS lease_state_kind
       FROM turn_lifecycle AS lifecycle
       JOIN runner_current_session_placement AS placement_head
         ON placement_head.session_id = lifecycle.session_id
       JOIN runner_session_placement_record AS placement
         ON placement.session_id = placement_head.session_id
        AND placement.event_ordinal = placement_head.event_ordinal
       JOIN tool_attempt
         ON tool_attempt.attempt_id = lifecycle.runner_recovery_tool_attempt_id
        AND tool_attempt.turn_id = lifecycle.turn_id
        AND tool_attempt.session_id = lifecycle.session_id
       JOIN runner_physical_attempt_lease_binding AS binding
         ON binding.attempt_id = tool_attempt.attempt_id
       JOIN runner_lease_generation AS lease
         ON lease.lease_id = binding.lease_id
        AND lease.attempt_id = tool_attempt.attempt_id
        AND lease.session_id = tool_attempt.session_id
       JOIN runner_session_placement_record AS leased_placement
         ON leased_placement.session_id = lease.session_id
        AND leased_placement.event_ordinal = lease.placement_event_ordinal
       JOIN runner_current_lease_event AS lease_head
         ON lease_head.lease_id = lease.lease_id
        AND lease_head.generation = lease.generation
       JOIN runner_lease_event AS lease_event
         ON lease_event.lease_id = lease_head.lease_id
        AND lease_event.generation = lease_head.generation
        AND lease_event.event_ordinal = lease_head.event_ordinal
      WHERE lifecycle.session_id = $1
        AND lifecycle.turn_id = $2
        AND lifecycle.state_kind = 'active'
        AND lifecycle.active_phase_kind = 'awaiting_runner_recovery'
        AND lifecycle.runner_recovery_tool_attempt_id = $3
        AND placement.state_kind = 'runner_lost'
        AND placement.interrupted_tool_attempt_id = tool_attempt.attempt_id
        AND placement.lost_runner_id = lease.runner_id
        AND runner_lease_placement_reaches_loss_revision(
            lease.session_id,
            lease.placement_event_ordinal,
            placement.placement_revision,
            placement.lost_runner_id
        )
        AND leased_placement.state_kind = 'pinned'
        AND leased_placement.pinned_runner_id = placement.lost_runner_id
      FOR UPDATE OF tool_attempt";

pub(crate) const DELEGATION_TERMINATION_SESSION_FRONTIER: &str =
    "SELECT lock_delegation_termination_session_frontier($1, $2)";

pub(crate) const DELEGATION_TERMINAL_RELATION: &str =
    "SELECT task.spawning_tool_request_id, relation.parent_session_id
       FROM session_delegation_initial_task AS task
       JOIN session_delegation AS relation
         ON relation.spawning_tool_request_id = task.spawning_tool_request_id
        AND relation.child_session_id = task.child_session_id
      WHERE task.child_session_id = $1
        AND task.turn_id = $2
      FOR UPDATE OF relation";

pub(crate) const DELEGATION_TERMINAL_RELATION_IDENTITY: &str =
    "SELECT task.spawning_tool_request_id, relation.parent_session_id
       FROM session_delegation_initial_task AS task
       JOIN session_delegation AS relation
         ON relation.spawning_tool_request_id = task.spawning_tool_request_id
        AND relation.child_session_id = task.child_session_id
      WHERE task.child_session_id = $1
        AND task.turn_id = $2";

pub(crate) const DELEGATION_TERMINAL_ENDPOINT_SESSION: &str =
    "SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE";

pub(crate) const DELEGATION_FIND_RELATION_FOR_WAIT: &str = "SELECT spawning_tool_request_id
       FROM session_delegation
      WHERE parent_session_id = $1 AND child_session_id = $2
      FOR UPDATE";

pub(crate) const DELEGATION_FIND_RELATION_FOR_MESSAGE: &str = "SELECT spawning_tool_request_id
       FROM session_delegation
      WHERE (parent_session_id = $1 AND child_session_id = $2)
         OR (parent_session_id = $2 AND child_session_id = $1)
      FOR UPDATE";

pub(crate) const DELEGATION_DELIVERY_SESSION: &str =
    "SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE";

pub(crate) const DELEGATION_LOAD_RELATION: &str =
    "SELECT relation.parent_session_id, relation.parent_turn_id,
            relation.child_session_id, relation.policy_kind,
            relation.on_parent_stopped, relation.on_parent_cancelled,
            task.turn_id AS child_turn_id, task.task_content
       FROM session_delegation AS relation
       JOIN session_delegation_initial_task AS task
         ON task.spawning_tool_request_id = relation.spawning_tool_request_id
        AND task.child_session_id = relation.child_session_id
      WHERE relation.spawning_tool_request_id = $1
      FOR UPDATE OF relation";

pub(crate) const REPLACE_SESSION_METADATA: &str =
    "SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE";

pub(crate) const UPDATE_SESSION_PLACEMENT_HEAD: &str = "SELECT session_row.ancestry_kind,
            event.version, event.prior_version, event.event_kind,
            event.placement_path, event.root_global_read_intent,
            native_registry.command_id AS native_creation_command_id,
            imported_registry.command_id AS imported_creation_command_id,
            placement_update_registry.command_id AS placement_update_command_id
       FROM session_current_placement AS head
       JOIN session AS session_row
         ON session_row.session_id = head.session_id
       JOIN session_placement_event AS event
         ON event.session_id = head.session_id
        AND event.version = head.current_version
       LEFT JOIN create_session_command AS native_creation
         ON native_creation.command_id = event.provenance_command_id
        AND native_creation.created_session_id = event.session_id
        AND native_creation.command_kind = 'create_session'
        AND native_creation.storage_version IN (1, 2, 3, 4, 6, 7)
        AND (native_creation.storage_version IN (6, 7)
             OR (native_creation.storage_version IN (1, 2, 3, 4)
                 AND event.placement_path IS NULL
                 AND NOT event.root_global_read_intent))
        AND native_creation.result_kind = 'applied'
        AND native_creation.placement_path IS NOT DISTINCT FROM event.placement_path
        AND native_creation.root_global_read_intent = event.root_global_read_intent
       LEFT JOIN durable_command AS native_registry
         ON native_registry.command_id = native_creation.command_id
        AND native_registry.command_kind = native_creation.command_kind
        AND native_registry.storage_version = native_creation.storage_version
       LEFT JOIN create_session_from_imported_frontier_command AS imported_creation
         ON imported_creation.command_id = event.provenance_command_id
        AND imported_creation.created_session_id = event.session_id
        AND imported_creation.command_kind = 'create_session_from_imported_frontier'
        AND imported_creation.storage_version IN (1, 2, 3, 5)
        AND imported_creation.result_kind = 'applied'
        AND event.placement_path IS NULL
        AND NOT event.root_global_read_intent
       LEFT JOIN durable_command AS imported_registry
         ON imported_registry.command_id = imported_creation.command_id
        AND imported_registry.command_kind = imported_creation.command_kind
        AND imported_registry.storage_version = imported_creation.storage_version
       LEFT JOIN update_session_placement_command AS placement_update
         ON placement_update.command_id = event.provenance_command_id
        AND placement_update.session_id = event.session_id
        AND placement_update.command_kind = 'update_session_placement'
        AND placement_update.storage_version = 1
        AND placement_update.result_kind = 'applied'
        AND placement_update.rejection_kind IS NULL
        AND placement_update.result_version = event.version
        AND placement_update.result_current_version IS NULL
        AND placement_update.expected_version = event.prior_version
        AND placement_update.replacement_path IS NOT DISTINCT FROM event.placement_path
        AND placement_update.root_global_read_intent = event.root_global_read_intent
       LEFT JOIN durable_command AS placement_update_registry
         ON placement_update_registry.command_id = placement_update.command_id
        AND placement_update_registry.command_kind = placement_update.command_kind
        AND placement_update_registry.storage_version = placement_update.storage_version
      WHERE head.session_id = $1
      FOR UPDATE OF head";

pub(crate) const PLAN_APPEND_ATTEMPT: &str = "SELECT attempt.attempt_id
  FROM tool_attempt AS attempt
  JOIN tool_request AS request
    ON request.request_id = attempt.request_id
 WHERE attempt.attempt_id = $1
   AND attempt.request_id = $2
   AND attempt.issuing_turn_attempt_id = $3
   AND attempt.dispatch_generation = $4
   AND attempt.turn_id = $5
   AND attempt.session_id = $6
   AND attempt.effect_class = 'external_effect'
   AND attempt.state_kind = 'in_flight'
   AND request.request_id = $2
   AND request.session_id = $6
   AND request.turn_id = $5
   AND request.tool_name = 'plan_write'
   AND request.arguments_kind = 'json'
   AND session_plan_request_arguments_json(
           request.arguments_kind, request.arguments_text
       ) =
        CASE $7::text
            WHEN 'created' THEN jsonb_build_object(
                'kind', 'create',
                'text', $9::text
            )
            WHEN 'text_revised' THEN jsonb_build_object(
                'kind', 'revise',
                'entry_id', $8::numeric,
                'text', $9::text
            )
            WHEN 'status_changed' THEN jsonb_build_object(
                'kind', 'set_status',
                'entry_id', $8::numeric,
                'status', $10::text
            )
            WHEN 'depends_on' THEN jsonb_build_object(
                'kind', 'depends_on',
                'entry_id', $8::numeric,
                'dependency_id', $11::numeric
            )
        END
 FOR SHARE OF attempt";

pub(crate) const OUTBOX_DELIVERY: &str = "SELECT delivered_through
           FROM outbox_consumer_cursor
          WHERE consumer_name = $1
          FOR UPDATE";

pub(crate) const OUTBOX_SEQUENCE_ALLOCATOR: &str = "SELECT singleton
           FROM outbox_sequence_state
          WHERE singleton
          FOR UPDATE";

pub(crate) const HUB_FENCE_GENERATION: &str = "SELECT generation
           FROM hub_fence_state
          WHERE singleton
          FOR UPDATE";

pub(crate) const RUNNER_ENROLLMENT: &str = "SELECT enrollment_id
               FROM runner_enrollment
              WHERE enrollment_id = $1
              FOR UPDATE";

pub(crate) const RUNNER_GRANT: &str = "SELECT credential_profile_name
               FROM runner_credential_grant
              WHERE session_id = $1
                AND lineage_origin_event_ordinal = $2
                AND runner_id = $3
                AND grant_revision = $4
              FOR UPDATE";

pub(crate) const RUNNER_LEASE_ENROLLMENT_AUTHORITY: &str = "SELECT state_kind
               FROM runner_enrollment
              WHERE enrollment_id = $1
              FOR SHARE";

pub(crate) const RUNNER_CONNECTION_LOSS_HEAD: &str = "SELECT loss_epoch
               FROM runner_current_connection_loss
              WHERE enrollment_id = $1
              FOR UPDATE";

pub(crate) const RUNNER_CONNECTION_LOSS_PROPAGATION: &str = "SELECT
                    propagation.propagated_through_session_id,
                    propagation.state_kind,
                    loss.connection_epoch,
                    loss.connection_event_ordinal
               FROM runner_connection_loss_propagation AS propagation
               JOIN runner_connection_loss_epoch AS loss
                 ON loss.enrollment_id = propagation.enrollment_id
                AND loss.loss_epoch = propagation.loss_epoch
              WHERE propagation.enrollment_id = $1
                AND propagation.loss_epoch = $2
              FOR UPDATE OF propagation";

pub(crate) const RUNNER_LEASE_GRANT_AUTHORITY: &str = "SELECT grant_record.credential_profile_name
               FROM runner_current_credential_grant_audit AS current_audit
               JOIN runner_credential_grant AS grant_record
                 ON grant_record.session_id = current_audit.session_id
                AND grant_record.lineage_origin_event_ordinal =
                    current_audit.lineage_origin_event_ordinal
                AND grant_record.runner_id = current_audit.runner_id
                AND grant_record.grant_revision = current_audit.grant_revision
              WHERE current_audit.session_id = $1
                AND current_audit.lineage_origin_event_ordinal = $2
                AND current_audit.runner_id = $3
                AND current_audit.grant_revision = $4
              FOR SHARE OF current_audit";

pub(crate) const RUNNER_REGISTRATION_HEAD: &str = "SELECT registration_revision
               FROM runner_current_registration
              WHERE enrollment_id = $1
              FOR UPDATE";

pub(crate) const RUNNER_PLACEMENT_HEAD: &str = "SELECT record.*
               FROM runner_current_session_placement AS current_placement
               JOIN runner_session_placement_record AS record
                 ON record.session_id = current_placement.session_id
                AND record.event_ordinal = current_placement.event_ordinal
              WHERE current_placement.session_id = $1
              FOR UPDATE OF current_placement";

pub(crate) const RUNNER_PLACEMENT_ENROLLMENT_BY_RUNNER: &str = "SELECT enrollment_id
               FROM runner_enrollment
              WHERE runner_id = $1
              FOR UPDATE";

pub(crate) const RUNNER_PLACEMENT_CONNECTION_AUTHORITY: &str = "SELECT connection_epoch
               FROM runner_connection_authority_head
              WHERE enrollment_id = $1
              FOR SHARE";

pub(crate) const RUNNER_PLACEMENT_CURRENT_LOSS: &str = "SELECT loss_epoch
               FROM runner_current_connection_loss
              WHERE enrollment_id = $1
              FOR SHARE";

pub(crate) const RUNNER_RETRY_REPLACEMENT_SCHEDULER: &str = "WITH satellite AS (
                SELECT session_id
                  FROM session_lifecycle
                 WHERE session_id = $1
                 FOR NO KEY UPDATE
            )
            SELECT session_id
               FROM session_scheduler
              WHERE session_id = (SELECT session_id FROM satellite)
              FOR UPDATE";

pub(crate) const RUNNER_LEASE_HEAD: &str = "SELECT current_event.event_ordinal, event.state_kind,
                    lease_generation.attempt_id,
                    lease_generation.session_id,
                    lease_generation.runner_id,
                    lease_generation.tool_name,
                    lease_generation.effect_class,
                    lease_generation.credential_profile_name,
                    lease_generation.credential_grant_lineage_origin_ordinal,
                    lease_generation.credential_grant_revision,
                    lease_generation.credential_approval_kind,
                    attempt.session_id AS canonical_dispatch_session,
                    attempt.turn_id AS canonical_dispatch_turn,
                    attempt.issuing_turn_attempt_id
                        AS canonical_dispatch_issuing_attempt,
                    attempt.request_id AS canonical_dispatch_request,
                    attempt.dispatch_generation
                        AS canonical_dispatch_generation
               FROM runner_current_lease_event AS current_event
               JOIN runner_lease_event AS event
                 ON event.lease_id = current_event.lease_id
                AND event.generation = current_event.generation
                AND event.event_ordinal = current_event.event_ordinal
               JOIN runner_lease_generation AS lease_generation
                 ON lease_generation.lease_id = current_event.lease_id
                AND lease_generation.generation = current_event.generation
               JOIN tool_attempt AS attempt
                 ON attempt.attempt_id = lease_generation.attempt_id
              WHERE current_event.lease_id = $1
                AND current_event.generation = $2
              FOR UPDATE OF current_event";

pub(crate) const RUNNER_LEASE_PLACEMENT: &str = "SELECT record.*
           FROM runner_current_session_placement AS current_placement
           JOIN runner_session_placement_record AS record
             ON record.session_id = current_placement.session_id
            AND record.event_ordinal = current_placement.event_ordinal
          WHERE current_placement.session_id = $1
          FOR UPDATE OF current_placement";
pub(crate) const REVIEW_RUN_TRANSITION: &str = "SELECT
            workflow_run.run_id, workflow_run.target_id,
            workflow_run.workflow_kind, workflow_run.policy_version,
            workflow_run.minimum_judge_confidence,
            workflow_run.minimum_publication_confidence,
            workflow_run.state_kind, workflow_run.state_pass_id,
            canonical_pass.pass_id AS evidence_pass_id,
            canonical_pass.run_id AS evidence_pass_run_id,
            canonical_pass.target_id AS evidence_pass_target_id,
            canonical_pass.pass_kind AS evidence_pass_kind,
            canonical_pass.state_kind AS evidence_pass_state_kind,
            canonical_pass.turn_id AS evidence_pass_turn_id,
            canonical_pass.output_frontier_id
                AS evidence_pass_output_frontier_id,
            canonical_pass.result_kind
                AS evidence_pass_result_kind,
            canonical_pass.result_finding_id
                AS evidence_pass_result_finding_id,
            canonical_pass.result_finding_run_id
                AS evidence_pass_result_finding_run_id,
            canonical_pass.result_finding_pass_id
                AS evidence_pass_result_finding_pass_id,
            canonical_pass.result_event_ordinal
                AS evidence_pass_result_event_ordinal,
            canonical_pass.result_event_kind
                AS evidence_pass_result_event_kind,
            canonical_pass.result_reason
                AS evidence_pass_result_reason,
            canonical_pass.result_referenced_finding_id
                AS evidence_pass_result_referenced_finding_id,
            canonical_pass.result_referenced_finding_run_id
                AS evidence_pass_result_referenced_finding_run_id,
            canonical_pass.result_referenced_finding_pass_id
                AS evidence_pass_result_referenced_finding_pass_id,
            canonical_pass.result_referenced_finding_status
                AS evidence_pass_result_referenced_finding_status,
            canonical_pass.result_external_link_id
                AS evidence_pass_result_external_link_id,
            canonical_pass.result_external_object_key
                AS evidence_pass_result_external_object_key,
            canonical_pass.result_observation_state
                AS evidence_pass_result_observation_state
       FROM review_run AS workflow_run
       LEFT JOIN review_pass AS canonical_pass
         ON canonical_pass.run_id = workflow_run.run_id
        AND canonical_pass.target_id = workflow_run.target_id
      WHERE workflow_run.run_id = $1
      FOR UPDATE OF workflow_run";

pub(crate) const REVIEW_PASS_TRANSITION: &str = "SELECT
            workflow_pass.pass_id, workflow_pass.run_id,
            workflow_pass.target_id, workflow_pass.pass_kind,
            canonical_run.workflow_kind AS run_workflow_kind,
            workflow_pass.session_id AS pass_session_id,
            workflow_pass.accepted_input_id, workflow_pass.origin_turn_id,
            workflow_pass.state_kind,
            workflow_pass.turn_id, workflow_pass.output_frontier_id,
            workflow_pass.result_kind,
            workflow_pass.result_finding_id,
            workflow_pass.result_finding_run_id,
            workflow_pass.result_finding_pass_id,
            workflow_pass.result_event_ordinal,
            workflow_pass.result_event_kind,
            workflow_pass.result_reason,
            workflow_pass.result_referenced_finding_id,
            workflow_pass.result_referenced_finding_run_id,
            workflow_pass.result_referenced_finding_pass_id,
            workflow_pass.result_referenced_finding_status,
            workflow_pass.result_external_link_id,
            workflow_pass.result_external_object_key,
            workflow_pass.result_observation_state,
            canonical_input.session_id AS accepted_input_session_id,
            canonical_input.origin_turn_id AS accepted_input_origin_turn_id,
            canonical_turn.turn_id AS evidence_turn_id,
            canonical_turn.session_id AS turn_session_id,
            canonical_turn.origin_accepted_input_id
                AS turn_accepted_input_id,
            canonical_turn.state_kind AS turn_state_kind,
            canonical_turn.terminal_disposition_kind
                AS turn_terminal_disposition_kind,
            canonical_turn.terminal_frontier_id
                AS turn_terminal_frontier_id,
            canonical_run.run_id AS canonical_run_id,
            canonical_run.target_id AS canonical_run_target_id
       FROM review_pass AS workflow_pass
       JOIN review_run AS canonical_run
         ON canonical_run.run_id = workflow_pass.run_id
        AND canonical_run.target_id = workflow_pass.target_id
       LEFT JOIN accepted_input AS canonical_input
         ON canonical_input.accepted_input_id =
            workflow_pass.accepted_input_id
       LEFT JOIN turn_lifecycle AS canonical_turn
         ON canonical_turn.turn_id =
            COALESCE(workflow_pass.turn_id, $2)
      WHERE workflow_pass.pass_id = $1
      FOR UPDATE OF workflow_pass";

pub(crate) const REVIEW_FINDINGS_TRANSITION: &str = "SELECT finding_id
       FROM review_finding
      WHERE finding_id = ANY($1)
      ORDER BY finding_id
      FOR NO KEY UPDATE";

pub(crate) const REVIEW_EXTERNAL_LINK_TRANSITION: &str = "SELECT external_link_id
       FROM review_external_link
      WHERE external_link_id = $1
      FOR NO KEY UPDATE";
