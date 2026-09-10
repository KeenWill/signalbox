DO $migration$
DECLARE definition text;
BEGIN
    SELECT pg_get_functiondef('require_runner_placement_boundary()'::regprocedure)
      INTO definition;
    definition := replace(definition,
        'AND boundary.prefix_context_frontier_id = projected.frontier_id',
        'AND (boundary.prefix_context_frontier_id = projected.frontier_id
                    OR EXISTS (
                        SELECT 1 FROM context_compaction AS summary
                        WHERE summary.session_id = NEW.session_id
                          AND summary.result_frontier_id = boundary.prefix_context_frontier_id
                          AND context_frontier_preserves_prefix(NEW.session_id,
                              projected.frontier_id, summary.source_frontier_id)
                    ))');
    EXECUTE definition;
END;
$migration$;
