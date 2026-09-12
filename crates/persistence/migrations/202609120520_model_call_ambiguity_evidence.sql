-- growth: at most 4096 diagnostic bytes per model call, retained with that call.
-- retention: model-call retention owns these diagnostic fields.
ALTER TABLE model_call
    ADD COLUMN terminal_ambiguity_evidence text,
    ADD COLUMN terminal_ambiguity_evidence_original_bytes numeric(20,0),
    ADD CONSTRAINT model_call_ambiguity_evidence_bound CHECK (
        (terminal_ambiguity_evidence IS NULL AND terminal_ambiguity_evidence_original_bytes IS NULL)
        OR (terminal_ambiguity_evidence IS NOT NULL
            AND terminal_ambiguity_evidence_original_bytes IS NOT NULL
            AND octet_length(terminal_ambiguity_evidence) <= 4096
            AND terminal_ambiguity_evidence_original_bytes >= octet_length(terminal_ambiguity_evidence))
    );

CREATE FUNCTION retain_missing_model_call_ambiguity_report() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.state_kind <> 'terminal'
       AND NEW.state_kind = 'terminal'
       AND NEW.terminal_disposition_kind = 'ambiguous'
       AND NEW.terminal_ambiguity_evidence IS NULL THEN
        NEW.terminal_ambiguity_evidence :=
            E'classification_point=terminalization_without_runtime_report\nprior_call_state=' || OLD.state_kind;
        NEW.terminal_ambiguity_evidence_original_bytes := octet_length(NEW.terminal_ambiguity_evidence);
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER retain_missing_model_call_ambiguity_report
    BEFORE UPDATE ON model_call
    FOR EACH ROW EXECUTE FUNCTION retain_missing_model_call_ambiguity_report();
