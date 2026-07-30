# PostgreSQL load harness

This on-demand Tokio harness measures the saturation curve of PostgreSQL
persistence work. It is not a latency microbenchmark: each curve point keeps a
fixed number of operations in flight for a fixed duration and reports completed
operations per second plus p50, p95, and p99 operation latency.

Run the complete sweep in the Cargo bench profile:

```console
devenv shell -- cargo bench -p signalbox-persistence \
  --features postgres-integration --bench postgres_load
```

The defaults run all three scenarios at concurrency 1, 2, 4, 8, 16, 32, and 64
for 10 seconds per point, first with fsync on and then with fsync off. The pool
opens 80 connections before measurement, above the highest offered concurrency.
Each point uses a fresh migrated database in the same mode-specific PostgreSQL
container, so accumulated rows and connection startup do not tilt the curve. Use
`-- --help` to select one scenario or mode, change the duration, or override the
sweep and pool size.

The scenarios are:

- `session_creation`: one atomic session creation, including its durable
  command, defaults, scheduler row, append-only records, and transactional
  outbox event.
- `full_path`: session creation, input submission, scheduler-locked turn
  activation, a completed model call proposing a tool, approval, and a completed
  tool attempt.
- `scheduler_lock`: session creation, input submission, and scheduler-locked
  turn activation on independent sessions. This isolates the common path through
  the per-session scheduler lock without the model and tool transactions.

Read each curve from low to high concurrency. The sustainable-throughput knee is
where additional concurrency no longer produces a meaningful throughput gain
while tail latency begins climbing. Report the whole curve because the knee and
post-knee behavior matter more than a single peak.

Every output row records the PostgreSQL image tag, detected host CPU count,
verified fsync setting, pool size, concurrency, and duration. Compare matching
fsync-on and fsync-off points. A large throughput gap or latency reduction with
fsync off points to durable-write I/O as the limiting cost; a small gap points
instead toward schema, locking, query, or application work.

The harness is deliberately absent from ordinary CI. Container scheduling,
filesystem behavior, and shared-host load make timings too noisy for a stable
pass/fail threshold. The harness prints measurements only and contains no
performance failure condition.
