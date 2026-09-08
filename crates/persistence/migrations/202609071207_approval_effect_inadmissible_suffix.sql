DO $$
DECLARE definition text;
BEGIN
    SELECT pg_get_functiondef('require_explicit_tool_approval_effect()'::regprocedure) INTO definition;
    definition := replace(definition,
        'AND earlier_decision.request_id IS NULL',
        'AND earlier_decision.request_id IS NULL AND earlier.inadmissible_reason IS NULL');
    definition := replace(definition,
        'AND later_decision.request_id IS NULL',
        'AND later_decision.request_id IS NULL AND later.inadmissible_reason IS NULL');
    definition := replace(definition,
        'AND undecided_decision.request_id IS NULL',
        'AND undecided_decision.request_id IS NULL AND undecided.inadmissible_reason IS NULL');
    EXECUTE definition;
END;
$$;
