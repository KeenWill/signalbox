# Signalbox schema audit: production-brain compromises

> Dated research intake (2026-07-24). Decision citations refer to entries in the
> [decision log](../decisions.md). Current requirements live in the
> [living specification](../spec/README.md), which supersedes any requirement
> stated here.

- Date: 2026-07-24
- Status: research intake, point-in-time and non-normative; where this document
  and the decision log or living specification disagree, those records win
- Audited snapshot: `origin/main` @ `2092c41` — the composed final schema across
  all 18 migration files, the persistence store, and the relevant specification
  pages
- Scope: nullable-column and enum-constraint discipline across all ~30 tables,
  store-side NULL handling, migration-history hygiene, and a squash assessment

**Disposition of findings.** Both actionable findings were dispatched the day
this audit was delivered. The single diseased-column finding
(`model_call.credential_reference`, section 2) was accepted by the owner the
same day, and its fix — a forward migration to `NOT NULL` plus the store
simplification that deletes the dead NULL-read branch — was commissioned
immediately. The squash assessment (section 3) feeds the "Migration baseline
reset" backlog item in the [agent backlog](../agents/backlog.md), to execute at
an owner-declared checkpoint under the pre-production schema discipline
decision.

## 1. Verdict: MINOR (schema) — with one genuinely diseased column and a substantial, cleanly-squashable migration-history tax

The composed final schema is the *opposite* of the rot pattern the owner fears.
Every text enum in all ~30 tables is closed by a CHECK constraint (zero
unchecked enum columns); every `numeric(20,0)` u64 carrier is range-checked to
`1..=2^64-1`; every nullable column except one is a conditionally-null member of
a discriminated union pinned by an exhaustive shape CHECK
(`accepted_input_delivery_shape`, `turn_lifecycle_state_payload_shape`,
`semantic_transcript_entry_payload_shape`, `session_ancestry_shape`,
`queued_input_origin_configuration_provenance_shape`, etc.), usually backed
again by write-once trigger guards and deferred cross-table final-state
assertions. The store fails closed on every NULL (`Corruption::Missing`), never
tolerantly defaults. There are no JSON blobs where columns belonged, no missing
FKs — if anything the schema is over-constrained relative to the domain.
However, the disease exists in exactly one column
(`model_call.credential_reference`) and — more interestingly — in the *migration
discipline itself*, which performs full production-deployment choreography
(guarded backfills, fail-closed ambiguity checks, `NOT VALID`/`VALIDATE`,
"historical rows" reasoning) against databases the repo's own decision log
certifies have never held production data. That ritual inflated 18 files/11,464
lines to roughly double what a baseline needs, but it left almost no residue in
the final schema, so a rewrite pass of the *schema* is not warranted; a squash
of the *files* is optional hygiene.

## 2. Findings table

