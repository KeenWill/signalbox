--
-- Session lifecycle §1/§2/§6: the mutable core-owned lifecycle satellite,
-- its armed-deadline record, and the ownership journal.
--
-- `session` itself stays append-only. Every mutable per-session lifecycle
-- value — state, the typed state detail, `ended_at`, the terminal outcome, the
-- ownership bit, the payload measurements, the settle-then-terminal handoff —
-- lands in `session_lifecycle`, the satellite pattern every other mutable
-- per-session value already follows.
--
-- The unconstrained dogfood-database reset is ratified, so every column lands
-- at its final shape: no nullable-then-backfill scaffolding and no historical
-- exemption spelling.
--

--
-- §6 provenance. The creation cause widens from `user_initiated | delegated`
-- to `interactive | module_dispatched{module, dispatch_ref} | delegated`.
-- `user_initiated` is respelled `interactive` rather than kept as an alias:
-- pre-alpha stored values move to their correct current shape.
--
-- The imported-frontier creation family records `interactive` and keeps its
-- import reference in its own ancestry columns, so the vocabulary stays closed
-- and no creation path needs a fourth spelling.
--

ALTER TABLE session
    ADD COLUMN dispatching_module text,
    ADD COLUMN dispatch_ref uuid;

ALTER TABLE session
    DROP CONSTRAINT session_creation_cause_closed;

ALTER TABLE session
    ADD CONSTRAINT session_creation_cause_closed CHECK (
        creation_cause = ANY (ARRAY[
            'interactive'::text,
            'module_dispatched'::text,
            'delegated'::text
        ])
    );

ALTER TABLE session
    DROP CONSTRAINT session_delegated_cause_shape;

ALTER TABLE session
    ADD CONSTRAINT session_creation_cause_shape CHECK (
        ((creation_cause = 'interactive'::text)
            AND (spawning_tool_request_id IS NULL)
            AND (dispatching_module IS NULL)
            AND (dispatch_ref IS NULL))
        OR ((creation_cause = 'module_dispatched'::text)
            AND (spawning_tool_request_id IS NULL)
            AND (ancestry_kind = 'none'::text)
            AND (dispatching_module IS NOT NULL)
            AND (dispatch_ref IS NOT NULL))
        OR ((creation_cause = 'delegated'::text)
            AND (ancestry_kind = 'none'::text)
            AND (spawning_tool_request_id IS NOT NULL)
            AND (dispatching_module IS NULL)
            AND (dispatch_ref IS NULL))
    );

--
-- The dispatching module is a closed set, not free text: §6's `module{name}`
-- names a module that exists, and a spelling with no producer is a placeholder
-- this schema refuses to hold.
--

ALTER TABLE session
    ADD CONSTRAINT session_dispatching_module_closed CHECK (
        (dispatching_module IS NULL)
        OR (dispatching_module = ANY (ARRAY[
            'repo_watch'::text,
            'commissioned_dispatch'::text
        ]))
    );

--
-- The lifecycle satellite.
--
-- One row per session, written in the creating transaction. State detail is
-- typed per state rather than pooled into one payload column, and each state's
-- shape constraint forbids the detail of every other state, so a stale
-- `waiting_kind` cannot survive a move to `active`.
--
-- `waiting`'s deadline and `recovering`'s bound are not repeated here: both are
-- the armed `session_deadline` row, which is where §1 puts the invariant. A
-- second copy could only disagree with it.
--

