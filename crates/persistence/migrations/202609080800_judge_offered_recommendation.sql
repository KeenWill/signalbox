ALTER TABLE tool_approval_judge_model_call
    ADD COLUMN offered_recommendation_kind text,
    ADD COLUMN substitution_cause text,
    ADD CONSTRAINT tool_approval_judge_offered_recommendation_shape CHECK ((
        (terminal_disposition_kind = 'completed'
            AND offered_recommendation_kind IS NOT NULL
            AND offered_recommendation_kind IN ('approve', 'deny', 'escalate_to_human')
            AND (
                (substitution_cause IS NULL
                    AND recommendation_kind = offered_recommendation_kind)
                OR (substitution_cause = 'authority_withdrawn'
                    AND offered_recommendation_kind IN ('approve', 'deny')
                    AND recommendation_kind = 'escalate_to_human')
            ))
        OR (terminal_disposition_kind IS DISTINCT FROM 'completed'
            AND offered_recommendation_kind IS NULL
            AND substitution_cause IS NULL)
    ) IS TRUE);
