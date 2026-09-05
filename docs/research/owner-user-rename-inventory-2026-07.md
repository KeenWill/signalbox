# Owner-to-user rename inventory

> Dated research intake (2026-07-25), non-normative. This page is the read-only
> inventory required by the [Owner-to-user backlog entry](../agents/backlog.md);
> it performs no rename and decides no terminology, compatibility, or migration
> policy. Historical decisions live in git history, and current requirements
> live in the [living specification](../spec/README.md).

- Date: 2026-07-25
- Status: complete point-in-time inventory; proposal-grade input to a later
  rename pass
- Audited snapshot: repository commit `e62f4ee1`
- Scope: every ownership-family occurrence in tracked text, classified as
  renameable platform-actor vocabulary, one of the three recorded carve-outs, or
  a judgment call
- Exclusion: this inventory file itself, because it did not exist in the audited
  snapshot

## 1. Result

The scan found **2,237 occurrences**. Every occurrence has one classification:

| Category  | Meaning                                                             |     Count |
| --------- | ------------------------------------------------------------------- | --------: |
| (a)       | Renameable platform actor                                           |       683 |
| (b)       | Personal repository-owner process voice                             |       103 |
| (c)       | Technical, data, filesystem, source-of-truth, or language ownership |     1,102 |
| (d)       | Historical `docs/decisions.md` text                                 |       309 |
| (e)       | Needs an owner ruling                                               |        40 |
| **Total** |                                                                     | **2,237** |

The important boundary is semantic, not path-wide. Process files are protected
only where “owner” means the repository owner personally. Nine occurrences in
the backlog instead name the platform actor and remain category (a):
`docs/agents/backlog.md:71,75,271,565,597,611,628,636,654`. Conversely, source
and specification files contain technical ownership that must not become “user”:
Rust ownership, owning aggregate or row identities, filesystem owner/mode
checks, a document owning a rule, and provider- or runtime-owned state.

### Count by crate or document area

Zeroes are explicit so the category totals can be checked mechanically.

| Area                            |     (a) |     (b) |       (c) |     (d) |    (e) |     Total |
| ------------------------------- | ------: | ------: | --------: | ------: | -----: | --------: |
| `apps/client`                   |       2 |       0 |         0 |       0 |      0 |         2 |
| `apps/hubd`                     |      16 |       0 |        64 |       0 |      0 |        80 |
| `clients/native`                |       0 |       0 |         3 |       0 |     13 |        16 |
| `crates/application`            |      59 |       0 |        67 |       0 |      0 |       126 |
| `crates/domain`                 |     268 |       0 |       342 |       0 |      0 |       610 |
| `crates/expect-table`           |       0 |       0 |        14 |       0 |      0 |        14 |
| `crates/model-provider-runtime` |       0 |       0 |         5 |       0 |      1 |         6 |
| `crates/model-runtime`          |       0 |       0 |         8 |       0 |      0 |         8 |
| `crates/model-runtime-openai`   |       0 |       0 |         6 |       0 |      0 |         6 |
| `crates/persistence`            |     174 |       0 |        82 |       0 |      2 |       258 |
| `crates/process-protocol`       |      21 |       0 |        11 |       0 |      0 |        32 |
| `docs/agents`                   |       9 |      80 |        80 |       0 |      0 |       169 |
| `docs/architecture.md`          |       6 |       0 |        12 |       0 |      0 |        18 |
| `docs/decisions.md`             |       0 |       0 |         0 |     309 |      0 |       309 |
| `docs/domain-spine.md`          |       8 |       0 |         8 |       0 |      1 |        17 |
| `docs/glossary.md`              |       2 |       0 |         6 |       0 |      0 |         8 |
| `docs/invariants.md`            |       6 |       0 |         8 |       0 |      0 |        14 |
| `docs/open-questions.md`        |       7 |       0 |        21 |       0 |      0 |        28 |
| `docs/research`                 |       0 |       0 |        40 |       0 |      9 |        49 |
| `docs/scenarios.md`             |      13 |       0 |        58 |       0 |      0 |        71 |
| `docs/spec`                     |      70 |       0 |       180 |       0 |     10 |       260 |
| `docs/style.md`                 |       0 |       0 |         4 |       0 |      0 |         4 |
| `docs/target-model.md`          |      16 |       0 |        28 |       0 |      4 |        48 |
| `docs/vision.md`                |       6 |       0 |         2 |       0 |      0 |         8 |
| Root and tooling                |       0 |      23 |        53 |       0 |      0 |        76 |
| **Total**                       | **683** | **103** | **1,102** | **309** | **40** | **2,237** |

