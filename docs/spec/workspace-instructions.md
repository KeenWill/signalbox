# Workspace instructions and skills

The daemon discovers and registers the agent documents and Agent Skills a
workspace or a registered directory supplies, and records for every turn which
of them a model call could use.

## Overview

A workspace instruction is an agent document or an agent skill that a repository
or an operator supplies to guide a model. An agent document is a file named
`AGENTS.md`; an agent skill is a directory holding a `SKILL.md`. The daemon, not
a model-runtime adapter, finds them, records them, and proves per turn what a
model could see. Model-runtime adapters run with their native document, rules,
and skill loaders disabled ([runtime-substrate.md](runtime-substrate.md)).

There are four stages. Discovery finds candidates, and registration validates a
candidate and gives it a typed identity and a source hash; both are built.
Planned eligibility decides which registered bundles one session may use, and
planned admission places a bundle's rendered text in one turn's model input;
both exist only as the empty per-turn record described below.

A bundle is one independently addressable instruction source. Discovery
(`discover_workspace_instructions` in
`crates/application/src/workspace_instructions.rs`) walks every directory under
two kinds of root: the session's daemon-local resolved workspace, and each
instruction directory that configuration registers
([configuration-and-credentials.md](configuration-and-credentials.md)). Under
both kinds of root it skips version-control metadata and build or dependency
outputs, under the workspace root it also skips nested repositories, and it
stops at fixed safety limits. One scan produces a discovery snapshot: the roots
it walked, the candidates it found, a typed finding for every entry it could not
read or classify, and whether the scan was complete.

Registration turns each candidate into an `InstructionBundleRegistration`
(`crates/domain/src/workspace_instruction.rs`). Its identity is a distinct
`InstructionBundleId`, not a display name, path, ordinal, or content hash. Its
source content is the exact bytes of the agent document or the skill's
`SKILL.md`, and a versioned SHA-256 over those bytes records what was available
to the session. A skill's frontmatter is parsed for its name and description.

The per-turn record is the `TurnInstructionManifest`. A turn that reaches
preparation owns one turn-start manifest, the only manifest the daemon stores;
it names the turn's discovery and carries the hashes of the turn's eligibility
set and admitted set, both empty. Two paths record it
([turn-lifecycle-and-scheduling.md](turn-lifecycle-and-scheduling.md)). On the
ordinary path the daemon scans after activation and records the manifest in a
transaction of its own while the turn is still active; a turn that stops being
active first gets no manifest and no model work. The other path prepares the
turn's initial model call inside the activation transaction; there the daemon
scans before activation, and the activation transaction records the manifest
with the activation. Discovery snapshots, registered bundles, and manifests live
in the tables that `crates/persistence/migrations/202609010007_workspaces.sql`
creates; `apps/signalboxd/src/workspace_instruction_runtime.rs` runs discovery
on both paths.

## Design decisions

Signalbox reads the `AGENTS.md` and Agent Skills formats itself. Why: one
reproducible path from file to model input, which host-local files cannot
bypass.

A runner-placed session records no workspace discovery root; configured roots
are still discovered. Why: the runner owns a different filesystem, and the
durable root inventory proves the omission instead of hiding it.

Registration verifies repository content by digest because a checkout is input,
not daemon authority.

Skill frontmatter is YAML deserialized into a closed struct: unknown keys are
rejected, a metadata mapping is accepted and discarded, mixed line endings are
accepted, bounds are counted in characters, and every failure is one
invalid-skill finding.

Discovery does not follow symbolic links.

The discovery safety limits are fixed by the daemon and not user-configurable.

## Boundary contracts

The daemon alone assembles workspace-instruction context. It never falls back to
an adapter's ambient loader and never substitutes a same-named bundle for the
one a session was authorized to use.

No stage implies the next. Discovery is not injection, registration grants no
session authority, and eligibility spends no model context. Finding content
confers neither trust nor context authority, so a bundle cannot affect a session
merely by appearing on disk.

