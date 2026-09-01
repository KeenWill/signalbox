# Migration reset: collapse the chain to a zero baseline

Status: proposed for owner decision; design only.

## Why

Signalbox is pre-alpha with exactly one deployment: the dogfood daemon. The
migration chain is written as if arbitrary external installations must upgrade
gently across every historical schema shape, and that assumption is false.
The cost of keeping it false grows daily:

- **149 migrations, 65,093 lines of SQL.** 81 of the 149 files redefine
  something an earlier file created; the chain carries **125 superseded
  function definitions (14,204 dead SQL lines — 39.6% of all function
  LOC)**, 105 superseded constraints, 27 triggers, 9 views, and 1,895
  lines of one-shot backfill. One function is defined 14 times.
- Rust tests that stage historical database shapes, supersession
  bookkeeping ceremony, and spec prose that exists only to keep the full
  history walkable. The measured inventory (appendix) totals
  **~23,700–27,750 deletable lines**.
- A version-prefix allocation scheme that collides across parallel agent
  branches, forcing renumber-on-merge archaeology (72 renames across 43
  migrations; 95 "renumber" commits). Honesty note: the reset does NOT
  cure this — collisions are branch-concurrency-relative, not
  chain-length-relative — it only shrinks the directory agents scan. The
  allocation scheme is a separate, small process question.

