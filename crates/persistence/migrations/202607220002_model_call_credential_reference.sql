-- ADR-0017: new model calls durably pin the non-secret credential reference
-- selected with their exact provider target. The nullable column preserves
-- forward migration of historical calls that predate this enforcement; the
-- adapter fails closed before resuming any Prepared call without a reference.

ALTER TABLE model_call
    ADD COLUMN credential_reference text;