The workspace and every configured root are separate discovery roots. A
configured root is not folded into the workspace's authority or its
relative-path namespace.

In the workspace, a skill candidate is each directory immediately below an
`.agents/skills` directory that holds a regular `SKILL.md`; `.agents/skills` may
occur at any depth. In a configured root, every nested directory holding a
regular `SKILL.md` is a candidate, including the root itself.

The walk is complete only within the daemon's fixed limits on classified
entries, findings, candidate source bytes, and elapsed time. An incomplete scan
is never presented as a complete inventory, and no turn-start manifest names
one.

Workspace roots sort before configured roots, and within each kind roots sort by
canonical path. The first root whose kind-specific rules yield a candidate is
its primary authorizing root; path containment alone grants no authority.

A registered identity stays stable even when another bundle has the same skill
name or the same bytes. Name collisions are recorded, not resolved to an
implicit winner.

Skill `allowed-tools` metadata is source metadata only and grants no Signalbox
permission.

Digest preimages frame values uniformly: literal UTF-8 separators with no
terminator, 16-byte UUIDs, eight-byte big-endian counts and lengths,
length-prefixed UTF-8 text, and length-framed variant names. The turn-start
manifest's boundary name is the exception: it is appended as the literal UTF-8
bytes `turn_start` with no length frame. A trigger in the migration checks the
turn-start manifest's hashes against these preimages.

The turn-start manifest is fixed before the first provider call and
authenticated whenever that call is prepared or reconstituted. Both recording
paths serialize on the session scheduler lock
([persistence-protocol.md](persistence-protocol.md)), and no present command can
change the empty eligibility or admitted sets.

A turn-start manifest names a complete discovery for its own session and turn,
and a prepared call cannot name a turn without the exact manifest used for its
instruction projection.

Comparing two manifests does not require the live workspace.

Reconstitution rejects a missing or mismatched manifest as typed storage
corruption ([persistence-protocol.md](persistence-protocol.md)).

## Planned

- Adapter delivery: a daemon result reaches a model-runtime adapter only as
  explicit prepared model input; no present operation carries instruction input
  ([design](../design/workspace-instructions.md)).
- Skill resources: supporting files in a skill directory are bundle resources,
  not bundles, and are neither enumerated nor registered; other vendor filenames
  and rule formats are not aliases
  ([design](../design/workspace-instructions.md)).
- Runner-workspace discovery: a placement-revision-correlated protocol returns
  bytes and findings from the runner's workspace, and no adapter is asked to
  load ambient files in its place
  ([design](../design/workspace-instructions.md)).
- Eligibility control: a session template supplies instruction selectors that
  resolve at the session's first activation into an allow-list, replaceable
  later by its own durable command; the present implementation exposes no
  replacement command, template field, or visibility variant
  ([design](../design/workspace-instructions.md)).
- Allow-list default: an absent allow-list means no bundle is eligible, never
  every discovered bundle ([design](../design/workspace-instructions.md)).
- Frozen eligibility snapshot: activation copies the exact ordered eligibility
  list under the session lock and records its versioned SHA-256 hash; the
  snapshot is immutable for the turn, and a registered bundle absent from it
  cannot be enumerated, previewed, or admitted
  ([design](../design/workspace-instructions.md)).
- Model-facing tools: `instructions_list`, `instructions_preview`, and
  `instructions_read`; no present tool or registry entry supplies them or their
  permission defaults, postures, schemas, or crash classifications
  ([design](../design/workspace-instructions.md)).
- Admission route: nothing is admitted because it is eligible, relevant, near a
  touched file, or named in a template; only a model request through
  `instructions_read` admits ([design](../design/workspace-instructions.md)).