The owner green-lit the reset on 2026-08-11 and reaffirmed it on
2026-08-31. The 2026-08-15 immutability ruling ("applied migrations are
immutable and forward-only") was explicitly reconciled with it as "rule
now, reset later" — this is the "later."

## What the reset is — and is not

**The reset is a chain collapse, not a schema redesign.** The baseline
migration reproduces the current schema exactly. Every planned schema
improvement — the repo-watch storage rewrite above all — lands as an
ordinary forward migration *on top of* the new baseline, in its own
campaign. This keeps the reset mechanical, verifiable, and days-scale.

**A second collapse is part of the plan, not a failure of it.** The trim
campaigns will pile forward migrations onto the fresh baseline, and that
pile is expected to get ugly in exactly the way the current chain did.
Once the campaigns land — and before any production database exists — the
same procedure runs again and re-baselines the result. Campaign authors
should therefore not spend effort making their migrations pretty or
minimal-in-count; the re-baseline erases them. The production-DB era is
what ends the collapse cycle, not this reset.

Out of scope: repo-watch frontier normalization, table drops, archive
imports, any data transformation.

## Data-preservation guarantee (binding constraint)

Owner ruling, 2026-09-01: **dogfood session and conversation data must
survive; repo-watch data is disposable.**

The design over-satisfies this: because the baseline equals the current
schema, the dogfood database is never dumped, restored, or transformed.
The only mutation is to migration *bookkeeping* (`_sqlx_migrations`). All
data — sessions, conversations, and incidentally repo-watch — survives in
place. "Disposable" becomes relevant only in the later repo-watch
campaign, which may drop those tables by forward migration.

## Design

### 1. Generate the baseline

On a clean database, apply the full existing chain, then emit the schema:

```
pg_dump --schema-only --no-owner --no-privileges > <version>_baseline.sql
```

Hand-clean the dump (drop `pg_dump` preamble noise, keep extension
statements, preserve comment blocks worth keeping), and commit it as the
single migration in an emptied `crates/persistence/migrations/`. The old
chain moves nowhere — git history is its archive.

The baseline version number restarts the clock: `202609010000_baseline.sql`
(timestamp format retained — sqlx expects it and tooling reads it).

### 2. Prove equivalence

Two checks, run locally with their outputs pasted into the reset PR as
merge confirmations — no CI wiring, no ceremony that outlives the PR:

- **Fresh-apply equivalence:** schema-dump a database built by the old
  chain and one built by the baseline; the diff must be empty (modulo
  `_sqlx_migrations` contents).
- **Live equivalence:** schema-dump the dogfood database and diff against
  the baseline-built schema. Any drift found here is a real, latent bug
  today (a hand-applied hotfix or failed migration) and must be resolved
  before cutover, not papered over.

Triggers, constraint names, index names, and function bodies all
participate in the diff — `pg_dump` output is normalized (sorted) by a
small comparison script that is scratch tooling, not a repo commitment.

### 3. Cut over the dogfood database

1. Stop the daemon (watchdog paused first).
2. Full safety dump (`pg_dump -Fc`) — belt and braces only; the procedure
   never needs it.
3. In one transaction: truncate `_sqlx_migrations`, insert the baseline
   row with the checksum sqlx computed for the new file.
4. Deploy the daemon binary built from the reset branch.
5. Verify: daemon boots, sessions list intact (spot-check counts against
   pre-cutover numbers for sessions, turns, conversations), new turn
   round-trips.
6. Resume watchdog.

Rollback at any step before a post-reset forward migration lands: restore
the old `_sqlx_migrations` rows (kept in the safety dump) and redeploy the
previous binary. Nothing else changed.

### 4. Delete the compat surface — precisely scoped

After the baseline merges, what goes (separate small PRs, each showing the
surviving callers are none rather than asserting it):

- the 149 old migration files (git history is their archive);
- **~3,000–3,500 lines of Rust tests that stage historical database
  shapes** to walk the chain;
- the SR-9 supersession-naming checker and its ceremony (~150 lines of
  Python plus the rule);
- persistence-protocol prose documenting cross-version chain walking.

What explicitly does NOT go — the inventory caught my earlier draft
over-claiming here:

- **Production decode paths for stored row shapes (~1,200 lines):**
  because the data survives in place, `storage_version` thresholds, the
  legacy repo-watch cursor JSONB reader, `ClaudeCodeSessionJsonlV1` (a
  stored shape, despite its name), and kin still read rows that exist.
  They die only when a later campaign rewrites those rows — or at the
  second collapse — never as part of this reset.
- **`lock_inventory.rs` and its pinned CI hash** — deadlock-ordering
  machinery, not migration machinery.
- **The `search_path` pinning effect** of migration `202608200001`: the
  file dies with the chain, but the baseline must reproduce its effect
  and `search_path_postgres.rs` stays as the guard — losing it silently
  makes backups unrestorable.
- The 170 append-only triggers, the 21,628 lines of final function
  definitions, and the external conversation-import parsers (3,269
  lines — they parse external files, not our history).

### 5. Rules after the reset

- **Forward-only immutability stands.** An applied migration is still
  never edited; fixes are new migrations. What died is the requirement to
  keep pre-alpha history applyable forever.
- **Version allocation:** timestamp prefixes continue, with the existing
  uniqueness check retained (it is small and earns its keep). The
  supersession-naming ceremony is retired — a new migration is just a new
  migration.
- **Future resets stay cheap** while there is one deployment and no
  release: this document is the template, and repeating it is expected
  rather than exceptional. The first external installation ends that era.

## Sequencing

Owner ruling 2026-09-01: reset first; trim campaigns land immediately
behind it on the new baseline. Unblocked by the reset, in order: archive
imports (spec then build), repo-watch storage rewrite, remaining trim
campaigns.

## Risks

- **Live drift discovered by check 2** — the most likely surprise; it is
  work the reset surfaces, not work it creates. Budget a day for it.
- **sqlx offline metadata** — the query cache references no migration
  versions, but the reset PR must regenerate it to prove that.
- **In-flight migration PRs** — every open PR carrying a migration
  conflicts with an emptied directory. The reset PR merges in a declared
  freeze window; open migration PRs rebase to re-emit their SQL as
  forward migrations on the baseline.
- **CodeRabbit's migration-immutability check runs at `mode: error`** and
  will fire on a PR deleting 149 base-branch migration files. The reset
  PR adjusts that check in the same change, with this document as the
  cited authority.

## Appendix: measured deletable-compat inventory (2026-09-01)

Total **~23,700–27,750 LOC**: superseded SQL 20,500–24,000 (function
bodies alone: 14,204 across 125 superseded definitions; worst offenders
`require_review_finding_event_sequence` 4 definitions/1,395 dead lines,
`require_semantic_entry_turn_state` 9 definitions,
`durable_command_storage_version_supported` 14 definitions) · Rust
historical-DB-staging tests 3,000–3,500 · Python SR-9 checker + tests
140–170 · docs prose 55–80.

Distribution notes: 81 of 149 files redefine earlier objects but only 15
are pure amendments — the churn is smeared inside large feature
migrations. Repo-watch, despite owning 32 migrations/5,657 SQL lines, is
*below* the chain-wide churn average (24.6% vs ~32–35%); `review_finding`
and `turn_lifecycle` are worse per line. Version-prefix tax measured at
72 renames across 43 migrations and 95 renumber commits; unaffected by
the reset (see Why).