## 2. Audit method and classification rule

The audit enumerated `git ls-files` and searched decoded text case-insensitively
for either:

- the `owner` stem followed by identifier or hyphen characters, which catches
  `Owner`, `owner_global`, `OwnerCommand`, `OwnershipMismatch`,
  `isCollapsedByOwner`, and their variants; or
- the whole words `own`, `owns`, `owned`, and `owning`.

An occurrence, not a matching line, is the counting unit. Thus a line containing
both `isCollapsedByOwner` and `is_collapsed_by_owner` contributes two. Binary
files, ignored build output, untracked files, and git history are outside the
repository surface being renamed. Possessive or emphatic uses of “own” were
retained in the scan rather than discarded as lexical noise; where they do not
name the platform actor they are category (b) or (c).

Classification precedence was:

1. Any hit in a dated entry in `docs/decisions.md` is (d), regardless of its
   underlying historical meaning.
2. The exact judgment-call ledger in section 6 is (e).
3. Personal repository-owner voice in the recorded process carve-out is (b).
4. Platform agency, product-user scope, commands, provenance, and client-visible
   actor vocabulary are (a).
5. All remaining ownership meanings are (c).

Section 7 is the complete per-file checksum. Its rows sum to the area table and
to 2,237, so no search hit is left unclassified.

## 3. Category (a): renameable platform actor

Category (a) contains **540 code, test, and migration occurrences** and **143
documentation occurrences**. The dominant families are:

- domain agency: `Actor::Owner`;
- session provenance: `OwnerInitiated`, `owner_initiated`, and owner-initiated
  prose;
- command identity and replay: “owner-global”;
- tool decisions and recovery: `OwnerCommand`, `owner_command`,
  `owner_command_id`, `prepare_owner_decision`, `OwnerChoseReconciliation`, and
  owner denial, approval, stop, and recovery prose;
- product and client language: single-owner scope, owner input, owner
  credentials, owner machines, owner-visible operations, and metadata’s owner
  last-writer.

The later pass must not treat this as one blind textual substitution:
`owner_global` becomes `user_global`, but an English “owner decision” becomes a
“user decision”; `OwnerInitiated` becomes `UserInitiated`; and storage and wire
spellings require their own ruled transitions.

### Public API and domain spine

The public domain surface includes `Actor::Owner`,
`SessionCreationCause::OwnerInitiated`,
`ReconciliationReason::OwnerChoseReconciliation`,
`ToolDecisionSource::OwnerCommand`, `StoredToolApprovalEvidence::OwnerCommand`,
`ToolApprovalResolutionReconstitutionInput::owner_command`, and
`ToolBatch::prepare_owner_decision`. Application request/accessor prose and
public orchestration APIs also expose owner-global and owner-attributed
vocabulary.

The domain spine has eight category-(a) occurrences mirroring these names and
must change in the same pull request as the public API. Its ownership-mismatch
types and owning-data comments are category (c) and must remain unchanged.

### Wire protocol and client compatibility

The current versioned process-protocol surface has **33 category-(a)
occurrences** across `crates/process-protocol`, its hub mapping and integration
tests, and the owning specification. The compatibility-bearing identifier is
metadata actor JSON `{"type":"owner"}`, derived from `MetadataActor::Owner`;
expected JSON fixtures pin it. Other hits in the wire crate are Rust names or
protocol prose, including owner input and recovery. `OwnerInitiated` also
crosses the hub’s process-read mapping even though `create_session` does not
carry a cause field.

This is a protocol compatibility change. The process protocol is versioned, so
the later pass needs an explicit ruling on the first version that emits/accepts
`{"type":"user"}` and on whether older versions continue to decode or emit
`owner`. No compatibility behavior is designed here.