CREATE TABLE session_lifecycle (
    session_id uuid NOT NULL,
    state_kind text NOT NULL,
    state_entered_at timestamp with time zone DEFAULT statement_timestamp() NOT NULL,
    owned boolean NOT NULL,

    -- §6 actor classification of the transition that produced this state.
    actor_kind text NOT NULL,
    actor_module text,
    actor_turn_id uuid,
    actor_tool_request_id uuid,

    -- waiting{kind, waker}
    waiting_kind text,
    waiting_waker text,
    waiting_subject_session_id uuid,

    -- recovering{op}
    recovering_op text,

    -- blocked{reason, cycle}
    blocked_reason text,
    blocked_cycle bigint,

    -- parked{cause, responder, since}: why it parked, who is being waited on,
    -- and since when.
    parked_cause text,
    parked_responder text,
    parked_since timestamp with time zone,
    parked_standing_cause_kind text,

    -- terminal{outcome}
    ended_at timestamp with time zone,
    terminal_outcome_kind text,
    terminal_cause_kind text,
    terminal_stop_sticky boolean,
    terminal_superseded_by uuid,

    -- Settle-then-terminal handoff: the outcome a closure has committed to
    -- while a live turn is still settling. The turn-settlement transaction
    -- reads it and terminalizes, so no closure records terminal over a live
    -- turn (§1, §2).
    pending_terminal_outcome_kind text,
    pending_terminal_cause_kind text,
    pending_terminal_stop_sticky boolean,
    pending_terminal_superseded_by uuid,

    -- The actor that made the decision, kept with it. A settlement runs later,
    -- possibly in another worker or after a restart, and the §6 provenance the
    -- terminal row records is the deciding actor's -- not whichever caller
    -- happened to observe the turn reach its boundary.
    pending_terminal_actor_kind text,
    pending_terminal_actor_module text,
    pending_terminal_actor_turn_id uuid,
    pending_terminal_actor_tool_request_id uuid,

    -- §15 dispatch payload measurements. T11 records them on every dispatch
    -- path; the columns land here because they are per-session mutable values.
    payload_token_count bigint,
    payload_byte_count bigint,

    CONSTRAINT session_lifecycle_pkey PRIMARY KEY (session_id),

    CONSTRAINT session_lifecycle_state_closed CHECK (
        state_kind = ANY (ARRAY[
            'created'::text,
            'dispatched'::text,
            'active'::text,
            'waiting'::text,
            'recovering'::text,
            'blocked'::text,
            'parked'::text,
            'terminal'::text
        ])
    ),

    CONSTRAINT session_lifecycle_actor_closed CHECK (
        actor_kind = ANY (ARRAY[
            'core'::text,
            'operator'::text,
            'module'::text,
            'watchdog'::text
        ])
    ),

    -- `module{name}` needs its name; `core` keeps the exact model or tool
    -- identity behind the classification, and at most one of the two.
    CONSTRAINT session_lifecycle_actor_shape CHECK (
        ((actor_kind = 'module'::text) = (actor_module IS NOT NULL))
        AND ((actor_module IS NULL) OR (actor_module = ANY (ARRAY[
            'repo_watch'::text,
            'commissioned_dispatch'::text
        ])))
        AND ((actor_turn_id IS NULL) OR (actor_tool_request_id IS NULL))
        AND ((actor_kind = 'core'::text)
             OR ((actor_turn_id IS NULL) AND (actor_tool_request_id IS NULL)))
    ),

    CONSTRAINT session_lifecycle_waiting_kind_closed CHECK (
        (waiting_kind IS NULL)
        OR (waiting_kind = ANY (ARRAY[
            'approval'::text,
            'external'::text,
            'child'::text,
            'provider_retry'::text,
            'pipeline'::text,
            'scheduler'::text
        ]))
    ),

    CONSTRAINT session_lifecycle_waiting_waker_closed CHECK (
        (waiting_waker IS NULL)
        OR (waiting_waker = ANY (ARRAY[
            'approval_decision'::text,
            'external_recheck'::text,
            'child_settlement'::text,
            'provider_backoff'::text,
            'pipeline_drain'::text,
            'scheduler_sweep'::text
        ]))
    ),

    -- §1's `external{gate}`, `provider_retry{backoff}` and `scheduler{fault}`
    -- members have no typed identity in the schema yet; the backoff and the
    -- recheck are the armed deadline, and the gate and fault land with the
    -- machinery that produces them. The child wait's subject exists today, so
    -- it is recorded and no other wait may carry one.
    CONSTRAINT session_lifecycle_waiting_shape CHECK (
        ((state_kind = 'waiting'::text)
            = ((waiting_kind IS NOT NULL) AND (waiting_waker IS NOT NULL)))
        AND ((state_kind = 'waiting'::text)
             OR ((waiting_kind IS NULL)
                 AND (waiting_waker IS NULL)
                 AND (waiting_subject_session_id IS NULL)))
        -- A child wait names its child: the loader cannot rebuild one without
        -- it, so a row missing it would commit and then be unreadable.
        AND ((waiting_kind = 'child'::text)
             = (waiting_subject_session_id IS NOT NULL))
        -- Each kind designates exactly one waker, so the pair is constrained
        -- rather than merely both present.
        AND ((waiting_kind IS NULL) OR ((waiting_kind, waiting_waker) = ANY (ARRAY[
            ('approval'::text, 'approval_decision'::text),
            ('external'::text, 'external_recheck'::text),
            ('child'::text, 'child_settlement'::text),
            ('provider_retry'::text, 'provider_backoff'::text),
            ('pipeline'::text, 'pipeline_drain'::text),
            ('scheduler'::text, 'scheduler_sweep'::text)
        ])))
    ),

    CONSTRAINT session_lifecycle_recovering_op_closed CHECK (
        (recovering_op IS NULL)
        OR (recovering_op = ANY (ARRAY[
            'model_call'::text,
            'tool'::text,
            'runner'::text
        ]))
    ),

    CONSTRAINT session_lifecycle_recovering_shape CHECK (
        (state_kind = 'recovering'::text) = (recovering_op IS NOT NULL)
    ),

    -- The blocked reasons are the committed goal-mode reasons; the cycle is
    -- how many resumes this blocked generation has already had.
    CONSTRAINT session_lifecycle_blocked_reason_closed CHECK (
        (blocked_reason IS NULL)
        OR (blocked_reason = ANY (ARRAY[
            'user_input_required'::text,
            'external_change_required'::text,
            'authorization_required'::text,
            'execution_failure'::text
        ]))
    ),

    -- Stated column by column, not as "the payload is complete": a partial
    -- payload satisfies both sides of an equivalence and then survives on a
    -- state that ignores it.
    CONSTRAINT session_lifecycle_blocked_shape CHECK (
        ((state_kind = 'blocked'::text)
            = ((blocked_reason IS NOT NULL) AND (blocked_cycle IS NOT NULL)))
        AND ((state_kind = 'blocked'::text)
             OR ((blocked_reason IS NULL) AND (blocked_cycle IS NULL)))
        AND ((blocked_cycle IS NULL) OR (blocked_cycle >= 0))
    ),

    CONSTRAINT session_lifecycle_parked_cause_closed CHECK (
        (parked_cause IS NULL)
        OR (parked_cause = ANY (ARRAY[
            'progress_budget_exhausted'::text,
            'retry_budget_exhausted'::text,
            'structural_failure'::text,
            'unknown_failure'::text,
            'active_stall_deadline_expired'::text,
            'waiting_deadline_expired'::text,
            'recovering_deadline_expired'::text,
            'blocked_deadline_expired'::text,
            'operator_hold'::text,
            'module_park'::text
        ]))
    ),

    CONSTRAINT session_lifecycle_parked_shape CHECK (
        ((state_kind = 'parked'::text)
            = ((parked_cause IS NOT NULL)
               AND (parked_responder IS NOT NULL)
               AND (parked_since IS NOT NULL)))
        AND ((parked_responder IS NULL) OR (parked_responder = ANY (ARRAY[
            'operator'::text,
            'repo_watch'::text,
            'commissioned_dispatch'::text
        ])))
        AND ((state_kind = 'parked'::text)
             OR ((parked_cause IS NULL)
                 AND (parked_responder IS NULL)
                 AND (parked_since IS NULL)
                 AND (parked_standing_cause_kind IS NULL)))
        AND ((parked_standing_cause_kind IS NULL) OR (parked_cause IS NOT NULL))
    ),

    CONSTRAINT session_lifecycle_terminal_outcome_closed CHECK (
        (terminal_outcome_kind IS NULL)
        OR (terminal_outcome_kind = ANY (ARRAY[
            'achieved_verified'::text,
            'failed_retryable'::text,
            'failed_structural'::text,
            'failed_unknown'::text,
            'stopped'::text,
            'superseded'::text,
            'abandoned'::text,
            'retired'::text
        ]))
    ),

    -- One cause vocabulary, scoped per outcome by the shape constraint below.
    CONSTRAINT session_lifecycle_terminal_cause_closed CHECK (
        (terminal_cause_kind IS NULL)
        OR (terminal_cause_kind = ANY (ARRAY[
            'provider_transient'::text,
            'provider_quota_exhausted'::text,
            'provider_overloaded'::text,
            'infrastructure_failure'::text,
            'retry_budget_exhausted'::text,
            'context_compaction_wall'::text,
            'context_headroom_exhausted'::text,
            'broken_toolchain'::text,
            'moderation_block'::text,
            'dispatch_deadline_expired'::text,
            'start_gate_deadline_expired'::text,
            'first_input_deadline_expired'::text,
            'stranded_queued_turn'::text
        ]))
    ),

    CONSTRAINT session_lifecycle_parked_standing_cause_closed CHECK (
        (parked_standing_cause_kind IS NULL)
        OR (parked_standing_cause_kind = ANY (ARRAY[
            'provider_transient'::text,
            'provider_quota_exhausted'::text,
            'provider_overloaded'::text,
            'infrastructure_failure'::text,
            'retry_budget_exhausted'::text,
            'context_compaction_wall'::text,
            'context_headroom_exhausted'::text,
            'broken_toolchain'::text,
            'moderation_block'::text
        ]))
    ),

    -- Terminal is exactly `ended_at` plus an outcome, and each outcome carries
    -- exactly the members §2 gives it.
    CONSTRAINT session_lifecycle_terminal_shape CHECK (
        ((state_kind = 'terminal'::text)
            = ((ended_at IS NOT NULL) AND (terminal_outcome_kind IS NOT NULL)))
        -- Guarded against a null outcome: `NULL = 'stopped'` is NULL, and a
        -- CHECK accepts NULL.
        AND ((terminal_outcome_kind IS NULL)
             OR ((terminal_outcome_kind = 'stopped'::text)
                 = (terminal_stop_sticky IS NOT NULL)))
        AND ((state_kind = 'terminal'::text) = (ended_at IS NOT NULL))
        AND ((state_kind = 'terminal'::text) = (terminal_outcome_kind IS NOT NULL))
        AND ((terminal_outcome_kind IS NOT NULL)
             OR ((terminal_stop_sticky IS NULL)
                 AND (terminal_superseded_by IS NULL)
                 AND (terminal_cause_kind IS NULL)
                 AND (ended_at IS NULL)))
        AND ((terminal_superseded_by IS NULL)
             OR (terminal_outcome_kind = 'superseded'::text))
        AND ((terminal_superseded_by IS NULL)
             OR (terminal_superseded_by <> session_id))
        AND (
            (terminal_outcome_kind IS NULL AND terminal_cause_kind IS NULL)
            OR (terminal_outcome_kind = ANY (ARRAY[
                    'achieved_verified'::text,
                    'failed_unknown'::text,
                    'stopped'::text,
                    'superseded'::text,
                    'abandoned'::text
                ]) AND terminal_cause_kind IS NULL)
            OR (terminal_outcome_kind = 'failed_retryable'::text
                AND terminal_cause_kind = ANY (ARRAY[
                    'provider_transient'::text,
                    'provider_quota_exhausted'::text,
                    'provider_overloaded'::text,
                    'infrastructure_failure'::text,
                    'retry_budget_exhausted'::text
                ]))
            OR (terminal_outcome_kind = 'failed_structural'::text
                AND terminal_cause_kind = ANY (ARRAY[
                    'context_compaction_wall'::text,
                    'context_headroom_exhausted'::text,
                    'broken_toolchain'::text,
                    'moderation_block'::text
                ]))
            OR (terminal_outcome_kind = 'retired'::text
                AND terminal_cause_kind = ANY (ARRAY[
                    'dispatch_deadline_expired'::text,
                    'start_gate_deadline_expired'::text,
                    'first_input_deadline_expired'::text,
                    'stranded_queued_turn'::text
                ]))
        )
    ),

    -- The handoff mirrors the terminal shape and exists only while the session
    -- is still non-terminal: settling it is what makes the session terminal.
    CONSTRAINT session_lifecycle_pending_terminal_shape CHECK (
        ((pending_terminal_outcome_kind IS NULL)
            OR (state_kind <> 'terminal'::text))
        AND ((pending_terminal_outcome_kind IS NOT NULL)
             OR ((pending_terminal_cause_kind IS NULL)
                 AND (pending_terminal_stop_sticky IS NULL)
                 AND (pending_terminal_superseded_by IS NULL)
                 AND (pending_terminal_actor_kind IS NULL)))
        -- A committed handoff always names its actor, and the actor's shape is
        -- the state actor's: one module name, or one acting identity, never
        -- both and never a bare classification that owes one.
        AND ((pending_terminal_outcome_kind IS NULL)
             = (pending_terminal_actor_kind IS NULL))
        AND ((pending_terminal_actor_kind IS NULL)
             OR (pending_terminal_actor_kind = ANY (ARRAY[
                    'core'::text,
                    'operator'::text,
                    'module'::text,
                    'watchdog'::text
                ])))
        AND ((pending_terminal_actor_kind IS NOT DISTINCT FROM 'module'::text)
             = (pending_terminal_actor_module IS NOT NULL))
        AND ((pending_terminal_actor_module IS NULL)
             OR (pending_terminal_actor_module = ANY (ARRAY[
                    'repo_watch'::text,
                    'commissioned_dispatch'::text
                ])))
        AND ((pending_terminal_actor_turn_id IS NULL)
             OR (pending_terminal_actor_tool_request_id IS NULL))
        AND ((pending_terminal_actor_kind IS NOT DISTINCT FROM 'core'::text)
             OR ((pending_terminal_actor_turn_id IS NULL)
                 AND (pending_terminal_actor_tool_request_id IS NULL)))
        AND ((pending_terminal_outcome_kind IS NULL)
             OR (pending_terminal_outcome_kind = ANY (ARRAY[
                    'achieved_verified'::text,
                    'failed_retryable'::text,
                    'failed_structural'::text,
                    'failed_unknown'::text,
                    'stopped'::text,
                    'superseded'::text,
                    'abandoned'::text,
                    'retired'::text
                ])))
        AND ((pending_terminal_outcome_kind IS NULL)
             OR ((pending_terminal_outcome_kind = 'stopped'::text)
                 = (pending_terminal_stop_sticky IS NOT NULL)))
        AND ((pending_terminal_superseded_by IS NULL)
             OR (pending_terminal_outcome_kind = 'superseded'::text))
        -- The pending shape carries the terminal shape's rules; a handoff
        -- settlement would reject can never be recorded.
        AND ((pending_terminal_superseded_by IS NULL)
             OR (pending_terminal_superseded_by <> session_id))
        -- The cause is scoped to its outcome here too: a handoff whose cause
        -- the terminal shape rejects can be committed and never settled.
        AND (
            (pending_terminal_outcome_kind IS NULL
                AND pending_terminal_cause_kind IS NULL)
            OR (pending_terminal_outcome_kind = ANY (ARRAY[
                    'achieved_verified'::text,
                    'failed_unknown'::text,
                    'stopped'::text,
                    'superseded'::text,
                    'abandoned'::text
                ]) AND pending_terminal_cause_kind IS NULL)
            OR (pending_terminal_outcome_kind = 'failed_retryable'::text
                AND pending_terminal_cause_kind = ANY (ARRAY[
                    'provider_transient'::text,
                    'provider_quota_exhausted'::text,
                    'provider_overloaded'::text,
                    'infrastructure_failure'::text,
                    'retry_budget_exhausted'::text
                ]))
            OR (pending_terminal_outcome_kind = 'failed_structural'::text
                AND pending_terminal_cause_kind = ANY (ARRAY[
                    'context_compaction_wall'::text,
                    'context_headroom_exhausted'::text,
                    'broken_toolchain'::text,
                    'moderation_block'::text
                ]))
            OR (pending_terminal_outcome_kind = 'retired'::text
                AND pending_terminal_cause_kind = ANY (ARRAY[
                    'dispatch_deadline_expired'::text,
                    'start_gate_deadline_expired'::text,
                    'first_input_deadline_expired'::text,
                    'stranded_queued_turn'::text
                ]))
        )
    ),

    CONSTRAINT session_lifecycle_payload_measurements_nonnegative CHECK (
        ((payload_token_count IS NULL) OR (payload_token_count >= 0))
        AND ((payload_byte_count IS NULL) OR (payload_byte_count >= 0))
    )
);

