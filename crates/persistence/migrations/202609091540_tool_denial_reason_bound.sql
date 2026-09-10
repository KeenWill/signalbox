ALTER TABLE decide_tool_request_command
    DROP CONSTRAINT decide_tool_request_command_decision_shape,
    ADD CONSTRAINT decide_tool_request_command_decision_shape CHECK (
        (decision_kind = 'approve' AND denial_reason IS NULL)
        OR (decision_kind = 'deny' AND (
            denial_reason IS NULL
            OR (
                octet_length(denial_reason) BETWEEN 1 AND 4096
                AND denial_reason !~ '[\x01-\x09\x0b-\x1f\x7f]'
                AND denial_reason !~ '[\u0080-\u009f]'
                AND denial_reason !~ '^[ \t\n\x0b\x0c\r]'
                AND denial_reason !~ '[ \t\n\x0b\x0c\r]$'
            )
        ))
    );

ALTER TABLE tool_approval_decision
    DROP CONSTRAINT tool_approval_decision_shape,
    ADD CONSTRAINT tool_approval_decision_shape CHECK (
        (decision_kind = 'approve' AND denial_reason IS NULL)
        OR (decision_kind = 'deny' AND (
            denial_reason IS NULL
            OR (
                octet_length(denial_reason) BETWEEN 1 AND 4096
                AND denial_reason !~ '[\x01-\x09\x0b-\x1f\x7f]'
                AND denial_reason !~ '[\u0080-\u009f]'
                AND denial_reason !~ '^[ \t\n\x0b\x0c\r]'
                AND denial_reason !~ '[ \t\n\x0b\x0c\r]$'
            )
        ))
    );