The native snapshot has a second, older JSON surface: `is_collapsed_by_owner` /
`isCollapsedByOwner`. Its twelve occurrences are category (e), not silently
grouped with the current process protocol; see section 6.

### Storage encodings and schema names

Persistence has **174 category-(a) occurrences**: 27 in migration SQL, 89 in
store source, and 58 in dedicated integration-test files. Compatibility-bearing
stored vocabulary includes:

- `creation_cause = 'owner_initiated'`;
- `actor_kind` and `result_actor_kind` value `'owner'`;
- `decision_source = 'owner_command'`;
- `owner_command_id` and its foreign-key, CHECK, trigger, query, and corruption
  vocabulary.

These imply a data/schema migration. This inventory flags that fact but does not
select a migration shape, edit an applied migration, or decide whether physical
column and constraint names should change. Technical `owner_fk` and
`owner_identity_key` names for imported-conversation membership are category
(c), not part of that migration.

### Specification and other prose

The living specification has 70 category-(a) occurrences. The largest
concentrations are actor attribution and owner-global replay in
`identity-and-commands.md` and `sessions-and-transcript.md`, process-visible
actor/input language in `process-protocol.md`, and approval provenance in
`tool-loop.md`. The domain spine, vision, target model, architecture, glossary,
invariant catalog, scenarios, and open-question catalog add 64 more; the backlog
adds nine.

The later terminology decision and implementation must update implemented
behavior prose with the code. Mere wording changes do not themselves advance a
spec page’s verified-against reference; any changed behavioral claim must be
reverified under the normal specification rule.

### Tests

Dedicated hub and persistence test files alone carry **71 category-(a)
occurrences** (13 and 58 respectively). The actual test blast radius is larger:
domain, application, process-protocol, and client unit tests are co-located with
production source and account for many of their area totals. Renames reach test
function names, fixture helpers, expected error strings, SQL row fixtures,
serialized JSON, snapshot tables, and assertions over `Actor::Owner`,
`OwnerInitiated`, and `OwnerCommand`.

## 4. Recorded carve-outs

### (b) Personal repository-owner process voice

All 103 category-(b) occurrences remain “owner.” They are confined to the
recorded process surface:

- root/tooling: 23 in `AGENTS.md`, `.coderabbit.yaml`, `.github/CODEOWNERS`,
  `CONTRIBUTING.md`, and `SECURITY.md`;
- `docs/agents`: 80 in `backlog.md` and `goal-mode.md`.

These describe the person who approves, commissions, merges, curates, or reviews
repository work. Technical rule ownership in the same files is instead category
(c), and the nine platform-actor backlog hits listed in section 1 are category
(a).

### (c) Technical ownership

All 1,102 category-(c) occurrences remain ownership vocabulary. They cover:

- Rust ownership, moved/returned owned values, and component-owned ports or
  capabilities;
- aggregate, row, record, session, turn, attempt, frontier, and graph-member
  ownership and ownership-mismatch validation;
- source-of-truth language in which a spec, invariant, scenario, or rule “owns”
  a statement;
- filesystem UID ownership and owner-only/owner-private Unix modes in the local
  socket boundary;
- deployment-, provider-, runtime-, client-, hub-, registry-, and platform-owned
  responsibilities;
- repository coordinates named `owner` in `devenv.lock`; and
- ordinary possessive “own” language that does not name the platform actor.

High-risk false-positive clusters are the 53 local-socket occurrences, 68
imported-conversation graph-owner occurrences, 28 context-frontier owning
session occurrences, imported-conversation `owner_fk` / `owner_identity_key`,
and every `OwnershipMismatch` API. A later mechanical pass should protect these
before replacing any text.

### (d) Historical decision entries

All 309 ownership-family occurrences in `docs/decisions.md` are category (d).
The ledger is append-only: none is edited. The later rename leaves every earlier
actor spelling, personal-owner statement, and technical-ownership statement
intact.

## 5. Ordered mechanical plan for the later pass

This is execution ordering only; the rulings in section 6 come first.

1. Freeze a fresh inventory at the later pass’s base commit. Encode explicit
   protected sets for (b), (c), and (d), including the local-socket,
   graph-owner, ownership-mismatch, and decision-ledger clusters. Resolve every
   (e) item.