- Admission approval: `instructions_read` declares the AlwaysConfirm permission
  default with an explicit Delegated posture, so the approval judge decides an
  admission; the request stays parked and admits a user decision when the judge
  escalates or the judge call ends in a terminal failure, except that in a
  repository-watch session that no accepted steering or operator resumption
  attends an escalation closes the batch, fails the turn, and blocks the goal
  ([design](../design/workspace-instructions.md)).
- Untrusted-data region: repository-controlled strings in a catalog or preview
  result are emitted inside a delimited region under a fixed daemon-authored
  label, escaped so they cannot close it
  ([design](../design/workspace-instructions.md)).
- Trusted envelope: result members that cannot carry prose stay outside the
  untrusted region, so a reader can address and order a page without parsing
  untrusted text ([design](../design/workspace-instructions.md)).
- Label fidelity: the untrusted-data label is part of the result the model-input
  contract preserves; an adapter that cannot carry it fails rather than
  presenting the fragments bare ([design](../design/workspace-instructions.md)).
- Durable admission: an immutable admission row is the plaintext authority for
  later projections even if the workspace source changes or disappears; no
  present persistence surface stores an admitted-set head or an admission
  ([design](../design/workspace-instructions.md)).
- Admitted-set authority: process memory and the live workspace are never
  authority for the admitted set
  ([design](../design/workspace-instructions.md)).
- Projection: admitted instructions are a model-input region rebuilt each turn
  from immutable rendered bytes, not transcript entries, because a context
  frontier is immutable conversation history and instruction policy is input
  configuration; no present runtime surface carries the region
  ([design](../design/workspace-instructions.md)).
- Frontier isolation: instruction text never advances a ContextFrontier, changes
  ancestry, or becomes user-role conversation
  ([design](../design/workspace-instructions.md)).
- Priority: the frozen session system prompt and the explicit user request
  outrank the repository-supplied region
  ([design](../design/workspace-instructions.md)).
- Preamble fidelity: no adapter writes its own preamble or drops, reorders, or
  rephrases the daemon's; an adapter that cannot deliver it intact fails before
  send ([design](../design/workspace-instructions.md)).
- Unload: whole-bundle unload is reserved; when built it defines unload
  authority, tombstone visibility, and the admitted-set transition, removes only
  whole bundles, and adds durable history rather than deleting an admission or
  manifest ([design](../design/workspace-instructions.md)).
- Render budgets: a fixed 32,768-byte per-bundle source budget with no
  caller-supplied field, and a fixed 65,536-byte aggregate budget measured over
  the region's exact serialized bytes; no present path renders a bundle
  ([design](../design/workspace-instructions.md)).
- Target capability: the model catalog declares typed-region transport and byte
  capacity for every selectable and serving target, a target with a smaller
  capacity or no typed transport is not eligible for this capability, and no
  token-window conversion or adapter inference supplies the value
  ([design](../design/workspace-instructions.md)).
- Overflow: no admitted bundle is silently dropped; if no nonempty valid
  rendering fits, preparation fails before provider spawn
  ([design](../design/workspace-instructions.md)).
- Context pressure never implicitly unloads, summarizes, or evicts instructions
  ([design](../design/workspace-instructions.md)).
- Successor manifests: a model-requested admission during a tool round appends
  admission evidence, and the next preparation atomically produces a successor
  manifest while earlier call-boundary manifests stay addressable
  ([design](../design/workspace-instructions.md)).
- Bundle reconstitution: a recorded bundle identity, path, or hash that
  disagrees with registration is typed storage corruption
  ([design](../design/workspace-instructions.md)).
- Rendered hash: preparation evidence, never delivery evidence; a call that
  fails before provider spawn or send leaves it behind although the model saw
  nothing ([design](../design/workspace-instructions.md)).
- Instruction authority: workspace instructions are repository-supplied
  untrusted input whose text cannot widen tools, reveal credentials, change
  sandbox placement, modify eligibility, or bypass system or user instructions
  ([design](../design/workspace-instructions.md)).
