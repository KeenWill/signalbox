-- Durable runner enrollment request receipts.

CREATE TABLE runner_enrollment_request_receipt (
    request_id uuid PRIMARY KEY,
    enrollment_id uuid NOT NULL UNIQUE,
    runner_id uuid NOT NULL UNIQUE,
    authentication_reference_id uuid NOT NULL UNIQUE,
    registration_revision numeric(20, 0) NOT NULL,

    CONSTRAINT runner_enrollment_request_receipt_initial_revision
        CHECK (registration_revision = 1),
    CONSTRAINT runner_enrollment_request_receipt_registration_fk
        FOREIGN KEY (
            enrollment_id,
            registration_revision,
            runner_id,
            authentication_reference_id
        )
        REFERENCES runner_registration (
            enrollment_id,
            registration_revision,
            runner_id,
            authentication_reference_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER runner_enrollment_request_receipt_is_append_only
BEFORE UPDATE OR DELETE ON runner_enrollment_request_receipt
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_enrollment_request_receipt_rejects_truncate
BEFORE TRUNCATE ON runner_enrollment_request_receipt
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();
