CREATE TABLE submit_input_stop_receipt (
    command_id uuid PRIMARY KEY REFERENCES submit_input_command(command_id)
);
CREATE TRIGGER submit_input_stop_receipt_immutable BEFORE UPDATE OR DELETE ON submit_input_stop_receipt
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