| table.column / artifact                                                                                                  | issue                                                                                                                                                                                                                                                                                                                                                  | evidence                                                                                                                                                                                                                                            | classification                                                                                                                                                                                       | fix                                                                                                                |
| ------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| `model_call.credential_reference`                                                                                        | Nullable, unconstrained `text` for a value the store always writes and treats as corruption when NULL; nullability exists solely for "historical calls that predate this enforcement" in a DB with no history. App type is a total `String` (`ModelCallCredentialReference`, `crates/application/src/model_execution.rs:33`, no non-empty validation). | migration `202607220002:7` ("nullable column preserves forward migration of historical calls"); always bound at insert `crates/persistence/src/model_execution.rs:2447,2461`; NULL→`ModelCallCorruption::Missing` at `model_execution.rs:2477-2496` | disease (recorded in decisions.md as "forward-only nullable", but the premise is phantom history — the hub-fencing entry one day later records the owner confirming no database predates this stack) | `SET NOT NULL` + non-empty CHECK now (costs nothing); in a squash, fold into `model_call` CREATE in `202607220001` |
| `202607220003_failed_terminal_execution.sql` (whole file)                                                                | Production backfill choreography for dev-only rows: DO-block ambiguity validation (lines 10-64), `DISABLE TRIGGER`/guarded `UPDATE`/`ENABLE TRIGGER` (68-88), `ADD CONSTRAINT … NOT VALID` + `VALIDATE CONSTRAINT` (163-166) — reshaping a decision made the same day in `202607220001`                                                                | file:1-166; decisions.md "Failed-terminal provenance backfill fails closed"                                                                                                                                                                         | deliberate-recorded (process ritual, not schema damage — final shape is tight)                                                                                                                       | disappears entirely in a squash; its two columns + final shape CHECK fold into `turn_lifecycle`                    |
| `turn_lifecycle` terminal-failed shape                                                                                   | Tolerant disjunct `(terminal_attempt_id IS NULL AND terminal_model_call_id IS NULL) OR terminal_attempt_id IS NOT NULL` (202607220003:142-148) looks loose, but the deferred trigger `assert_failed_terminal_execution_final_state` (168-268) closes it exactly (direct static failure vs. execution-backed failure)                                   | same file                                                                                                                                                                                                                                           | genuine-optionality (failure without attempt is a real domain state)                                                                                                                                 | none                                                                                                               |
| `accepted_input.expected_defaults_version`, `.model_override_kind`, `.origin_turn_id`                                    | `DROP NOT NULL` in `202607180005:11-14` — smells like tolerance, but each is exactly pinned per `disposition_kind` by the rewritten `accepted_input_delivery_shape` (final version `202607220004:12-76`); domain counterpart is the `AcceptedInputDisposition` enum (`crates/domain/src/accepted_input.rs:177`)                                        | migrations 0005/220001/220004/220005                                                                                                                                                                                                                | genuine-optionality (union flattening for pending-steering receipts)                                                                                                                                 | none; in a squash the NOT NULLs are never declared then dropped                                                    |
| `queued_input_origin.defaults_version/requested_*/frozen_*/model_parameters/known_provider_failure_retry/model_fallback` | Six `DROP NOT NULL` in `202607220001:157-170` — all governed by `queued_input_origin_configuration_provenance_shape` (172-199): either full config or a `source_configuration_turn_id` reference, never partial                                                                                                                                        | migration 202607220001                                                                                                                                                                                                                              | genuine-optionality                                                                                                                                                                                  | none                                                                                                               |
| `turn_lifecycle.pinned_provider_model_identity_id`                                                                       | Nullable, absent from state-shape CHECK — but trigger-enforced: NULL at insert, write-once, settable only for the current running attempt (`202607220001:686-750`)                                                                                                                                                                                     | migration 202607220001                                                                                                                                                                                                                              | genuine-optionality                                                                                                                                                                                  | none                                                                                                               |
| `submit_input_command` / `session_defaults_version` / all command result columns                                         | Wide nullable result\_\* clusters — every combination exhaustively enumerated per `result_kind`×`rejection_kind` in shape CHECKs (e.g., `202607180003:196-289`, rewritten `202607180005:34-199`, `202607220005:13-…`)                                                                                                                                  | migrations 0003/0005/220005                                                                                                                                                                                                                         | genuine-optionality (typed command receipts)                                                                                                                                                         | none                                                                                                               |
| `numeric(20,0)` everywhere                                                                                               | Wide type for u64 — Postgres necessity, always bounded by CHECK                                                                                                                                                                                                                                                                                        | all files                                                                                                                                                                                                                                           | deliberate (persistence-protocol.md §Relational representation)                                                                                                                                      | none                                                                                                               |
| `turn_lifecycle.attempt_history_present … DEFAULT false`                                                                 | Only functional DEFAULT in the schema: store inserts omit it (`crates/persistence/src/submit_input.rs:2709`), the `turn_attempt` insert trigger maintains it. Not a validate-old-rows shim                                                                                                                                                             | `202607180004:48,616`                                                                                                                                                                                                                               | deliberate                                                                                                                                                                                           | none (keep)                                                                                                        |
| `hub_fence_state.singleton DEFAULT TRUE` + singleton seed INSERTs                                                        | Singleton-row pattern with self-seeding                                                                                                                                                                                                                                                                                                                | `202607230001:4-18`, `202607200002:29-30,51-52`                                                                                                                                                                                                     | deliberate                                                                                                                                                                                           | none                                                                                                               |
| 1 MiB content bound as separate migration                                                                                | Constraint a baseline would inline; the bound itself is an owner decision explicitly noting "No deployed row exceeds the bound (test databases only)"                                                                                                                                                                                                  | `202607200001`; decisions.md                                                                                                                                                                                                                        | deliberate-recorded                                                                                                                                                                                  | fold into baseline CREATE                                                                                          |
| `require_submit_input_legacy_effect_correlation`                                                                         | Migration-0003 validator renamed "legacy" (`202607220005:1140`) yet still live, partitioned by trigger WHEN clauses (1275-1313) — naming implies superseded code that is actually load-bearing                                                                                                                                                         | migration 202607220005                                                                                                                                                                                                                              | mild wake-residue of `submit_input`→`stop_requests` evolution                                                                                                                                        | rename in a squash; it becomes just "the non-interrupt validator"                                                  |
| Backfill/validation residue in 0002/0004/0005                                                                            | `DO` block validating "preexisting" rows (`202607180002:217-231`); backfill INSERTs into `session_scheduler` and `turn_lifecycle` (`202607180004:19-21,175-188`); comments "Existing version-one records remain valid" (0005:3), "Existing rows satisfy the bound trivially" (0200001:8), "historical NULL-to-NULL" (0220002:10-11)                    | as cited                                                                                                                                                                                                                                            | process ritual; zero schema residue                                                                                                                                                                  | all evaporates in a squash                                                                                         |
| `docs/spec/persistence-protocol.md:51-52`                                                                                | Stale doctrine: claims "sixteen files, 202607180001 through 202607240001"; there are 18, through `202607240003`                                                                                                                                                                                                                                        | spec vs. migrations dir                                                                                                                                                                                                                             | doc rot                                                                                                                                                                                              | update count or de-enumerate                                                                                       |

