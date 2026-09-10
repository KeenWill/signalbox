ALTER TABLE tool_request
    ADD COLUMN inadmissible_limit numeric(20, 0),
    ADD COLUMN inadmissible_argument_bytes numeric(20, 0),
    DROP CONSTRAINT tool_request_arguments_bounded,
    DROP CONSTRAINT tool_request_inadmissible_shape,
    ADD CONSTRAINT tool_request_inadmissible_limit_u64 CHECK (
        inadmissible_limit BETWEEN 0 AND 18446744073709551615),
    ADD CONSTRAINT tool_request_inadmissible_argument_bytes_u64 CHECK (
        inadmissible_argument_bytes BETWEEN 0 AND 18446744073709551615),
    ADD CONSTRAINT tool_request_inadmissible_shape CHECK (
        (resolution_kind IS NULL AND inadmissible_reason IS NULL
            AND inadmissible_limit IS NULL AND inadmissible_argument_bytes IS NULL)
        OR (resolution_kind IS NOT NULL AND resolution_kind = 'closed_inadmissible'
            AND inadmissible_reason IS NOT NULL AND (
                (inadmissible_reason = 'placement_lost' AND inadmissible_limit IS NULL
                    AND inadmissible_argument_bytes IS NULL)
                OR (inadmissible_reason = 'proposal_limit_exceeded'
                    AND inadmissible_limit IS NOT NULL AND inadmissible_argument_bytes IS NULL)
                OR (inadmissible_reason = 'argument_bytes_exceeded'
                    AND inadmissible_limit IS NOT NULL AND inadmissible_argument_bytes IS NOT NULL
                    AND inadmissible_argument_bytes > inadmissible_limit)
            ))
    );

ALTER TABLE tool_round
    DROP CONSTRAINT tool_round_counts_bounded,
    ADD CONSTRAINT tool_round_counts_bounded CHECK (
        response_part_count BETWEEN 1 AND 4294967295
        AND request_count BETWEEN 1 AND response_part_count
    );

DO $migration$
DECLARE definition text;
BEGIN
    SELECT pg_get_functiondef('guard_tool_request_resolution()'::regprocedure)
      INTO definition;
    definition := replace(definition,
        'IF NEW.inadmissible_reason IS NOT NULL THEN',
        'IF NEW.inadmissible_reason = ''placement_lost'' THEN');
    EXECUTE definition;
END;
$migration$;

CREATE FUNCTION tool_inadmissible_error(reason text, admitted_limit numeric, argument_bytes numeric)
RETURNS jsonb LANGUAGE sql IMMUTABLE AS $$
    SELECT jsonb_build_object('kind',
        CASE WHEN reason = 'argument_bytes_exceeded' THEN 'invalid_arguments'
            ELSE 'execution_failed' END,
        'detail', CASE reason
            WHEN 'proposal_limit_exceeded' THEN
                format('proposal_limit_exceeded: maximum %s proposals per response', admitted_limit)
            WHEN 'argument_bytes_exceeded' THEN
                format('argument payload has %s bytes; maximum %s bytes', argument_bytes, admitted_limit)
            ELSE reason END)
$$;
