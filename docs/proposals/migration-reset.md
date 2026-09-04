# Migration reset: collapse the chain to a zero baseline

Status: proposed for owner decision; design only.

## Why

Signalbox is pre-alpha with exactly one deployment: the dogfood daemon. The
migration chain is written as if arbitrary external installations must upgrade
across every historical schema shape, and that assumption is false. The cost of
maintaining it grows daily:

- **149 migrations, 65,093 lines of SQL.** 81 of the 149 files redefine
  something an earlier file created; the chain carries **125 superseded function
  definitions (14,204 dead SQL lines — 39.6% of all function LOC)**, 105
  superseded constraints, 27 triggers, 9 views, and 1,895 lines of one-shot
  backfill. One function is defined 14 times.
- Rust tests that stage historical database shapes, supersession bookkeeping,
  and spec prose that exists only to keep the full history walkable. The
  measured inventory (appendix) totals **~23,700–27,750 deletable lines**.
- A version-prefix allocation scheme that collides across parallel agent
  branches, forcing renumbering on merge (72 renames across 43 migrations; 95
  "renumber" commits). The reset does NOT cure this — collisions are
  branch-concurrency-relative, not chain-length-relative — it only shrinks the
  directory agents scan. The allocation scheme is a separate, small process
  question.

## What the reset is — and is not

**The reset is a chain collapse, not a schema redesign.** The baseline migration
reproduces the current schema exactly. Every planned schema improvement — the
repo-watch storage rewrite above all — lands as an ordinary forward migration
*on top of* the new baseline, in its own campaign. This keeps the reset
mechanical, verifiable, and days-scale.

**A second collapse is part of the plan, not a failure of it.** The trim
campaigns will pile forward migrations onto the fresh baseline, and that pile is
expected to accumulate superseded definitions in exactly the way the current
chain did. Once the campaigns land — and before any production database exists —
the same procedure runs again and re-baselines the result. Campaign migrations
therefore need not be minimal in count or form; the re-baseline erases them. The
production-DB era is what ends the collapse cycle, not this reset.

Out of scope: repo-watch frontier normalization, table drops, archive imports,
any data transformation.

## Data-preservation guarantee (binding constraint)

**Dogfood session and conversation data must survive; repo-watch data is
disposable.**

The design over-satisfies this: because the baseline equals the current schema,
the dogfood database is never dump-and-restored or transformed — the only dump
anywhere in the procedure is §3's precautionary safety copy, which nothing
consumes. The only mutation is to migration *bookkeeping* (`_sqlx_migrations`).
All data — sessions, conversations, and incidentally repo-watch — survives in
place. "Disposable" becomes relevant only in the later repo-watch campaign,
which may drop those tables by forward migration.

## Design

### 1. Generate the baseline

On a clean database, apply the full existing chain, then emit the schema:

```shell
pg_dump --schema-only --no-owner --no-privileges > baseline-dump.sql
```

Hand-clean the dump into a valid migration. Three of these steps are correctness
requirements the equivalence checks below must catch if skipped, not cosmetics:

- Drop the `pg_dump` preamble; keep extension statements and comment blocks
  worth keeping.
- **Drop `_sqlx_migrations` entirely** (or dump with
  `--exclude-table=_sqlx_migrations`). SQLx's migrator creates that table itself
  before applying the first pending migration (`crates/persistence/src/lib.rs`,
  `MIGRATOR.run`), so a baseline that recreates it fails on every fresh apply.
- **Neutralize schema qualifications.** The dump pins an empty `search_path` and
  hard-qualifies objects with the source database's schema, while the chain it
  replaces creates unqualified objects in the connection's `current_schema()` —
  `crates/persistence/tests/approval_judge_eval_postgres.rs` proves migration
  into a configured schema. The baseline stays unqualified.
- **Re-add the seed rows.** `--schema-only` discards required data the old chain
  installs: the `outbox_sequence_state`, `outbox_delivery_state`, and
  `hub_fence_state` singletons and both automatic-reconciliation cursors. A
  schema-perfect baseline without them fails daemon startup (`hub_fence.rs`
  treats the missing singleton as corruption). Carry those `INSERT`s into the
  baseline from the migrations that own them.

Split the cleaned dump into a few per-domain schema files (roughly one file per
domain, ordered so sqlx can apply them by filename, with foreign keys that would
point forward across files collected in a final file), and commit those as the
only migrations in an emptied `crates/persistence/migrations/` — emptied and
repopulated in the same reset commit, so no tree ever carries both chains (§4).
The old chain moves nowhere — git history is its archive.

The baseline version numbers start a new run — `202609010000_core.sql` through
`202609010014_cross_domain_foreign_keys.sql`, one consecutive prefix run
(timestamp format retained — sqlx expects it and tooling reads it). The files
are one schema and only apply as a whole; the fence boundary
(`HUB_FENCE_MIGRATION_VERSION`) names the last of them.

### 2. Prove equivalence

Two checks, run locally with their outputs recorded in the reset PR; neither is
wired into CI:

- **Fresh-apply equivalence:** schema-dump a database built by the old chain and
  one built by the baseline; the diff must be empty (modulo `_sqlx_migrations`
  contents). Run it twice — once on the default schema and once with the role's
  `search_path` pointing at a configured schema, the shape
  `crates/persistence/tests/approval_judge_eval_postgres.rs` stages — and dump
  the seed-row tables with data, so the singletons and cursors are compared and
  not only their DDL.
- **Live equivalence:** schema-dump the dogfood database and diff against the
  baseline-built schema. Any drift found here is a latent bug (a hand-applied
  hotfix or failed migration) and must be resolved before cutover.