What the three named migrations left in their wake: `replace_session_defaults`
(0002) dropped 0001's one-kind reverse FK (`durable_command_typed_record_fk`)
and replaced it with the `require_durable_command_typed_record` trigger — clean
replacement, no orphans, but 0001's FK design lived one file.
`occupied_slot_submit_input` (0005) left the three conditionally-null
`accepted_input` columns (legitimate, union-governed) and
`result_actual_active_turn_id`; it rewrote 0003's result shapes wholesale two
days after they were written. `failed_terminal_execution` (220003) left the
tolerant-looking-but-trigger-closed terminal disjunct and the only sanctioned
immutability-trigger bypass in the history.

## 3. Squash assessment

- Files that vanish entirely (exist only to reshape same-week pre-production
  decisions): `202607200001` (bounded content), `202607200006` (index),
  `202607220002` (credential column), `202607220003` (failed-terminal
  provenance), `202607240003` (trigger-function rewrites only — its 729 lines
  are `CREATE OR REPLACE` of three validators). 5 of 18.
- Files that are >50% reshape ritual: `202607180005` (rewrites 0003's shapes),
  plus the widen-drop-readd fractions of `0002`, `0003`, `202607210001`,
  `202607220001`, `202607220004`, `202607220005`, `202607230001`,
  `202607240002`.
- Churn quantified: `require_semantic_entry_turn_state` is defined 6 times
  across the set; `require_outbox_event_typed_record` and
  `assert_turn_lifecycle_final_state` 5 times each;
  `accepted_input_delivery_shape`, `semantic_transcript_entry_payload_shape`,
  and `outbox_event_kind_closed` each written 5 times;
  `durable_command_kind_closed` and `turn_lifecycle_state_payload_shape` 4
  times; `submit_input_command_result_shape` 3 times. A baseline keeps only the
  final version of each.
- Resulting shape: a clean baseline lands at roughly 5–6 subsystem files
  (sessions/commands/defaults; input–turn–transcript–scheduling; outbox+event
  projections; model-call execution; process runtime/fence; conversation import
  — note `202607240001` is already a perfect example: pure CREATEs, no ALTERs)
  totaling an estimated ~6–7k lines vs. 11,464 — the other ~40% is superseded
  constraint/function versions and backfill choreography.
- Blocker is doctrinal, not technical: persistence-protocol.md records
  forward-only-checksummed-files as doctrine "so a deployed database's history
  is never silently edited" — while the hub-fencing decision (2026-07-23)
  records the owner confirming no deployment or database predates this stack and
  explicitly rejects claiming "a migration population that does not exist." The
  repo has already written down the factual premise for a squash; executing one
  needs an owner decision entry amending the doctrine plus recreating dev DBs
  (the sqlx checksum ledger makes edited history fail loudly — which on
  disposable DBs costs nothing).

## 4. Other production-brain-shaped observations

- Dead defensive branch: `load_call_credential_reference`
  (`crates/persistence/src/model_execution.rs:2477-2496`) carries a permanent
  corruption arm for a NULL that no from-scratch database can produce — the only
  store code defending against phantom history. Everything else the store
  defends against (missing rows, count mismatches) is genuinely reachable
  corruption; the two tolerant fallbacks found (`process_read.rs:1180`,
  `submit_input.rs:2201`) are semantically correct optionality, not compat.
- The compat instinct is linguistic, not structural: comments repeatedly address
  "pre-migration rows," "historical records," and "forward migration"
  (0002:214-216, 0005:3, 220002:2-4, 220003:4-5) — written for a production
  audience that doesn't exist, but crucially validated fail-closed instead of
  loosening, which is why the schema stayed clean while the prose rotted.
- Not disease, worth knowing: `storage_version smallint CHECK (= 1)` on every
  command/event table is versioning ceremony for a v1-only system — but it's
  closed, not tolerant, and is spec doctrine (identity-and-commands), so it's
  deliberate ceremony, not compromise.

Bottom line: one column to fix (`credential_reference` → NOT NULL + CHECK, plus
deleting its dead NULL-read branch), one stale spec sentence, and an optional
squash that would delete 5 files outright and ~40% of the SQL. The
nullable-column disease the owner's prior project had is essentially absent —
every other nullable is a constrained union member, and the store never papers
over a NULL.
