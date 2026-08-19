-- Bounded in-flight lookups for the turn-liveness inventory.
--
-- The liveness watchdog reads its inventory once per scan interval and drains
-- the whole rotation before deciding anything, so every per-candidate lookup in
-- that statement must cost the same whatever a session's history weighs. Two of
-- its in-flight absence checks had no index leading with `session_id`:
-- `context_compaction_model_call` carries only its `model_call_id` primary key
-- and a `(model_call_id, session_id)` unique key, and `tool_attempt`'s live-row
-- index `tool_attempt_one_live_per_turn` is keyed by `turn_id`. Both tables are
-- append-only, so each check scanned an ever-growing global table once per
-- candidate.
--
-- The third check, over `model_call`, could reach
-- `model_call_usage_by_session_state_turn_call` and so looked covered, but is
-- not: `<> 'terminal'` is not one contiguous B-tree range, so that index
-- narrows to the session and then filters every historical call the session
-- ever made. Its cost grows with history exactly as the other two did.
--
-- Each index below is partial on exactly the predicate those checks use, so it
-- indexes only rows describing work still in flight. That set is bounded by how
-- much is happening at once rather than by how much has ever happened, so the
-- indexes stay small however much terminal history accrues behind them, and the
-- check reads an index entry rather than filtering one.

CREATE INDEX context_compaction_model_call_live_by_session
    ON context_compaction_model_call (session_id)
    WHERE state_kind <> 'terminal';

CREATE INDEX tool_attempt_live_by_session
    ON tool_attempt (session_id)
    WHERE state_kind <> 'terminal';

CREATE INDEX model_call_live_by_session
    ON model_call (session_id)
    WHERE state_kind <> 'terminal';
