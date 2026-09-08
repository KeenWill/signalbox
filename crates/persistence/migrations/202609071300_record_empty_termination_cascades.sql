DO $$
BEGIN
    EXECUTE replace(
        pg_get_functiondef('materialize_session_delegation_termination_cascade(uuid, text)'::regprocedure),
        E'    IF disposition_count = 0 THEN\n        RETURN;\n    END IF;\n',
        ''
    );
END;
$$;
