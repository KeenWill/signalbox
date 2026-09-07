ALTER TABLE model_call_credential_pool_policy
    ADD COLUMN tie_break text NOT NULL DEFAULT 'first_listed'
        CHECK (tie_break IN ('first_listed', 'least_used')),
    ADD COLUMN headroom_reserve_percent smallint
        CHECK (headroom_reserve_percent BETWEEN 0 AND 99),
    ADD COLUMN on_headroom_low text NOT NULL DEFAULT 'stay'
        CHECK (on_headroom_low IN ('stay', 'switch_next_turn', 'avoid_new_sessions', 'quarantine'));

ALTER TABLE model_call_credential_pool_member
    ADD COLUMN headroom_reserve_percent smallint
        CHECK (headroom_reserve_percent BETWEEN 0 AND 99);

ALTER TABLE credential_pool_member_action
    DROP CONSTRAINT credential_pool_member_action_cause_kind_check,
    ADD CONSTRAINT credential_pool_member_action_cause_kind_check
        CHECK (cause_kind IN ('rate_limited', 'quota_exhausted', 'overloaded', 'credential_rejected', 'headroom_low'));