ALTER TABLE ONLY session_lifecycle
    ADD CONSTRAINT session_lifecycle_session_id_fkey
        FOREIGN KEY (session_id) REFERENCES session(session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT;

ALTER TABLE ONLY session_lifecycle
    ADD CONSTRAINT session_lifecycle_superseded_by_fkey
        FOREIGN KEY (terminal_superseded_by) REFERENCES session(session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT;

-- The pending successor is checked for the same reason.
ALTER TABLE ONLY session_lifecycle
    ADD CONSTRAINT session_lifecycle_pending_superseded_by_fkey
        FOREIGN KEY (pending_terminal_superseded_by) REFERENCES session(session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT;

--
-- Every session owns exactly one satellite, written in its own creating
-- transaction. The deferred back-reference is how `session_current_defaults`
-- already makes a satellite mandatory: a creation path that forgets the
-- lifecycle row fails at commit instead of leaving a stateless session behind.
--

ALTER TABLE ONLY session
    ADD CONSTRAINT session_lifecycle_fk
        FOREIGN KEY (session_id) REFERENCES session_lifecycle(session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

--
-- A child wait names a child this session actually spawned. Without the key
-- the subject is any UUID, and the loader rebuilds it into a wait on a
-- relationship the delegation records do not have.
--

ALTER TABLE session_delegation
    ADD CONSTRAINT session_delegation_parent_child_key
        UNIQUE (parent_session_id, child_session_id);

ALTER TABLE ONLY session_lifecycle
    ADD CONSTRAINT session_lifecycle_waiting_child_fk
        FOREIGN KEY (session_id, waiting_subject_session_id)
        REFERENCES session_delegation(parent_session_id, child_session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

--
-- The recorded agency is this session's own. Both columns are session-scoped
-- foreign keys, so a turn or request from another session cannot be recorded
-- as this one's actor.
--

ALTER TABLE ONLY session_lifecycle
    ADD CONSTRAINT session_lifecycle_actor_turn_fk
        FOREIGN KEY (actor_turn_id, session_id)
        REFERENCES turn_lifecycle(turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE ONLY session_lifecycle
    ADD CONSTRAINT session_lifecycle_actor_tool_request_fk
        FOREIGN KEY (actor_tool_request_id, session_id)
        REFERENCES tool_request(request_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

--
-- The operator queue is `SELECT * FROM session_lifecycle WHERE state = 'parked'`
-- (§1), and the eligibility sweep and liveness watchdog now read the state to
-- skip parked sessions. Both want the state, not the session.
--

CREATE INDEX session_lifecycle_by_state
    ON session_lifecycle (state_kind, session_id);

--
-- §1's armed-deadline record. At most one row per session by primary key;
-- the invariant trigger below supplies the "exactly one, while non-terminal
-- and owned" half.
--
-- `expires_at IS NULL` is the explicit unbounded marker a `none` bound
-- journals. §12's alarm never counts it; only a missing record is a violation.
--

CREATE TABLE session_deadline (
    session_id uuid NOT NULL,
    deadline_kind text NOT NULL,
    on_expiry_kind text NOT NULL,
    expires_at timestamp with time zone,
    armed_at timestamp with time zone DEFAULT statement_timestamp() NOT NULL,

    CONSTRAINT session_deadline_pkey PRIMARY KEY (session_id),

    CONSTRAINT session_deadline_kind_closed CHECK (
        deadline_kind = ANY (ARRAY[
            'dispatch'::text,
            'start_gate'::text,
            'first_input'::text,
            'active_stall'::text,
            'waiting_approval'::text,
            'waiting_external'::text,
            'waiting_child'::text,
            'waiting_provider_retry'::text,
            'waiting_pipeline'::text,
            'waiting_scheduler'::text,
            'recovering'::text,
            'blocked'::text,
            'parked_renotify'::text
        ])
    ),

    -- Every deadline's expiry is a defined transition (§1). Admission expiries
    -- retire; post-admission expiries park; a parked deadline re-notifies and
    -- re-arms without moving the session (§13).
    CONSTRAINT session_deadline_expiry_transition_defined CHECK (
        ((deadline_kind = ANY (ARRAY[
            'dispatch'::text, 'start_gate'::text, 'first_input'::text
        ])) AND (on_expiry_kind = 'retire'::text))
        OR ((deadline_kind = 'parked_renotify'::text)
            AND (on_expiry_kind = 'renotify'::text))
        OR ((deadline_kind = ANY (ARRAY[
            'active_stall'::text,
            'waiting_approval'::text,
            'waiting_external'::text,
            'waiting_child'::text,
            'waiting_provider_retry'::text,
            'waiting_pipeline'::text,
            'waiting_scheduler'::text,
            'recovering'::text,
            'blocked'::text
        ])) AND (on_expiry_kind = 'park'::text))
    )
);

ALTER TABLE ONLY session_deadline
    ADD CONSTRAINT session_deadline_session_id_fkey
        FOREIGN KEY (session_id) REFERENCES session(session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT;

-- The expiry engine (T6) claims due deadlines in expiry order.
CREATE INDEX session_deadline_by_expiry
    ON session_deadline (expires_at, session_id)
    WHERE expires_at IS NOT NULL;

--
-- §6's journaled ownership transitions. Cohort membership for §12's metrics
-- follows this journal, so it is append-only and contiguous per session.
--

CREATE TABLE session_ownership_event (
    session_id uuid NOT NULL,
    event_ordinal bigint NOT NULL,
    transition_kind text NOT NULL,
    owned_after boolean NOT NULL,
    actor_kind text NOT NULL,
    actor_module text,
    actor_turn_id uuid,
    actor_tool_request_id uuid,
    recorded_at timestamp with time zone DEFAULT statement_timestamp() NOT NULL,

    CONSTRAINT session_ownership_event_pkey
        PRIMARY KEY (session_id, event_ordinal),

    CONSTRAINT session_ownership_event_ordinal_positive CHECK (event_ordinal >= 1),

    CONSTRAINT session_ownership_event_kind_closed CHECK (
        transition_kind = ANY (ARRAY[
            'created_owned'::text,
            'created_unmonitored'::text,
            'adopted'::text,
            'released'::text
        ])
    ),

    -- The transition and the resulting bit cannot disagree.
    CONSTRAINT session_ownership_event_shape CHECK (
        ((transition_kind = ANY (ARRAY['created_owned'::text, 'adopted'::text]))
            = owned_after)
    ),

    CONSTRAINT session_ownership_event_actor_closed CHECK (
        (actor_kind = ANY (ARRAY[
            'core'::text, 'operator'::text, 'module'::text, 'watchdog'::text
        ]))
        AND ((actor_kind = 'module'::text) = (actor_module IS NOT NULL))
        AND ((actor_module IS NULL) OR (actor_module = ANY (ARRAY[
            'repo_watch'::text,
            'commissioned_dispatch'::text
        ])))
        -- The exact model or tool identity behind a `core` classification.
        AND ((actor_turn_id IS NULL) OR (actor_tool_request_id IS NULL))
        AND ((actor_kind = 'core'::text)
             OR ((actor_turn_id IS NULL) AND (actor_tool_request_id IS NULL)))
    )
);

ALTER TABLE ONLY session_ownership_event
    ADD CONSTRAINT session_ownership_event_session_id_fkey
        FOREIGN KEY (session_id) REFERENCES session(session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT;

ALTER TABLE ONLY session_ownership_event
    ADD CONSTRAINT session_ownership_event_actor_turn_fk
        FOREIGN KEY (actor_turn_id, session_id)
        REFERENCES turn_lifecycle(turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE ONLY session_ownership_event
    ADD CONSTRAINT session_ownership_event_actor_tool_request_fk
        FOREIGN KEY (actor_tool_request_id, session_id)
        REFERENCES tool_request(request_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

--
-- The journal is contiguous from one and its head is the bit the rest of the
-- daemon reads. §12's cohort membership follows this journal, so a gap or a
-- flip recorded on only one of the two would make the metric and the
-- deadline machinery disagree about the same session.
--

CREATE FUNCTION require_session_ownership_journal_agrees(subject uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    events bigint;
    highest bigint;
    head boolean;
    owned_now boolean;
BEGIN
    SELECT owned INTO owned_now FROM session_lifecycle WHERE session_id = subject;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT count(*), COALESCE(max(event_ordinal), 0)
      INTO events, highest
      FROM session_ownership_event
     WHERE session_id = subject;

    IF events <> highest THEN
        RAISE EXCEPTION
            'session % ownership journal has % events through ordinal %',
            subject, events, highest
            USING ERRCODE = '23514';
    END IF;

    SELECT owned_after INTO head
      FROM session_ownership_event
     WHERE session_id = subject
     ORDER BY event_ordinal DESC
     LIMIT 1;

    IF NOT FOUND OR head IS DISTINCT FROM owned_now THEN
        RAISE EXCEPTION
            'session % ownership bit does not match its journal head', subject
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION require_session_ownership_journal() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM require_session_ownership_journal_agrees(NEW.session_id);
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER session_ownership_event_agrees_with_its_bit
    AFTER INSERT ON session_ownership_event
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_session_ownership_journal();

CREATE CONSTRAINT TRIGGER session_lifecycle_agrees_with_its_journal
    AFTER INSERT OR UPDATE ON session_lifecycle
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_session_ownership_journal();

CREATE TRIGGER session_ownership_event_is_append_only
    BEFORE DELETE OR UPDATE ON session_ownership_event
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

-- Row triggers do not fire for TRUNCATE, so append-only needs its statement
-- guard too.
CREATE FUNCTION reject_session_lifecycle_table_truncate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION 'session lifecycle history cannot be truncated'
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER session_ownership_event_truncate_is_rejected
    BEFORE TRUNCATE ON session_ownership_event
    FOR EACH STATEMENT EXECUTE FUNCTION reject_session_lifecycle_table_truncate();

-- The invariant triggers are row-level, so a truncation would disarm every
-- deadline at once and fire none of them.
CREATE TRIGGER session_deadline_truncate_is_rejected
    BEFORE TRUNCATE ON session_deadline
    FOR EACH STATEMENT EXECUTE FUNCTION reject_session_lifecycle_table_truncate();

CREATE TRIGGER session_lifecycle_truncate_is_rejected
    BEFORE TRUNCATE ON session_lifecycle
    FOR EACH STATEMENT EXECUTE FUNCTION reject_session_lifecycle_table_truncate();

--
-- Guards.
--

--
-- Terminal is final. Without this a later transition could reopen a closed
-- session, and every §12 cohort built on `ended_at` would move underneath the
-- week that already reported it.
--

CREATE FUNCTION guard_session_lifecycle_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'session lifecycle rows are never deleted'
            USING ERRCODE = '23514';
    END IF;
    IF OLD.state_kind = 'terminal' THEN
        RAISE EXCEPTION 'session lifecycle is terminal and cannot change'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.session_id IS DISTINCT FROM OLD.session_id THEN
        RAISE EXCEPTION 'session lifecycle identity is immutable'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER session_lifecycle_terminal_is_final
    BEFORE DELETE OR UPDATE ON session_lifecycle
    FOR EACH ROW EXECUTE FUNCTION guard_session_lifecycle_change();

--
-- §1's invariant, enforced rather than asserted: every non-terminal state of an
-- owned session carries exactly one armed deadline whose kind is the one that
-- state defines. An unmonitored session and a terminal session carry none.
-- A bound configured `none` still writes its record, with `expires_at` null as
-- the explicit unbounded marker.
--
-- Deferred, so one transaction may move the state and re-arm the deadline in
-- either order.
--

CREATE FUNCTION require_session_deadline_invariant_for(subject uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    lifecycle session_lifecycle%ROWTYPE;
    armed session_deadline%ROWTYPE;
    expected text;
BEGIN
    SELECT * INTO lifecycle FROM session_lifecycle WHERE session_id = subject;
    IF NOT FOUND THEN
        -- The session-level back-reference owns the missing-satellite case.
        RETURN;
    END IF;

    SELECT * INTO armed FROM session_deadline WHERE session_id = subject;

    IF lifecycle.state_kind = 'terminal' OR NOT lifecycle.owned THEN
        IF FOUND THEN
            RAISE EXCEPTION
                'session % holds an armed deadline while % and %',
                subject,
                lifecycle.state_kind,
                CASE WHEN lifecycle.owned THEN 'owned' ELSE 'unmonitored' END
                USING ERRCODE = '23514';
        END IF;
        RETURN;
    END IF;

    IF NOT FOUND THEN
        RAISE EXCEPTION
            'owned session % is % with no armed deadline', subject,
            lifecycle.state_kind
            USING ERRCODE = '23514';
    END IF;

    expected := CASE lifecycle.state_kind
        WHEN 'dispatched' THEN 'dispatch'
        WHEN 'active' THEN 'active_stall'
        WHEN 'recovering' THEN 'recovering'
        WHEN 'blocked' THEN 'blocked'
        WHEN 'parked' THEN 'parked_renotify'
        WHEN 'waiting' THEN 'waiting_' || lifecycle.waiting_kind
        ELSE NULL
    END;

    IF lifecycle.state_kind = 'created' THEN
        IF armed.deadline_kind NOT IN ('first_input', 'start_gate') THEN
            RAISE EXCEPTION
                'owned session % is created holding a % deadline', subject,
                armed.deadline_kind
                USING ERRCODE = '23514';
        END IF;
        RETURN;
    END IF;

    IF armed.deadline_kind IS DISTINCT FROM expected THEN
        RAISE EXCEPTION
            'owned session % is % holding a % deadline, not %', subject,
            lifecycle.state_kind, armed.deadline_kind, expected
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION require_session_deadline_invariant() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    -- A move between sessions leaves two sessions to check: validating only
    -- the destination would let the source lose its required deadline.
    IF TG_OP <> 'INSERT' THEN
        PERFORM require_session_deadline_invariant_for(OLD.session_id);
    END IF;
    IF TG_OP <> 'DELETE' THEN
        PERFORM require_session_deadline_invariant_for(NEW.session_id);
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER session_lifecycle_holds_its_deadline
    AFTER INSERT OR UPDATE ON session_lifecycle
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_session_deadline_invariant();

CREATE CONSTRAINT TRIGGER session_deadline_matches_its_state
    AFTER INSERT OR DELETE OR UPDATE ON session_deadline
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_session_deadline_invariant();

--
-- §1/§2: no terminal session leaves a non-terminal turn behind. Session
-- closures settle the live turn through the committed machinery first, and
-- this is what makes "first" true rather than intended.
--

CREATE FUNCTION require_terminal_session_has_no_live_turn() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    live uuid;
BEGIN
    IF NEW.state_kind <> 'terminal' THEN
        RETURN NULL;
    END IF;

    -- A queued goal turn already published as retired is not live work. The
    -- disposition that would let it reach `terminal` lands with the event
    -- vocabulary; requiring it here would make every closure of a dispatched
    -- goal session impossible.
    SELECT lifecycle.turn_id INTO live
      FROM turn_lifecycle AS lifecycle
     WHERE lifecycle.session_id = NEW.session_id
       AND lifecycle.state_kind <> 'terminal'
       AND NOT EXISTS (
            SELECT 1
              FROM goal_turn_retired_outbox_event AS retired
             WHERE retired.session_id = lifecycle.session_id
               AND retired.turn_id = lifecycle.turn_id
       )
     LIMIT 1;

    IF FOUND THEN
        RAISE EXCEPTION
            'terminal session % still holds non-terminal turn %',
            NEW.session_id, live
            USING ERRCODE = '23514';
    END IF;

    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER session_terminal_settles_its_turns
    AFTER INSERT OR UPDATE ON session_lifecycle
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_terminal_session_has_no_live_turn();

--
-- §2 closure settlement (codex finding F1): goal state is the sole
-- continuation-stopping condition in the committed goal contract, so a
-- pursuing or resumable generation must never survive beneath a terminal
-- session. The lineage's last event decides: `commissioned`, `resumed`, and
-- `blocked` leave a live generation, and `superseded` starts its successor.
--

CREATE FUNCTION require_terminal_session_settles_its_goal() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    last_kind text;
    last_outcome text;
BEGIN
    IF NEW.state_kind <> 'terminal' THEN
        RETURN NULL;
    END IF;

    SELECT event_kind, session_outcome_kind INTO last_kind, last_outcome
      FROM goal_event
     WHERE session_id = NEW.session_id
     ORDER BY event_ordinal DESC
     LIMIT 1;

    IF NOT FOUND THEN
        RETURN NULL;
    END IF;

    IF last_kind NOT IN ('achieved', 'user_stopped', 'session_closed') THEN
        RAISE EXCEPTION
            'terminal session % leaves its goal generation live at %',
            NEW.session_id, last_kind
            USING ERRCODE = '23514';
    END IF;

    -- The two records describe one ending, so they must agree. Checking only
    -- that the goal is settled admits an `abandoned` session over a goal that
    -- recorded a retryable failure, and both rows reconstitute individually.
    IF NOT (
        (last_kind = 'achieved' AND NEW.terminal_outcome_kind = 'achieved_verified')
        OR (last_kind = 'user_stopped' AND NEW.terminal_outcome_kind = 'stopped')
        OR (last_kind = 'session_closed'
            AND last_outcome = NEW.terminal_outcome_kind)
    ) THEN
        RAISE EXCEPTION
            'terminal session % records % over a goal settled as %',
            NEW.session_id, NEW.terminal_outcome_kind,
            COALESCE(last_outcome, last_kind)
            USING ERRCODE = '23514';
    END IF;

    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER session_terminal_settles_its_goal
    AFTER INSERT OR UPDATE ON session_lifecycle
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_terminal_session_settles_its_goal();

--
-- Codex finding F1's event: the goal vocabulary gains one closed terminal kind
-- so every §2 session closure has a matching terminal goal event. It carries
-- the session outcome and the §6 classification of the actor that closed it.
--

-- The closure's agency gets its own columns rather than reusing the model
-- declaration's. `enforce_goal_model_declaration_request` validates every
-- non-null `model_tool_request_id` as an achieved-or-blocked `goal_declare`
-- request, and that column is globally unique, so a closure borrowing it would
-- be rejected at commit or collide with the declaration that already used it.
ALTER TABLE goal_event
    ADD COLUMN session_outcome_kind text,
    ADD COLUMN closure_actor_kind text,
    ADD COLUMN closure_actor_module text,
    ADD COLUMN closure_actor_turn_id uuid,
    ADD COLUMN closure_actor_tool_request_id uuid;

ALTER TABLE goal_event
    DROP CONSTRAINT goal_event_event_kind_check;

ALTER TABLE goal_event
    ADD CONSTRAINT goal_event_event_kind_check CHECK (
        event_kind = ANY (ARRAY[
            'commissioned'::text,
            'blocked'::text,
            'resumed'::text,
            'achieved'::text,
            'user_stopped'::text,
            'superseded'::text,
            'session_closed'::text
        ])
    );

ALTER TABLE goal_event
    ADD CONSTRAINT goal_event_session_closed_shape CHECK (
        ((event_kind = 'session_closed'::text)
            = ((session_outcome_kind IS NOT NULL)
               AND (closure_actor_kind IS NOT NULL)))
        AND ((session_outcome_kind IS NULL) OR (session_outcome_kind = ANY (ARRAY[
            'failed_retryable'::text,
            'failed_structural'::text,
            'failed_unknown'::text,
            'superseded'::text,
            'abandoned'::text,
            'retired'::text
        ])))
        AND ((closure_actor_kind IS NULL) OR (closure_actor_kind = ANY (ARRAY[
            'core'::text, 'operator'::text, 'module'::text, 'watchdog'::text
        ])))
        AND ((closure_actor_kind IS NULL)
             OR ((closure_actor_kind = 'module'::text)
                 = (closure_actor_module IS NOT NULL)))
        -- Column by column: requiring only that the pair is absent together
        -- lets an ordinary event carry one of them, which replay ignores.
        AND ((event_kind = 'session_closed'::text)
             OR ((session_outcome_kind IS NULL)
                 AND (closure_actor_kind IS NULL)
                 AND (closure_actor_module IS NULL)
                 AND (closure_actor_turn_id IS NULL)
                 AND (closure_actor_tool_request_id IS NULL)))
        AND ((closure_actor_module IS NULL) OR (closure_actor_module = ANY (ARRAY[
            'repo_watch'::text,
            'commissioned_dispatch'::text
        ])))
        -- A core closure keeps at most one acting identity, and no other
        -- classification keeps any: the reader rejects the combinations this
        -- would otherwise let commit, leaving the goal unreadable.
        AND ((closure_actor_turn_id IS NULL) OR (closure_actor_tool_request_id IS NULL))
        AND ((closure_actor_kind = 'core'::text)
             OR ((closure_actor_turn_id IS NULL)
                 AND (closure_actor_tool_request_id IS NULL)))
    );

--
-- The committed `goal_event_shape` constraint enumerates every kind, so the
-- new one joins it: a `session_closed` event carries no statement, reason,
-- need, guidance, report, or command/turn/request provenance — its outcome and
-- actor columns above are its whole payload.
--

ALTER TABLE goal_event
    DROP CONSTRAINT goal_event_shape;

ALTER TABLE goal_event
    ADD CONSTRAINT goal_event_shape CHECK (
        (((event_kind = 'commissioned'::text) AND (statement IS NOT NULL) AND (blocked_reason IS NULL) AND (need IS NULL) AND (guidance IS NULL) AND (report IS NULL) AND (user_command_id IS NOT NULL) AND (model_turn_id IS NULL) AND (model_tool_request_id IS NULL) AND (scheduler_turn_id IS NULL))
        OR ((event_kind = 'blocked'::text) AND (statement IS NULL) AND (blocked_reason IS NOT NULL) AND (need IS NOT NULL) AND (guidance IS NULL) AND (report IS NULL) AND (user_command_id IS NULL) AND (((blocked_reason = 'execution_failure'::text) AND (model_turn_id IS NULL) AND (model_tool_request_id IS NULL) AND (scheduler_turn_id IS NOT NULL)) OR ((blocked_reason <> 'execution_failure'::text) AND (model_turn_id IS NOT NULL) AND (model_tool_request_id IS NOT NULL) AND (scheduler_turn_id IS NULL))))
        OR ((event_kind = 'resumed'::text) AND (statement IS NULL) AND (blocked_reason IS NULL) AND (need IS NULL) AND (report IS NULL) AND (user_command_id IS NOT NULL) AND (model_turn_id IS NULL) AND (model_tool_request_id IS NULL) AND (scheduler_turn_id IS NULL))
        OR ((event_kind = 'achieved'::text) AND (statement IS NULL) AND (blocked_reason IS NULL) AND (need IS NULL) AND (guidance IS NULL) AND (report IS NOT NULL) AND (user_command_id IS NULL) AND (model_turn_id IS NOT NULL) AND (model_tool_request_id IS NOT NULL) AND (scheduler_turn_id IS NULL))
        OR ((event_kind = 'user_stopped'::text) AND (statement IS NULL) AND (blocked_reason IS NULL) AND (need IS NULL) AND (guidance IS NULL) AND (report IS NULL) AND (user_command_id IS NOT NULL) AND (model_turn_id IS NULL) AND (model_tool_request_id IS NULL) AND (scheduler_turn_id IS NULL))
        OR ((event_kind = 'superseded'::text) AND (statement IS NOT NULL) AND (blocked_reason IS NULL) AND (need IS NULL) AND (guidance IS NULL) AND (report IS NULL) AND (user_command_id IS NOT NULL) AND (model_turn_id IS NULL) AND (model_tool_request_id IS NULL) AND (scheduler_turn_id IS NULL))
        OR ((event_kind = 'session_closed'::text) AND (statement IS NULL) AND (blocked_reason IS NULL) AND (need IS NULL) AND (guidance IS NULL) AND (report IS NULL) AND (user_command_id IS NULL) AND (scheduler_turn_id IS NULL) AND (model_turn_id IS NULL) AND (model_tool_request_id IS NULL)))
    );

--
-- A tool-attributed closure names a real request in its own session. The
-- committed composite key is skipped when its turn member is null, which a
-- tool-only actor leaves null by construction.
--

CREATE FUNCTION require_goal_closure_actor_identity() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.event_kind <> 'session_closed' THEN
        RETURN NULL;
    END IF;

    IF NEW.closure_actor_tool_request_id IS NOT NULL AND NOT EXISTS (
        SELECT 1
          FROM tool_request AS request
         WHERE request.request_id = NEW.closure_actor_tool_request_id
           AND request.session_id = NEW.session_id
    ) THEN
        RAISE EXCEPTION
            'session closure for % names tool request %, which is not its own',
            NEW.session_id, NEW.closure_actor_tool_request_id
            USING ERRCODE = '23503';
    END IF;

    IF NEW.closure_actor_turn_id IS NOT NULL AND NOT EXISTS (
        SELECT 1
          FROM turn_lifecycle AS turn
         WHERE turn.turn_id = NEW.closure_actor_turn_id
           AND turn.session_id = NEW.session_id
    ) THEN
        RAISE EXCEPTION
            'session closure for % names turn %, which is not its own',
            NEW.session_id, NEW.closure_actor_turn_id
            USING ERRCODE = '23503';
    END IF;

    RETURN NULL;
END;
$$;

-- A trigger rather than a foreign key: the key would validate every historical
-- `achieved` and `blocked` row, and this rule is about the closure kind alone.
CREATE CONSTRAINT TRIGGER goal_event_closure_names_its_own_identity
    AFTER INSERT ON goal_event
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_goal_closure_actor_identity();

--
-- Deployment policy for the armed deadlines.
--
-- §1 lets a bound live in config or the database. It lives in both here: the
-- daemon writes its `[numeric_bounds]` policy into this table at startup, and
-- the arming below reads it. That is what lets the invariant be maintained by
-- the same statement that moves the state — no write path can move a session
-- and forget to re-arm, because it is not the write path that arms.
--
-- A null `bound` is the explicit unbounded marker a `none` policy journals.
-- The seeded rows are all unbounded, so a daemon that has not written its
-- policy yet still satisfies the invariant with a journaled record rather than
-- an absent one.
--

CREATE TABLE session_lifecycle_bound (
    deadline_kind text NOT NULL,
    bound interval,
    updated_at timestamp with time zone DEFAULT statement_timestamp() NOT NULL,

    CONSTRAINT session_lifecycle_bound_pkey PRIMARY KEY (deadline_kind),

    CONSTRAINT session_lifecycle_bound_kind_closed CHECK (
        deadline_kind = ANY (ARRAY[
            'dispatch'::text,
            'start_gate'::text,
            'first_input'::text,
            'active_stall'::text,
            'waiting_approval'::text,
            'waiting_external'::text,
            'waiting_child'::text,
            'waiting_provider_retry'::text,
            'waiting_pipeline'::text,
            'waiting_scheduler'::text,
            'recovering'::text,
            'blocked'::text,
            'parked_renotify'::text
        ])
    ),

    CONSTRAINT session_lifecycle_bound_positive CHECK (
        (bound IS NULL) OR (bound > '0'::interval)
    )
);

INSERT INTO session_lifecycle_bound (deadline_kind, bound)
SELECT kind, NULL
  FROM unnest(ARRAY[
        'dispatch',
        'start_gate',
        'first_input',
        'active_stall',
        'waiting_approval',
        'waiting_external',
        'waiting_child',
        'waiting_provider_retry',
        'waiting_pipeline',
        'waiting_scheduler',
        'recovering',
        'blocked',
        'parked_renotify'
       ]) AS kind;

-- Losing a policy row silently turns a configured bound into `none`.
CREATE TRIGGER session_lifecycle_bound_truncate_is_rejected
    BEFORE TRUNCATE ON session_lifecycle_bound
    FOR EACH STATEMENT EXECUTE FUNCTION reject_session_lifecycle_table_truncate();

--
-- Arming. The satellite's state decides which deadline is armed, and the
-- policy table decides when it expires; the trigger below is the only writer
-- of `session_deadline`'s armed record, so the §1 invariant holds by
-- construction rather than by every caller remembering.
--
-- Re-arming happens exactly when the state was entered, the ownership bit
-- moved, or the armed record disagrees with the state. An unrelated column
-- write — a payload measurement, a pending-terminal handoff — leaves a running
-- deadline running.
--

CREATE FUNCTION session_deadline_kind_for_state(
    state_kind text,
    waiting_kind text
) RETURNS text
    LANGUAGE sql
    IMMUTABLE
    AS $$
    SELECT CASE state_kind
        WHEN 'created' THEN 'first_input'
        WHEN 'dispatched' THEN 'dispatch'
        WHEN 'active' THEN 'active_stall'
        WHEN 'recovering' THEN 'recovering'
        WHEN 'blocked' THEN 'blocked'
        WHEN 'parked' THEN 'parked_renotify'
        WHEN 'waiting' THEN 'waiting_' || waiting_kind
        ELSE NULL
    END;
$$;

CREATE FUNCTION session_deadline_expiry_for_kind(deadline_kind text) RETURNS text
    LANGUAGE sql
    IMMUTABLE
    AS $$
    SELECT CASE
        WHEN deadline_kind IN ('dispatch', 'start_gate', 'first_input') THEN 'retire'
        WHEN deadline_kind = 'parked_renotify' THEN 'renotify'
        ELSE 'park'
    END;
$$;

CREATE FUNCTION arm_session_deadline() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    required text;
    policy interval;
    armed text;
BEGIN
    required := session_deadline_kind_for_state(NEW.state_kind, NEW.waiting_kind);

    IF NOT NEW.owned OR required IS NULL THEN
        DELETE FROM session_deadline WHERE session_id = NEW.session_id;
        RETURN NULL;
    END IF;

    SELECT deadline_kind INTO armed
      FROM session_deadline
     WHERE session_id = NEW.session_id;

    IF TG_OP = 'UPDATE'
       AND armed IS NOT DISTINCT FROM required
       AND OLD.owned = NEW.owned
       AND OLD.state_entered_at = NEW.state_entered_at
    THEN
        RETURN NULL;
    END IF;

    SELECT bound INTO policy
      FROM session_lifecycle_bound
     WHERE deadline_kind = required;

    INSERT INTO session_deadline
            (session_id, deadline_kind, on_expiry_kind, expires_at, armed_at)
         VALUES (
            NEW.session_id,
            required,
            session_deadline_expiry_for_kind(required),
            CASE WHEN policy IS NULL THEN NULL ELSE statement_timestamp() + policy END,
            statement_timestamp()
         )
    ON CONFLICT (session_id) DO UPDATE
       SET deadline_kind = EXCLUDED.deadline_kind,
           on_expiry_kind = EXCLUDED.on_expiry_kind,
           expires_at = EXCLUDED.expires_at,
           armed_at = EXCLUDED.armed_at;

    RETURN NULL;
END;
$$;

CREATE TRIGGER session_lifecycle_arms_its_deadline
    AFTER INSERT OR UPDATE ON session_lifecycle
    FOR EACH ROW EXECUTE FUNCTION arm_session_deadline();

--
-- The §1 mapping, projected rather than restated.
--
-- The session state follows the turn and goal machines in the same
-- transaction that moves them. Projecting it here is what makes "core updates
-- it in the same transaction as every turn or goal transition that changes the
-- mapping" true of every such path at once — including paths a later change
-- adds, which is the property a per-call-site update cannot have.
--
-- `parked` overrides the mapping: parking suspends a live turn in place and
-- the turn keeps its phase, so a projection that recomputed the state would
-- undo the park. `terminal` is final for the same structural reason.
--
-- Every turn-lifecycle writer acquires the scheduler lock before touching a
-- turn row, and that statement now acquires the satellite first, so this
-- projection's write is always inside the declared session-then-satellite-then-
-- scheduler prefix.
--

CREATE FUNCTION project_session_lifecycle(subject uuid, lifts_park boolean) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    held session_lifecycle%ROWTYPE;
    live_phase text;
    goal_turn uuid;
    goal_request uuid;
    actor text;
    live_child_request uuid;
    child uuid;
    queued boolean;
    goal_kind text;
    goal_reason text;
    goal_generation numeric(20,0);
    cycles bigint;
    next_state text;
    next_waiting_kind text;
    next_waiting_waker text;
    next_recovering_op text;
    next_blocked_reason text;
    next_blocked_cycle bigint;
BEGIN
    SELECT * INTO held FROM session_lifecycle WHERE session_id = subject;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    -- Terminal is final in both directions. As a projection no-op, a later
    -- turn or goal write would land beneath a closed session and then
    -- activate: the deferred terminal-turn constraint fires on lifecycle
    -- writes, and nothing re-fires it for the turn.
    IF held.state_kind = 'terminal' THEN
        RAISE EXCEPTION
            'session % is terminal and admits no further turn or goal work',
            subject
            USING ERRCODE = '23514';
    END IF;

    -- `parked` overrides the mapping. One write lifts it, the operator's goal
    -- resume, and the caller says so: inferring it from the lineage would let
    -- a park taken after an earlier resume be lifted by the next unrelated
    -- turn write.
    IF held.state_kind = 'parked' AND NOT lifts_park THEN
        RETURN;
    END IF;

    SELECT live.active_phase_kind, live.child_wait_request_id
      INTO live_phase, live_child_request
      FROM turn_lifecycle AS live
     WHERE live.session_id = subject
       AND live.state_kind = 'active'
       AND NOT live.delegation_runtime_terminal
     ORDER BY live.acceptance_position DESC
     LIMIT 1;

    -- The goal lineage is read only when it can decide the state: a live turn
    -- outranks it, and this runs on every turn write.
    IF live_phase IS NULL THEN
        SELECT event.event_kind, event.blocked_reason, event.generation,
               event.model_turn_id, event.model_tool_request_id
          INTO goal_kind, goal_reason, goal_generation,
               goal_turn, goal_request
          FROM goal_event AS event
         WHERE event.session_id = subject
         ORDER BY event.event_ordinal DESC
         LIMIT 1;
    END IF;

    IF live_phase IS NOT NULL THEN
        CASE live_phase
            WHEN 'running' THEN
                next_state := 'active';
            WHEN 'awaiting_tool_approval' THEN
                next_state := 'waiting';
                next_waiting_kind := 'approval';
                next_waiting_waker := 'approval_decision';
            WHEN 'awaiting_child' THEN
                next_state := 'waiting';
                next_waiting_kind := 'child';
                next_waiting_waker := 'child_settlement';
                SELECT waiting.child_session_id INTO child
                  FROM session_delegation_wait AS waiting
                 WHERE waiting.awaiting_tool_request_id = live_child_request
                   AND waiting.parent_session_id = subject;
            WHEN 'awaiting_model_call_recovery' THEN
                next_state := 'recovering';
                next_recovering_op := 'model_call';
            WHEN 'awaiting_tool_recovery' THEN
                next_state := 'recovering';
                next_recovering_op := 'tool';
            WHEN 'awaiting_runner_recovery' THEN
                next_state := 'recovering';
                next_recovering_op := 'runner';
            ELSE
                RAISE EXCEPTION 'unmapped active turn phase %', live_phase
                    USING ERRCODE = '23514';
        END CASE;
    ELSIF goal_kind = 'blocked' THEN
        SELECT count(*) INTO cycles
          FROM goal_event AS resumed
         WHERE resumed.session_id = subject
           AND resumed.generation = goal_generation
           AND resumed.event_kind = 'resumed';
        next_state := 'blocked';
        next_blocked_reason := goal_reason;
        next_blocked_cycle := cycles;
    ELSE
        SELECT EXISTS (
            SELECT 1
              FROM turn_lifecycle AS pending
             WHERE pending.session_id = subject
               AND pending.state_kind = 'queued'
        ) INTO queued;

        -- A creation stays `created` until its first turn is queued, and a
        -- dispatched session stays `dispatched` until one activates: the
        -- dispatch deadline is what covers a queued turn that never runs. A
        -- queued successor inside a live session never re-enters `dispatched`
        -- (§1) — the active stall deadline covers it, so the session reads
        -- `active` while the scheduler owes it a pass.
        IF held.state_kind = 'created' THEN
            next_state := CASE WHEN queued THEN 'dispatched' ELSE 'created' END;
        ELSIF held.state_kind = 'dispatched' THEN
            next_state := 'dispatched';
        ELSE
            next_state := 'active';
        END IF;
    END IF;

    IF held.state_kind = next_state
       AND held.waiting_kind IS NOT DISTINCT FROM next_waiting_kind
       AND held.waiting_subject_session_id IS NOT DISTINCT FROM child
       AND held.recovering_op IS NOT DISTINCT FROM next_recovering_op
       AND held.blocked_reason IS NOT DISTINCT FROM next_blocked_reason
       AND held.blocked_cycle IS NOT DISTINCT FROM next_blocked_cycle
    THEN
        RETURN;
    END IF;

    -- A projected transition is core machinery, and the live turn is the
    -- agency behind it; the creating actor is not.
    -- The identity recorded here is model or tool agency and nothing else. A
    -- turn activating, a phase moving, a scheduler-authored block: those are
    -- daemon machinery that happens to concern a turn, and the reader takes a
    -- stored turn to mean the model acted. Only a model-declared goal event
    -- carries one. A lift is the operator's own resume.
    IF lifts_park THEN
        actor := 'operator';
        goal_turn := NULL;
        goal_request := NULL;
    ELSE
        actor := 'core';
        IF live_phase IS NOT NULL THEN
            goal_turn := NULL;
            goal_request := NULL;
        END IF;
    END IF;

    UPDATE session_lifecycle
       SET state_kind = next_state,
           state_entered_at = statement_timestamp(),
           actor_kind = actor,
           actor_module = NULL,
           actor_turn_id = goal_turn,
           actor_tool_request_id = CASE
               WHEN goal_turn IS NULL THEN goal_request
               ELSE NULL
           END,
           waiting_kind = next_waiting_kind,
           waiting_waker = next_waiting_waker,
           waiting_subject_session_id = child,
           recovering_op = next_recovering_op,
           blocked_reason = next_blocked_reason,
           blocked_cycle = next_blocked_cycle,
           parked_cause = NULL,
           parked_responder = NULL,
           parked_since = NULL,
           parked_standing_cause_kind = NULL
     WHERE session_id = subject;
END;
$$;

CREATE FUNCTION project_session_lifecycle_from_turn() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM project_session_lifecycle(NEW.session_id, false);
    RETURN NULL;
END;
$$;

CREATE FUNCTION project_session_lifecycle_from_goal() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM project_session_lifecycle(
        NEW.session_id,
        NEW.event_kind = 'resumed'
    );
    RETURN NULL;
END;
$$;

-- Split, and the update arm fires only when a column the mapping reads moves.
-- A turn write that advances attempt pointers, frontier identities, or tool
-- round bookkeeping cannot change the session's state, and this trigger runs on
-- every one of them.
CREATE TRIGGER turn_lifecycle_projects_session_state_on_insert
    AFTER INSERT ON turn_lifecycle
    FOR EACH ROW EXECUTE FUNCTION project_session_lifecycle_from_turn();

CREATE TRIGGER turn_lifecycle_projects_session_state_on_update
    AFTER UPDATE ON turn_lifecycle
    FOR EACH ROW
    WHEN (OLD.state_kind IS DISTINCT FROM NEW.state_kind
          OR OLD.active_phase_kind IS DISTINCT FROM NEW.active_phase_kind
          OR OLD.child_wait_request_id IS DISTINCT FROM NEW.child_wait_request_id
          OR OLD.delegation_runtime_terminal IS DISTINCT FROM NEW.delegation_runtime_terminal)
    EXECUTE FUNCTION project_session_lifecycle_from_turn();

CREATE TRIGGER goal_event_projects_session_state
    AFTER INSERT ON goal_event
    FOR EACH ROW EXECUTE FUNCTION project_session_lifecycle_from_goal();

--
-- §2 resource release.
--
-- Every terminal outcome releases the session's held dispatch slot. The
-- release is already trigger-driven off a terminal goal event, so the closure's
-- own `session_closed` event joins the gate rather than growing a second
-- release path that could disagree with the first.
--

CREATE OR REPLACE FUNCTION repo_watch_release_completed_dispatch_batches_for_goal()
RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.event_kind NOT IN
        ('blocked', 'achieved', 'user_stopped', 'session_closed')
    THEN
        RETURN NULL;
    END IF;
    PERFORM repo_watch_release_completed_dispatch_batches_for_turn(
        NULL::uuid,
        NEW.session_id
    );
    RETURN NULL;
END;
$$;

--
-- What a closure cannot release by itself. §2 gives `abandoned` cleanup
-- obligations for worktrees and containers: an operator write-off leaves live
-- resources behind, unlike an achievement or a supersession, whose successor
-- takes them. The obligation is recorded rather than performed here — the
-- cleanup runs outside the closing transaction, and a record is what lets it
-- be found.
--

CREATE TABLE session_cleanup_obligation (
    session_id uuid NOT NULL,
    outcome_kind text NOT NULL,
    recorded_at timestamp with time zone DEFAULT statement_timestamp() NOT NULL,
    discharged_at timestamp with time zone,

    CONSTRAINT session_cleanup_obligation_pkey PRIMARY KEY (session_id),

    CONSTRAINT session_cleanup_obligation_outcome_closed CHECK (
        outcome_kind = 'abandoned'::text
    )
);

ALTER TABLE ONLY session_cleanup_obligation
    ADD CONSTRAINT session_cleanup_obligation_session_id_fkey
        FOREIGN KEY (session_id) REFERENCES session(session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT;

CREATE TRIGGER session_cleanup_obligation_truncate_is_rejected
    BEFORE TRUNCATE ON session_cleanup_obligation
    FOR EACH STATEMENT EXECUTE FUNCTION reject_session_lifecycle_table_truncate();

CREATE INDEX session_cleanup_obligation_outstanding
    ON session_cleanup_obligation (recorded_at, session_id)
    WHERE discharged_at IS NULL;

--
-- The create-session command records the cause it created the session with,
-- and a composite foreign key ties the two together, so the command's closed
-- vocabulary widens with the session's. Delegated creation still has no
-- writer on this command family.
--

ALTER TABLE create_session_command
    DROP CONSTRAINT create_session_command_creation_cause_closed;

ALTER TABLE create_session_command
    ADD CONSTRAINT create_session_command_creation_cause_closed CHECK (
        creation_cause = ANY (ARRAY[
            'interactive'::text,
            'module_dispatched'::text
        ])
    );

--
-- The imported-frontier command family records `interactive` too: importing a
-- conversation is a user-initiated act, so its cause moves with the spelling
-- rather than acquiring a fourth one.
--

ALTER TABLE create_session_from_imported_frontier_command
    DROP CONSTRAINT create_session_from_imported_frontier_command_cause_closed;

ALTER TABLE create_session_from_imported_frontier_command
    ADD CONSTRAINT create_session_from_imported_frontier_command_cause_closed CHECK (
        creation_cause = 'interactive'::text
    );

--
-- Exactly one creation family per session, restated over the widened
-- vocabulary. A module-dispatched creation goes through the native family: it
-- is the same command, differing only in who issued it.
--

CREATE OR REPLACE FUNCTION require_session_creation_command() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE native_count bigint; imported_count bigint; delegated_count bigint;
BEGIN
    SELECT count(*) INTO native_count FROM create_session_command
     WHERE created_session_id = NEW.session_id;
    SELECT count(*) INTO imported_count FROM create_session_from_imported_frontier_command
     WHERE created_session_id = NEW.session_id;
    SELECT count(*) INTO delegated_count FROM session_delegation
     WHERE child_session_id = NEW.session_id;
    IF ((NEW.creation_cause IN ('interactive', 'module_dispatched'))
            AND NEW.ancestry_kind = 'none'
            AND (native_count, imported_count, delegated_count) <> (1, 0, 0))
        OR (NEW.creation_cause = 'interactive' AND NEW.ancestry_kind = 'imported_conversation'
            AND (native_count, imported_count, delegated_count) <> (0, 1, 0))
        OR (NEW.creation_cause = 'delegated'
            AND (native_count, imported_count, delegated_count) <> (0, 0, 1)) THEN
        RAISE EXCEPTION 'session % requires exactly one matching creation family', NEW.session_id
            USING ERRCODE = '23503', CONSTRAINT = 'session_requires_creation_command';
    END IF;
    RETURN NULL;
END;
$$;

--
-- The command records the dispatch it created the session for, and the
-- composite foreign key is widened to cover it, so the command and the session
-- cannot disagree about which dispatch a session serves. Without the widened
-- key the reference would live on one row only and the reconstitution
-- comparison would be comparing a value against itself.
--

ALTER TABLE create_session_command
    ADD COLUMN dispatching_module text,
    ADD COLUMN dispatch_ref uuid;

ALTER TABLE create_session_command
    ADD CONSTRAINT create_session_command_dispatch_shape CHECK (
        ((creation_cause = 'module_dispatched'::text)
            = ((dispatching_module IS NOT NULL) AND (dispatch_ref IS NOT NULL)))
        AND ((dispatching_module IS NULL) OR (dispatching_module = ANY (ARRAY[
            'repo_watch'::text,
            'commissioned_dispatch'::text
        ])))
    );

--
-- The committed composite foreign key still ties the command's cause to its
-- session's. It cannot carry the dispatch pair as well: those columns are null
-- for every interactive creation, and `MATCH SIMPLE` skips a composite foreign
-- key entirely when any member is null — so widening the key would silently
-- stop checking the cause it checks today. A deferred constraint trigger
-- states the agreement instead, and it fires whether or not the pair is null.
--

CREATE FUNCTION require_create_session_command_dispatch_matches() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    stored_module text;
    stored_dispatch uuid;
BEGIN
    SELECT dispatching_module, dispatch_ref
      INTO stored_module, stored_dispatch
      FROM session
     WHERE session_id = NEW.created_session_id;

    IF NOT FOUND THEN
        RETURN NULL;
    END IF;

    IF stored_module IS DISTINCT FROM NEW.dispatching_module
       OR stored_dispatch IS DISTINCT FROM NEW.dispatch_ref
    THEN
        RAISE EXCEPTION
            'create-session command % names a dispatch its session does not',
            NEW.command_id
            USING ERRCODE = '23514';
    END IF;

    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER create_session_command_dispatch_matches_session
    AFTER INSERT OR UPDATE ON create_session_command
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_create_session_command_dispatch_matches();

--
-- The committed goal-event continuity trigger enumerates every transition, so
-- the new terminal kind joins it: a pursuing or blocked generation admits a
-- session closure, and a generation already closed by one admits nothing —
-- not even a later commission, because the session that would run it is gone.
--

CREATE OR REPLACE FUNCTION require_goal_event_continuity() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    prior_ordinal numeric(20, 0);
    prior_generation numeric(20, 0);
    prior_kind text;
    current_generation numeric(20, 0);
BEGIN
    PERFORM 1 FROM session WHERE session_id = NEW.session_id FOR NO KEY UPDATE;
    SELECT event_ordinal, generation, event_kind
      INTO prior_ordinal, prior_generation, prior_kind
      FROM goal_event
     WHERE session_id = NEW.session_id
     ORDER BY event_ordinal DESC
     LIMIT 1;

    IF NOT FOUND THEN
        IF NEW.event_ordinal <> 1 OR NEW.generation <> 1
            OR NEW.event_kind <> 'commissioned' THEN
            RAISE EXCEPTION 'first goal event must commission generation one at ordinal one'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF NEW.event_ordinal <> prior_ordinal + 1 THEN
        RAISE EXCEPTION 'goal event ordinal must be contiguous'
            USING ERRCODE = '23514';
    END IF;
    current_generation := prior_generation
        + CASE
            WHEN prior_kind = 'superseded' THEN 1
            WHEN prior_kind IN ('achieved', 'user_stopped')
                AND NEW.event_kind = 'commissioned' THEN 1
            ELSE 0
          END;
    IF NEW.generation <> current_generation THEN
        RAISE EXCEPTION 'goal event generation does not name the current statement'
            USING ERRCODE = '23514';
    END IF;
    IF prior_kind IN ('achieved', 'user_stopped') AND NEW.event_kind <> 'commissioned' THEN
        RAISE EXCEPTION 'terminal goal generation admits only a later commission'
            USING ERRCODE = '23514';
    END IF;
    IF prior_kind = 'session_closed' THEN
        RAISE EXCEPTION 'a generation closed with its session admits no further event'
            USING ERRCODE = '23514';
    END IF;
    IF prior_kind = 'blocked'
        AND NEW.event_kind NOT IN ('resumed', 'user_stopped', 'superseded', 'session_closed') THEN
        RAISE EXCEPTION 'blocked goal admits only resume, stop, supersede, or session closure'
            USING ERRCODE = '23514';
    END IF;
    IF prior_kind IN ('commissioned', 'resumed', 'superseded')
        AND NEW.event_kind NOT IN
            ('blocked', 'achieved', 'user_stopped', 'superseded', 'session_closed') THEN
        RAISE EXCEPTION 'pursuing goal transition is invalid'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.event_kind = 'superseded'
        AND NEW.generation = 18446744073709551615 THEN
        RAISE EXCEPTION 'goal generation exhausted'
            USING ERRCODE = '22003';
    END IF;
    RETURN NEW;
END;
$$;