2. Add the bottom-of-stack specification diff that describes the behavior the
   stack implements. Set the protocol and storage transition scope from the
   recorded rulings; do not prestate an unimplemented transition on `main`.
3. Rename the domain public vocabulary and regenerate `docs/api/` in the same
   change. Let compiler failures enumerate application, persistence, hub, and
   test call sites.
4. Rename application and composition mappings, then repair co-located unit
   tests, fixture accessors, error prose, and snapshots without touching
   technical ownership.
5. Implement the ruled forward storage transition. Update store reads/writes,
   embedded SQL, schema checks, integration fixtures, and corruption vocabulary;
   do not edit historical decision entries.
6. Implement the ruled process-protocol version transition and hub mapping.
   Update exact JSON expectations and compatibility tests against each supported
   version.
7. Apply the separate ruling for the deferred native snapshot, then update the
   remaining client-facing prose and fixtures.
8. Update the remaining spec, invariant, scenario, glossary, architecture,
   vision, target-model, open-question, and backlog category-(a) prose. Recheck
   that personal repository-owner and technical-ownership language did not move.
9. Repeat the exhaustive search, classify every residual hit, verify public
   API/spine parity and storage/wire inventories, and run the full repository
   validation sequence.

## 6. Judgment calls and required decisions

The 40 category-(e) occurrences form seven decisions. Line numbers refer to the
audited snapshot.

1. **Deferred native snapshot wire name — 12 occurrences.**
   `clients/native/SignalboxNativeTests/SignalboxNativeTests.swift:143,165`,
   `Sources/SignalboxApp/MockSignalboxFixtures.swift:222,230,239,249,258`,
   `Sources/SignalboxModels/SignalboxEvents.swift:318,335` (three occurrences),
   and `Tests/SignalboxModelsTests/SignalboxModelsTests.swift:99,133` carry
   `isCollapsedByOwner` / `is_collapsed_by_owner`. The snapshot’s own README
   says the protocol rewire replaces these layers. **Recommendation:** do not
   mechanically rename a compatibility key in the old snapshot; either remove it
   in the rewire or give it an explicit legacy decoding transition if the rewire
   retains it.
2. **Personal provenance outside a process document — 1 occurrence.**
   `clients/native/README.md:3` says the snapshot came from the owner’s private
   monorepo. **Recommendation:** retain the permitted provenance meaning or
   rephrase it as “repository owner”; never change it to “user.”
3. **Deployment operator versus platform user — 4 occurrences.**
   `docs/spec/configuration-and-credentials.md:267,269` says “owner-client” and
   “owner-held” identity; `docs/spec/conversation-import.md:6,278` says
   “owner-operated” and “owner terminal.” **Recommendation:** use “user” only if
   the accepted single-user model deliberately makes the platform user and
   deployment operator one role; otherwise record and use a distinct operator
   term.
4. **Personal decision voice embedded in normative or API-review documents — 11
   occurrences.** These are `docs/domain-spine.md:3`,
   `docs/spec/identity-and-commands.md:366`,
   `docs/spec/process-protocol.md:575`,
   `docs/spec/sessions-and-transcript.md:573,597,639,643`, and
   `docs/target-model.md:3,265,402,425`. **Recommendation:** preserve the
   personal meaning but remove actor ambiguity with “repository owner” in the
   spine or neutral phrasing such as “the recorded decision”; do not rename
   these to “user.”
5. **Personal decision voice in dated research — 9 occurrences.**
   `docs/research/codex-cli-subscription-protocol.md:21,382` and
   `docs/research/schema-audit-2026-07-24.md:18,25,30,62,72,124,127` refer to
   owner gates, fears, confirmations, or decisions. **Recommendation:** treat
   dated research as point-in-time evidence and retain it, or neutralize the
   voice without changing the finding; never reinterpret it as platform-user
   action.
6. **Personal decision voice in an applied migration — 2 occurrences.**
   `crates/persistence/migrations/202607200001_bounded_user_content.sql:1,3`
   calls the bound “owner-decided” and the owner’s provisional choice.
   **Recommendation:** do not rewrite an applied migration merely for
   terminology. If migration immutability is not the governing policy, rephrase
   only to a dated reference in git history; never use “user-decided.”
