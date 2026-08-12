-- Eval-owned recording for approval-judge eval runs and their verdicts.
--
-- The approval-judge-eval binary replays synthetic labeled cases that hold no
-- session, turn, or parked request, so its calls can never satisfy the
-- live-request linkage `tool_approval_judge_model_call` enforces through its
-- triggers (an active delegated wait and a reserved global call identity).
-- These dedicated tables record the same measurement the stdout scorecard
-- prints without claiming daemon provenance.

CREATE TABLE approval_judge_eval_run (
    eval_run_id uuid PRIMARY KEY,
    direct_model_selection_id uuid NOT NULL,
    -- The exact resolved target the calls were sent to. A selection is a
    -- mutable configuration mapping and provider_model spellings are not
    -- unique across targets, so neither can identify the invoked target
    -- after a configuration change; mirrors the column of the same name on
    -- tool_approval_judge_model_call.
    resolved_provider_model_identity_id uuid NOT NULL,
    provider_model text NOT NULL,
    -- Whether the resolved adapter's reported input total includes the cache
    -- axes, resolved once for the run's single frozen binding. Without it the
    -- stored token columns cannot be aggregated across adapter families, and
    -- provider_model cannot recover it later because adapter mappings are
    -- configurable; mirrors the column of the same name on
    -- tool_approval_judge_model_call.
    usage_input_includes_cache_tokens boolean NOT NULL,
    corpus_digest text NOT NULL,
    contract_digest text NOT NULL,
    rendered_digest text NOT NULL,
    repeats numeric(10, 0) NOT NULL,
    scorecard jsonb NOT NULL,

    CONSTRAINT approval_judge_eval_run_provider_model_nonempty
        CHECK (char_length(provider_model) > 0),
    CONSTRAINT approval_judge_eval_run_digests_nonempty
        CHECK (
            char_length(corpus_digest) > 0
            AND char_length(contract_digest) > 0
            AND char_length(rendered_digest) > 0
        ),
    -- Every repeat is a paid provider call, so the binary already rejects a
    -- zero; the u32 ceiling mirrors tool_request_ordinal_u32.
    CONSTRAINT approval_judge_eval_run_repeats_u32
        CHECK (repeats BETWEEN 1 AND 4294967295),
    CONSTRAINT approval_judge_eval_run_scorecard_shape
        CHECK (jsonb_typeof(scorecard) = 'object')
);

CREATE TABLE approval_judge_eval_call (
    eval_run_id uuid NOT NULL,
    case_name text NOT NULL,
    repeat_ordinal numeric(10, 0) NOT NULL,
    recommendation_kind text NOT NULL,
    rationale text NOT NULL,
    input_tokens numeric,
    output_tokens numeric,
    cache_creation_input_tokens numeric,
    cache_read_input_tokens numeric,

    PRIMARY KEY (eval_run_id, case_name, repeat_ordinal),
    CONSTRAINT approval_judge_eval_call_run_fk
        FOREIGN KEY (eval_run_id)
        REFERENCES approval_judge_eval_run (eval_run_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CONSTRAINT approval_judge_eval_call_case_name_nonempty
        CHECK (char_length(case_name) > 0),
    -- Ordinals count attempts from one; a failed attempt records no row, so
    -- a case's stored ordinals may hold gaps up to the run's repeats. The
    -- parent-relative upper bound is cross-table and is enforced by
    -- record_eval_run in the persistence crate, the one writer of these rows.
    CONSTRAINT approval_judge_eval_call_repeat_ordinal_u32
        CHECK (repeat_ordinal BETWEEN 1 AND 4294967295),
    CONSTRAINT approval_judge_eval_call_recommendation_closed
        CHECK (recommendation_kind IN ('approve', 'deny', 'escalate_to_human')),
    -- The same rationale bound the completed live judge call enforces.
    CONSTRAINT approval_judge_eval_call_rationale_bounded
        CHECK (octet_length(rationale) BETWEEN 1 AND 4096),
    CONSTRAINT approval_judge_eval_call_usage_u64_range
        CHECK (
            (
                input_tokens IS NULL
                OR (
                    input_tokens = trunc(input_tokens)
                    AND input_tokens BETWEEN 0 AND 18446744073709551615
                )
            )
            AND (
                output_tokens IS NULL
                OR (
                    output_tokens = trunc(output_tokens)
                    AND output_tokens BETWEEN 0 AND 18446744073709551615
                )
            )
            AND (
                cache_creation_input_tokens IS NULL
                OR (
                    cache_creation_input_tokens =
                        trunc(cache_creation_input_tokens)
                    AND cache_creation_input_tokens
                        BETWEEN 0 AND 18446744073709551615
                )
            )
            AND (
                cache_read_input_tokens IS NULL
                OR (
                    cache_read_input_tokens = trunc(cache_read_input_tokens)
                    AND cache_read_input_tokens
                        BETWEEN 0 AND 18446744073709551615
                )
            )
        )
);

-- Recorded runs are measurement evidence: a rewrite or removal after commit
-- would let a stored run stop carrying the complete verdict evidence the
-- recording API promises, so both tables refuse every change but insertion,
-- truncation included.
CREATE TRIGGER approval_judge_eval_run_is_append_only
BEFORE UPDATE OR DELETE ON approval_judge_eval_run
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER approval_judge_eval_run_cannot_be_truncated
BEFORE TRUNCATE ON approval_judge_eval_run
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER approval_judge_eval_call_is_append_only
BEFORE UPDATE OR DELETE ON approval_judge_eval_call
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER approval_judge_eval_call_cannot_be_truncated
BEFORE TRUNCATE ON approval_judge_eval_call
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();
