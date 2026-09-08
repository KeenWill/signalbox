CREATE INDEX automatic_reconciliation_scheduled_due
    ON automatic_reconciliation (next_attempt_at, turn_id)
    WHERE state_kind = 'scheduled';

CREATE INDEX automatic_reconciliation_attempting_due
    ON automatic_reconciliation (next_attempt_at, turn_id)
    WHERE state_kind = 'attempting';