7. **Owner-gated open-question wording in production code — 1 occurrence.**
   `crates/model-provider-runtime/src/lib.rs:643` says an open question is
   “owner-gated.” **Recommendation:** replace it with a link-shaped neutral
   phrase such as “the recorded open question remains unresolved”; it is not a
   platform-user gate.

The later plan also needs explicit rulings on two transition mechanics that do
not add category-(e) occurrences: the supported-version behavior for the wire
actor tag, and the forward migration/compatibility treatment for stored
`owner_initiated`, `owner`, `owner_command`, and `owner_command_id` spellings.

## 7. Complete per-file classification checksum

An em dash means zero. This ledger is the exhaustive path-level classification
of the search corpus.

| File                                                                       | (a) | (b) | (c) | (d) | (e) | Total |
| -------------------------------------------------------------------------- | --: | --: | --: | --: | --: | ----: |
| `.coderabbit.yaml`                                                         |   — |   4 |  27 |   — |   — |    31 |
| `.github/CODEOWNERS`                                                       |   — |   1 |   — |   — |   — |     1 |
| `.github/workflows/rust.yml`                                               |   — |   — |   1 |   — |   — |     1 |
| `AGENTS.md`                                                                |   — |  15 |   9 |   — |   — |    24 |
| `CONTRIBUTING.md`                                                          |   — |   2 |   4 |   — |   — |     6 |
| `README.md`                                                                |   — |   — |   3 |   — |   — |     3 |
| `SECURITY.md`                                                              |   — |   1 |   — |   — |   — |     1 |
| `apps/client/src/presentation.rs`                                          |   2 |   — |   — |   — |   — |     2 |
| `apps/hubd/src/configuration.rs`                                           |   — |   — |   2 |   — |   — |     2 |
| `apps/hubd/src/lib.rs`                                                     |   — |   — |   2 |   — |   — |     2 |
| `apps/hubd/src/local_socket.rs`                                            |   — |   — |  53 |   — |   — |    53 |
| `apps/hubd/src/main.rs`                                                    |   — |   — |   1 |   — |   — |     1 |
| `apps/hubd/src/process_runtime.rs`                                         |   3 |   — |   5 |   — |   — |     8 |
| `apps/hubd/tests/offline_tool_loop.rs`                                     |  10 |   — |   — |   — |   — |    10 |
| `apps/hubd/tests/process_protocol_runtime.rs`                              |   3 |   — |   — |   — |   — |     3 |
| `apps/hubd/tests/process_substrate.rs`                                     |   — |   — |   1 |   — |   — |     1 |
| `clients/native/README.md`                                                 |   — |   — |   1 |   — |   1 |     2 |
| `clients/native/SignalboxNativeTests/SignalboxNativeTests.swift`           |   — |   — |   — |   — |   2 |     2 |
| `clients/native/Sources/SignalboxApp/MockSignalboxFixtures.swift`          |   — |   — |   — |   — |   5 |     5 |
| `clients/native/Sources/SignalboxModels/SignalboxEvents.swift`             |   — |   — |   — |   — |   3 |     3 |
| `clients/native/Tests/SignalboxModelsTests/SignalboxModelsTests.swift`     |   — |   — |   — |   — |   2 |     2 |
| `clients/native/docs/tart-vm-validation.md`                                |   — |   — |   2 |   — |   — |     2 |
| `crates/application/src/conversation_import.rs`                            |   — |   — |  12 |   — |   — |    12 |
| `crates/application/src/create_session.rs`                                 |  11 |   — |   4 |   — |   — |    15 |
| `crates/application/src/create_session_from_imported_frontier.rs`          |   4 |   — |   3 |   — |   — |     7 |
| `crates/application/src/load_session.rs`                                   |   1 |   — |   2 |   — |   — |     3 |
| `crates/application/src/model_execution.rs`                                |   8 |   — |  15 |   — |   — |    23 |
| `crates/application/src/operator_failure.rs`                               |   — |   — |   1 |   — |   — |     1 |
| `crates/application/src/replace_session_defaults.rs`                       |   6 |   — |   3 |   — |   — |     9 |
| `crates/application/src/scheduler.rs`                                      |   — |   — |   7 |   — |   — |     7 |
| `crates/application/src/session_metadata.rs`                               |   9 |   — |   7 |   — |   — |    16 |
| `crates/application/src/start_eligible_turn.rs`                            |   1 |   — |   3 |   — |   — |     4 |
| `crates/application/src/startup_scan.rs`                                   |   — |   — |   2 |   — |   — |     2 |
| `crates/application/src/submit_input.rs`                                   |  13 |   — |   2 |   — |   — |    15 |
| `crates/application/src/tool_loop.rs`                                      |   4 |   — |   5 |   — |   — |     9 |
| `crates/application/src/tool_loop_ports.rs`                                |   2 |   — |   1 |   — |   — |     3 |
| `crates/domain/src/accepted_input.rs`                                      |   — |   — |   2 |   — |   — |     2 |
| `crates/domain/src/actor.rs`                                               |   8 |   — |   — |   — |   — |     8 |
| `crates/domain/src/applied_interrupt.rs`                                   |   1 |   — |   2 |   — |   — |     3 |
| `crates/domain/src/configuration.rs`                                       |   1 |   — |   5 |   — |   — |     6 |
| `crates/domain/src/context_frontier.rs`                                    |   — |   — |  28 |   — |   — |    28 |
| `crates/domain/src/delivery_request.rs`                                    |   — |   — |   1 |   — |   — |     1 |
| `crates/domain/src/fatal_mismatch.rs`                                      |   1 |   — |  12 |   — |   — |    13 |
| `crates/domain/src/fatal_mismatch/lifecycle.rs`                            |   — |   — |   9 |   — |   — |     9 |
| `crates/domain/src/fatal_mismatch/prepared.rs`                             |   — |   — |   1 |   — |   — |     1 |
| `crates/domain/src/imported_conversation.rs`                               |   — |   — |  68 |   — |   — |    68 |
| `crates/domain/src/imported_session.rs`                                    |   8 |   — |   9 |   — |   — |    17 |
| `crates/domain/src/lib.rs`                                                 |   1 |   — |   — |   — |   — |     1 |
| `crates/domain/src/model_call.rs`                                          |   — |   — |  10 |   — |   — |    10 |
| `crates/domain/src/model_execution.rs`                                     |   5 |   — |  39 |   — |   — |    44 |
| `crates/domain/src/provider_evidence.rs`                                   |   — |   — |  11 |   — |   — |    11 |
| `crates/domain/src/queue_order.rs`                                         |   — |   — |   6 |   — |   — |     6 |
| `crates/domain/src/replace_session_defaults.rs`                            |  12 |   — |   3 |   — |   — |    15 |
| `crates/domain/src/semantic_entry.rs`                                      |   — |   — |   4 |   — |   — |     4 |
| `crates/domain/src/session.rs`                                             |  53 |   — |  19 |   — |   — |    72 |
| `crates/domain/src/session_metadata.rs`                                    |  15 |   — |   3 |   — |   — |    18 |
| `crates/domain/src/submit_input.rs`                                        |  80 |   — |  16 |   — |   — |    96 |
| `crates/domain/src/tool.rs`                                                |  33 |   — |   5 |   — |   — |    38 |
| `crates/domain/src/tool_attempt.rs`                                        |   — |   — |   7 |   — |   — |     7 |
| `crates/domain/src/tool_execution.rs`                                      |  15 |   — |  10 |   — |   — |    25 |
| `crates/domain/src/turn_attempt.rs`                                        |   — |   — |   1 |   — |   — |     1 |
| `crates/domain/src/turn_eligibility.rs`                                    |  23 |   — |  68 |   — |   — |    91 |
| `crates/domain/src/turn_lifecycle.rs`                                      |  12 |   — |   3 |   — |   — |    15 |
| `crates/expect-table/src/lib.rs`                                           |   — |   — |   2 |   — |   — |     2 |
| `crates/expect-table/src/parse.rs`                                         |   — |   — |   6 |   — |   — |     6 |
| `crates/expect-table/tests/tables.rs`                                      |   — |   — |   6 |   — |   — |     6 |
| `crates/model-provider-runtime/src/lib.rs`                                 |   — |   — |   5 |   — |   1 |     6 |
| `crates/model-runtime-openai/src/stream.rs`                                |   — |   — |   4 |   — |   — |     4 |
| `crates/model-runtime-openai/src/translate.rs`                             |   — |   — |   2 |   — |   — |     2 |
| `crates/model-runtime/src/evidence.rs`                                     |   — |   — |   2 |   — |   — |     2 |
| `crates/model-runtime/src/lib.rs`                                          |   — |   — |   1 |   — |   — |     1 |
| `crates/model-runtime/src/message.rs`                                      |   — |   — |   1 |   — |   — |     1 |
| `crates/model-runtime/src/output.rs`                                       |   — |   — |   3 |   — |   — |     3 |
| `crates/model-runtime/src/runtime.rs`                                      |   — |   — |   1 |   — |   — |     1 |
| `crates/persistence/migrations/202607180001_create_session.sql`            |   3 |   — |   — |   — |   — |     3 |
| `crates/persistence/migrations/202607180002_replace_session_defaults.sql`  |   1 |   — |   — |   — |   — |     1 |
| `crates/persistence/migrations/202607180003_submit_input.sql`              |   2 |   — |   — |   — |   — |     2 |
| `crates/persistence/migrations/202607180004_turn_lifecycle_storage.sql`    |   — |   — |   4 |   — |   — |     4 |
| `crates/persistence/migrations/202607200001_bounded_user_content.sql`      |   — |   — |   — |   — |   2 |     2 |
| `crates/persistence/migrations/202607200002_transactional_outbox.sql`      |   — |   — |   1 |   — |   — |     1 |
| `crates/persistence/migrations/202607220001_model_call_execution.sql`      |   — |   — |   4 |   — |   — |     4 |
| `crates/persistence/migrations/202607220003_failed_terminal_execution.sql` |   — |   — |   2 |   — |   — |     2 |
| `crates/persistence/migrations/202607220004_steering_consumption.sql`      |   — |   — |   2 |   — |   — |     2 |
| `crates/persistence/migrations/202607220005_stop_requests.sql`             |   — |   — |   1 |   — |   — |     1 |
| `crates/persistence/migrations/202607240001_conversation_import.sql`       |   — |   — |   2 |   — |   — |     2 |
| `crates/persistence/migrations/202607240002_imported_session_seed.sql`     |   1 |   — |   2 |   — |   — |     3 |
| `crates/persistence/migrations/202607250001_tool_loop.sql`                 |  14 |   — |   2 |   — |   — |    16 |
| `crates/persistence/migrations/202607260101_session_metadata.sql`          |   6 |   — |   — |   — |   — |     6 |
| `crates/persistence/src/command_registry.rs`                               |   1 |   — |   — |   — |   — |     1 |
| `crates/persistence/src/conversation_import.rs`                            |   — |   — |   1 |   — |   — |     1 |
| `crates/persistence/src/create_session.rs`                                 |   9 |   — |   2 |   — |   — |    11 |
| `crates/persistence/src/create_session_from_imported_frontier.rs`          |   9 |   — |   3 |   — |   — |    12 |
| `crates/persistence/src/lib.rs`                                            |   — |   — |   1 |   — |   — |     1 |
| `crates/persistence/src/mapping.rs`                                        |   — |   — |   1 |   — |   — |     1 |
| `crates/persistence/src/model_execution.rs`                                |   4 |   — |   5 |   — |   — |     9 |
| `crates/persistence/src/outbox.rs`                                         |   1 |   — |   5 |   — |   — |     6 |
| `crates/persistence/src/process_read.rs`                                   |   8 |   — |  18 |   — |   — |    26 |
| `crates/persistence/src/replace_session_defaults.rs`                       |   4 |   — |   2 |   — |   — |     6 |
| `crates/persistence/src/session.rs`                                        |   4 |   — |   2 |   — |   — |     6 |
| `crates/persistence/src/session_metadata.rs`                               |  12 |   — |   1 |   — |   — |    13 |
| `crates/persistence/src/start_eligible_turn.rs`                            |   — |   — |   1 |   — |   — |     1 |
| `crates/persistence/src/startup.rs`                                        |   — |   — |   1 |   — |   — |     1 |
| `crates/persistence/src/submit_input.rs`                                   |   7 |   — |   4 |   — |   — |    11 |
| `crates/persistence/src/tool_loop.rs`                                      |  30 |   — |   5 |   — |   — |    35 |
| `crates/persistence/tests/conversation_import_postgres.rs`                 |   2 |   — |   3 |   — |   — |     5 |
| `crates/persistence/tests/postgres_integration.rs`                         |  40 |   — |   6 |   — |   — |    46 |
| `crates/persistence/tests/session_metadata_postgres.rs`                    |  16 |   — |   1 |   — |   — |    17 |
| `crates/process-protocol/src/lib.rs`                                       |  21 |   — |  11 |   — |   — |    32 |
| `devenv.lock`                                                              |   — |   — |   6 |   — |   — |     6 |
| `docs/agents/backlog.md`                                                   |   9 |  70 |  70 |   — |   — |   149 |
| `docs/agents/goal-mode.md`                                                 |   — |  10 |   3 |   — |   — |    13 |
| `docs/agents/testing-style.md`                                             |   — |   — |   7 |   — |   — |     7 |
| `docs/architecture.md`                                                     |   6 |   — |  12 |   — |   — |    18 |
| `docs/decisions.md`                                                        |   — |   — |   — | 309 |   — |   309 |
| `docs/domain-spine.md`                                                     |   8 |   — |   8 |   — |   1 |    17 |
| `docs/glossary.md`                                                         |   2 |   — |   6 |   — |   — |     8 |
| `docs/invariants.md`                                                       |   6 |   — |   8 |   — |   — |    14 |
| `docs/open-questions.md`                                                   |   7 |   — |  21 |   — |   — |    28 |
| `docs/research/codex-cli-subscription-protocol.md`                         |   — |   — |   5 |   — |   2 |     7 |
| `docs/research/runtime-adapter-conformance.md`                             |   — |   — |  25 |   — |   — |    25 |
| `docs/research/schema-audit-2026-07-24.md`                                 |   — |   — |   2 |   — |   7 |     9 |
| `docs/research/serdes-ai-phase0-audit.md`                                  |   — |   — |   8 |   — |   — |     8 |
| `docs/scenarios.md`                                                        |  13 |   — |  58 |   — |   — |    71 |
| `docs/spec/README.md`                                                      |   3 |   — |  16 |   — |   — |    19 |
| `docs/spec/configuration-and-credentials.md`                               |   — |   — |   8 |   — |   2 |    10 |
| `docs/spec/conversation-import.md`                                         |   1 |   — |  11 |   — |   2 |    14 |
| `docs/spec/identity-and-commands.md`                                       |  19 |   — |  11 |   — |   1 |    31 |
| `docs/spec/model-call-execution.md`                                        |   1 |   — |  16 |   — |   — |    17 |
| `docs/spec/persistence-protocol.md`                                        |   4 |   — |  20 |   — |   — |    24 |
| `docs/spec/process-protocol.md`                                            |   6 |   — |  26 |   — |   1 |    33 |
| `docs/spec/review-workflows.md`                                            |   — |   — |  16 |   — |   — |    16 |
| `docs/spec/runtime-substrate.md`                                           |   — |   — |   7 |   — |   — |     7 |
| `docs/spec/sessions-and-transcript.md`                                     |  17 |   — |  30 |   — |   4 |    51 |
| `docs/spec/tool-loop.md`                                                   |  11 |   — |  12 |   — |   — |    23 |
| `docs/spec/turn-lifecycle-and-scheduling.md`                               |   8 |   — |   7 |   — |   — |    15 |
| `docs/style.md`                                                            |   — |   — |   4 |   — |   — |     4 |
| `docs/target-model.md`                                                     |  16 |   — |  28 |   — |   4 |    48 |
| `docs/vision.md`                                                           |   6 |   — |   2 |   — |   — |     8 |
| `scripts/check_docs_consistency.py`                                        |   — |   — |   2 |   — |   — |     2 |
| `scripts/check_domain_spine.py`                                            |   — |   — |   1 |   — |   — |     1 |
