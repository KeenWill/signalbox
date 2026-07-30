# PostgreSQL load harness

This on-demand Tokio harness measures the saturation curve of PostgreSQL
persistence work. It is not a latency microbenchmark: each curve point keeps a
fixed number of operations circulating during warmup and across the start of a
fixed offered-load duration, then drains the operations already started. It
reports operations completed during the offered-load window per offered-load
second. The final drain is excluded from throughput but retained in the
uncensored p50, p95, and p99 operation latency samples. Percentiles use the
empirical nearest-rank convention.

Run the complete sweep in the Cargo bench profile:

```console
devenv shell -- cargo bench -p signalbox-persistence \
  --features postgres-integration --bench postgres_load
```

The defaults run all three scenarios at concurrency 1, 2, 4, 8, 16, 32, and 64
for 10 seconds per point, first with fsync on and then with fsync off. The pool
opens 80 connections before measurement, above the highest offered concurrency.
Each point uses its own freshly started PostgreSQL container and migrated
database, so accumulated rows, cluster-wide maintenance, and connection startup
do not tilt the curve. Use `-- --help` to select one scenario or mode, change
the duration, or override the sweep and pool size.

The scenarios are:

- `session_creation`: one atomic session creation, including its durable
  command, defaults, scheduler row, append-only records, and transactional
  outbox event.
- `full_path`: session creation, input submission, scheduler-locked turn
  activation, model-call checkpoint and reload, a completed model call proposing
  a tool, approval, and a completed tool attempt.
- `scheduler_lock`: session creation, input submission, and scheduler-locked
  turn activation on independent sessions. This isolates the common path through
  the per-session scheduler lock without the model and tool transactions.

Read each curve from low to high concurrency. The sustainable-throughput knee is
where additional concurrency no longer produces a meaningful throughput gain
while tail latency begins climbing. Report the whole curve because the knee and
post-knee behavior matter more than a single peak.

Every output row records the PostgreSQL image tag, detected host CPU count,
verified fsync setting, pool size, configured server connection limit,
concurrency, configured offered-load duration, and actual elapsed duration
including the final drain. It separately reports completions inside the offered
window and the number of latency samples, which includes operations that started
inside the window and completed during the drain. Completed rows print
immediately, so a later point failure preserves earlier measurements. Compare
matching fsync-on and fsync-off points. A large throughput gap or latency
reduction with fsync off points to durable-write I/O as the limiting cost; a
small gap points instead toward schema, locking, query, or application work.

A single operation that does not return within 60 seconds ends its curve point
with an explicit liveness error, preventing an unresponsive database or
container from hanging the sweep indefinitely. This bound is an execution
safeguard, not a performance threshold; successful operations are never judged
against it.

The harness is deliberately absent from ordinary CI. Container scheduling,
filesystem behavior, and shared-host load make timings too noisy for a stable
pass/fail threshold. The harness prints measurements only and contains no
performance failure condition.