Triggers, constraint names, index names, and function bodies all participate in
the diff — `pg_dump` output is normalized (sorted) by a small comparison script
that is scratch tooling, not committed to the repository.

### 3. Cut over the dogfood database

1. Stop the daemon (watchdog paused first).
2. Full safety dump (`pg_dump -Fc`) — a precaution only; the procedure never
   needs it.
3. In one transaction, with the table qualified to the daemon's configured
   migration schema (an operator session's `search_path` is not the daemon's):
   truncate `_sqlx_migrations`, insert one row per baseline file with the
   checksums sqlx computed for the new files.
4. Deploy the daemon binary built from the reset branch.
5. Verify: daemon boots, sessions list intact (spot-check counts against
   pre-cutover numbers for sessions, turns, conversations), new turn
   round-trips.
6. Resume watchdog.

Rollback at any step before a post-reset forward migration lands: restore the
old `_sqlx_migrations` rows (kept in the safety dump; same qualified table) and
redeploy the previous binary. Nothing else changed.

### 4. Delete the compat surface — precisely scoped

Two removals belong in the reset PR itself, not in follow-ups:

- the 149 old migration files go in the same commit that adds the baseline (§1's
  emptied directory): a tree carrying both chains would apply the old chain and
  then the higher-versioned baseline on a fresh database, colliding on every
  `CREATE`;
- the persistence-protocol migration prose in `docs/spec/` changes in the reset
  PR, never in a post-merge cleanup, and that edit records §3's
  `_sqlx_migrations` truncation as the exception to the immutability rule.

After the baseline merges, what goes next (separate small PRs):

- **~3,000–3,500 lines of Rust tests that stage historical database shapes** to
  walk the chain;
- the SR-9 supersession-naming checker (~150 lines of Python) and the rule it
  enforces.

What does NOT go:

- **Production decode paths for stored row shapes (~1,200 lines):** because the
  data survives in place, `storage_version` thresholds, the legacy repo-watch
  cursor JSONB reader, `ClaudeCodeSessionJsonlV1` (a stored shape, despite its
  name), and the other decoders still read rows that exist. They are removed
  only when an authorized migration rewrites every row they decode — never as
  part of this reset, and not automatically at the second collapse either: a
  collapse changes bookkeeping, not rows, so a reader retires there only if the
  intervening campaigns have already rewritten its rows.
- **`lock_inventory.rs` and its pinned CI hash** — deadlock-ordering machinery,
  not migration machinery.
- **The `search_path` pinning effect** of migration `202608200001`: the file is
  deleted with the chain, but the baseline must reproduce its effect and
  `search_path_postgres.rs` stays as the guard — losing it silently makes
  backups unrestorable.
- The 170 append-only triggers, the 21,628 lines of final function definitions,
  and the external conversation-import parsers (3,269 lines — they parse
  external files, not our history).

### 5. Rules after the reset

- **Forward-only immutability stands.** An applied migration is never edited;
  fixes are new migrations — `docs/spec/persistence-protocol.md` owns this rule.
  The reset removes only the requirement that a fresh database be able to replay
  pre-alpha history.
- **Version allocation:** timestamp prefixes continue, with the existing
  uniqueness check (`scripts/check_migration_versions.py`, the owner of that
  rule) retained — it is small and still needed. The supersession-naming rule is
  retired — a new migration carries no supersession naming.
- **Future resets stay cheap** while there is one deployment and no release:
  this document is the template, and repeating it is expected rather than
  exceptional. That era ends at the freeze condition in
  `docs/spec/process-protocol.md` — the first durable deployment, a client that
  cannot be rebuilt at will — which an owner-operated remote daemon or installed
  app triggers as an external installation does; from then on compatibility
  policy is decided explicitly.

## Sequencing

Reset first; trim campaigns land immediately behind it on the new baseline.
Unblocked by the reset, in order: archive imports (spec then build), repo-watch
storage rewrite, remaining trim campaigns.

## Risks

- **Live drift discovered by check 2** — the most likely surprise; it is work
  the reset surfaces, not work it creates. Budget a day for it.
- **sqlx offline metadata** — the query cache references no migration versions,
  but the reset PR must regenerate it to prove that.
- **In-flight migration PRs** — every open PR carrying a migration conflicts
  with an emptied directory. The reset PR merges in a declared freeze window;
  open migration PRs rebase to re-emit their SQL as forward migrations on the
  baseline.
- **CodeRabbit's migration-immutability check runs at `mode: error`** and will
  fire on a PR deleting 149 base-branch migration files. The reset PR adjusts
  that check in the same change.

## Appendix: measured deletable-compat inventory (2026-09-01)

Total **~23,700–27,750 LOC**: superseded SQL 20,500–24,000 (function bodies
alone: 14,204 across 125 superseded definitions; the largest are
`require_review_finding_event_sequence` 4 definitions/1,395 dead lines,
`require_semantic_entry_turn_state` 9 definitions,
`durable_command_storage_version_supported` 14 definitions) · Rust
historical-DB-staging tests 3,000–3,500 · Python SR-9 checker + tests 140–170 ·
docs prose 55–80.

Distribution notes: 81 of 149 files redefine earlier objects but only 15 are
pure amendments — the churn is distributed inside large feature migrations.
Repo-watch, despite owning 32 migrations/5,657 SQL lines, is *below* the
chain-wide churn average (24.6% vs ~32–35%); `review_finding` and
`turn_lifecycle` are worse per line. Version-prefix renumbering measured at 72
renames across 43 migrations and 95 renumber commits; unaffected by the reset
(see Why).
