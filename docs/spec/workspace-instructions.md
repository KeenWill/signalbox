# Workspace instructions and skills

The comparative evidence and foundation-proposal boundary on this page were
verified against PR #796 (`agent/agent-docs-skills-spec`). Bounded filesystem
discovery, typed registration construction, and explicit-root configuration were
verified against PR #798 (`agent/agent-docs-skills-foundation`). Durable
registration, empty eligibility, turn-start evidence, and model-call correlation
were verified against PR #810 (`agent/agent-docs-skills-model-call-followup`).

This page is the foundation proposal at the bottom of the workspace-instruction
implementation stack. It specifies daemon-owned discovery, registration,
eligibility, admission, model-input projection, and per-turn provenance for
agent documentation and Agent Skills found in a session workspace or an
explicitly registered directory. Its first implementing child supplies
discovery, registration, and the durable per-turn record. Later child pull
requests implement the functionality explicitly labeled unimplemented below.

The contract is informed by the comparative evidence collected for issue
[#788](https://github.com/KeenWill/signalbox/issues/788). The
[AGENTS.md convention](https://agents.md/) defines a portable filename and
directory scope but not a complete discovery, budget, or provenance algorithm.
The [Agent Skills specification](https://agentskills.io/specification) defines a
portable bundle and progressive disclosure but leaves discovery locations,
runtime selection, and invalidation to clients. Signalbox consumes those formats
without treating any client's ambient loader as authority.

## Boundary and vocabulary

The daemon solely owns workspace-instruction context assembly. Model-runtime
adapters continue to disable native project-document, rules, user-configuration,
and skill-instruction loaders. A daemon result reaches an adapter only as
explicit prepared model input. This preserves one reproducible path and keeps
host-local files from bypassing session policy.

A **bundle** is one independently addressable instruction source. Version one
admits two kinds:

- `agent_document`, one file named exactly `AGENTS.md`; and
- `agent_skill`, one directory containing a `SKILL.md` whose required `name` and
  `description` satisfy the Agent Skills specification.

Supporting skill files are bundle resources, not independent bundles. Other
vendor filenames and rule formats are not aliases in version one; adding one
requires specifying its parsing, scope, and precedence rather than guessing from
its name.

The pipeline has four independent stages:

1. **Discovery** finds candidates without making them model-visible.
2. **Registration** validates a candidate and assigns typed identity and
   source-content evidence.
3. **Eligibility** determines whether one session or template may use a
   registered bundle.
4. **Admission** deliberately places rendered content in one turn's model-input
   projection.

No stage implies the next. Greedy discovery is not eager injection, registration
grants no session authority, and eligibility spends no model context.

## Discovery

Discovery greedily walks the complete directory tree rooted at the session's
daemon-local resolved workspace and each instruction directory explicitly
registered by the owning
[configuration grammar](configuration-and-credentials.md#workspace-instruction-roots).
The workspace and every configured root are separate `DiscoveryRoot` values; a
configured root is not silently folded into the workspace's authority or
relative-path namespace. A session without a daemon-local resolved workspace
still discovers configured roots.

Version one has exactly two root kinds and canonical lowercase spellings:
`workspace` for the session's daemon-local resolved workspace and `configured`
for an explicitly registered daemon directory. Selectors, wrappers, stored
variants, and canonical digests use only those spellings.

A runner placement owns a different filesystem and is not a daemon-local
resolved workspace. Version one records no workspace `DiscoveryRoot` and does
not claim to scan that workspace; configured daemon roots remain discoverable,
and the durable root inventory proves the omission. Runner discovery requires a
later placement-revision-correlated protocol that returns bytes and findings
from the pinned runner workspace. It may not be emulated by asking a
model-runtime adapter to load ambient files.

The walk considers every nested directory and yields an agent-document candidate
for each `AGENTS.md` regular file. In the workspace it yields an agent-skill
candidate for each directory immediately below an `.agents/skills` directory
that contains a regular `SKILL.md`; `.agents/skills` may occur at any depth.
Within an explicitly registered root, every nested directory containing a
regular `SKILL.md` is a candidate, including the root itself. This lets a
configured root name either one bundle or a collection without requiring the
workspace convention outside the workspace. Version one does not follow symbolic
links. It sorts directory entries by raw path spelling before descending, so
identical trees yield candidates in identical order. Entries that cannot be read
or classified produce typed discovery findings; they do not disappear as an
empty successful result.

The greedy walk is complete only within fixed daemon safety limits. Version one
admits at most 100,000 classified directory entries, 4,096 findings, 64 MiB of
candidate source bytes, and 30 seconds of elapsed scan time across all roots.
These limits are not user-configurable discovery policy. The scan records the
limit-set version, every consumed count, and a typed `limit_reached` finding
naming the first exhausted dimension; it then stops without presenting the
partial inventory as complete. Registration may retain the candidates already
found, but session creation or turn preparation that requires a complete scan
fails closed. Product ignore rules and configurable depth policy remain
deferred. The 4,096-finding bound reserves its final slot for this terminal
limit finding.

One scan emits a canonical source path only once even when workspace and
configured roots overlap; the first read fixes its source hash for that scan.
Workspace roots sort before configured roots and each kind sorts by canonical
path. The first root whose kind-specific discovery rules actually yielded the
candidate is its primary authorizing root; mere path containment grants no
candidate authority. The complete ordered root inventory preserves every other
root that also yielded that source. Registration therefore assigns one identity,
and admission cannot render the same source twice through root aliases or
observe two versions from a mid-scan edit.

The greedy walk intentionally exceeds terminal clients' common
root-to-working-directory behavior: a daemon owns the workspace and must make
sibling-package instructions discoverable before a rollout. Finding content
still confers neither trust nor context authority, so a newly added bundle
cannot affect a session merely by appearing on disk.

Discovery is a snapshot operation. Each scan records its roots and findings;
watching, automatic rescans, ignore semantics, configurable depth bounds, and
symlink traversal are deferred. A later scan may register different evidence but
never rewrites an earlier turn manifest.

## Registration and identity

Registration validates each candidate into a `RegisteredInstructionBundle`. Its
identity is a distinct `InstructionBundleId`, not a display name, path, ordinal,
or content hash. It remains stable for that registered record even when another
bundle has the same skill name or bytes. Name collisions are inventory, not an
implicit winner.

Every registered bundle carries:

- bundle identity and closed kind;
- canonical absolute source path and the `DiscoveryRoot` authorizing the read;
- root-relative source path and, for an agent document, its root-relative
  directory scope;
- for a skill, its validated portable name and description;
- source byte length; and
- a versioned SHA-256 source-content hash.

For an agent document, source content is the file's exact bytes. For a skill,
version-one source content is the exact `SKILL.md` bytes. Supporting resources
are neither enumerated nor registered until a later contract defines their
relative identities, traversal, ordering, and hashes. Registration rejects a
non-UTF-8 source, invalid frontmatter, a skill name that differs from its parent
directory, or a path escaping its root. A candidate whose canonical or
root-relative source path is not UTF-8 produces a typed `non_utf8_source_path`
discovery finding and never reaches registration. Rejection is a typed finding
and creates no partial bundle.

SHA-256 is named in the representation rather than assumed. The MCP skill
transfer proposal
[SEP-2640](https://github.com/modelcontextprotocol/modelcontextprotocol/issues/2640)
provides public precedent for digest verification across an instruction-content
boundary. Signalbox applies that discipline to repository content because a
checkout is input, not daemon authority.

Registration hashes source bytes; admission separately hashes rendered bytes.
They answer different questions: what was available and what the model saw.
Re-reading a changed path creates new registration evidence and never mutates an
earlier record.

## Eligibility

Eligibility is an allow-list bound to a session template and copied into a
session at creation, with optional later session-specific replacement owned by
its own durable command. Two sessions using one checkout may therefore differ
without editing shared files. An absent allow-list means no bundle is eligible,
never every discovered bundle.

A session allow-list names registered bundle identities. A template predates the
workspace registrations, so it instead names exact `TemplateInstructionSelector`
values: a root reference, root-relative source path, bundle kind, and expected
source hash. The root reference is exactly `workspace`, or `configured` plus the
configured root's stable `ConfiguredInstructionRootId` defined by the
[configuration contract](configuration-and-credentials.md#workspace-instruction-roots).
The checked session-creation operation scans and registers its daemon-local
workspace, resolves each selector to exactly one identity, and then copies those
identities. A missing, stale, or ambiguous selector grants nothing and is
recorded as a typed resolution finding; it never degrades to a path glob or
newest-content match. Runner-workspace selectors remain unresolved until the
runner discovery protocol exists.

The effective eligibility snapshot is immutable for one turn. A turn records a
versioned SHA-256 hash of the canonical ordered bundle-identity list effective
at start. Replacement affects only later turns. A registered bundle absent from
that snapshot cannot be enumerated, previewed, or admitted.

Until whole-bundle unload is implemented, a replacement command rejects removal
of any currently admitted identity. This makes replacement neither an implicit
unload nor an authority revocation that effective input ignores. Additions and
removal of never-admitted identities can still take effect at the next turn
boundary; the later unload transition owns removal from both sets.

**Committed unimplemented functionality — eligibility control.** The first
implementation slice records the empty snapshot and exposes no replacement
command. Template authoring, session replacement, and visibility variants such
as metadata-only or user-only have no present template field, process request,
or session command. Their implementation must preserve the allow-list default
and frozen turn snapshot.

## Enumeration, preview, and admission

Eligible inventory is progressively disclosed. `instructions.list` returns a
bounded cursor-paginated catalog with bundle identity, kind, display name,
description when present, source byte length, source hash, authorizing root, and
root-relative source and scope paths. For `AGENTS.md`, an empty scope means the
root and a nested scope applies only to that directory and descendants. Catalog
order is root, then increasing scope depth, then raw relative path. An ancestor
document precedes a descendant document, so a deliberately admitted descendant
is the later, more specific instruction; sibling scopes never apply to each
other. `instructions.preview` returns bounded structure — headings for a
document and validated metadata plus headings for a skill — with full source
byte length and estimated model-token cost. Preview re-reads at most that one
registered source under its authorizing root, revalidates the registered source
hash, and returns typed stale-source evidence if the bytes changed or
disappeared. It neither returns the full body nor admits content.

`instructions.read` names one eligible bundle and requests deliberate admission.
The daemon re-reads and validates it under its registered root, compares the
source hash with registration evidence, applies the per-bundle render budget,
and returns only a typed admission receipt containing identity, source hash,
rendered hash, byte length, truncation evidence, and durable admission identity.
The rendered instruction body is never tool-result content and therefore never
enters semantic tool-result history. A changed or missing source fails with
stale-source evidence rather than admitting unregistered bytes. Skill-resource
reads require a later resource-address and hash contract.

Admission is idempotent by bundle within the effective admitted set. A distinct
request for an already admitted bundle returns an `already_admitted` receipt
naming the existing admission and exact rendered evidence, records that
request's replay link, and appends no second `InstructionAdmission`. It does not
re-read a moving source. A replay of either request returns its recorded
receipt, so one manifest can never contain duplicate bundle identities.

List reads only registration metadata; preview performs the single-source
revalidated read above. Their bounds are independent of aggregate registered
content. Catalog budgeting shortens descriptions before omitting identities, and
every shortening or omission is explicit. This retains the Agent Skills
progressive-disclosure economics without unrecorded selection or budget
outcomes.

Nothing is admitted merely because it is eligible, heuristically relevant, near
a touched file, or present in a template. Version one admits only by the closed
`model_requested` route. Path-triggered or template-eager routes need separate
variants and triggering evidence.

**Committed unimplemented functionality — model-facing operations.** No present
tool supplies list, preview, or read unless an implementing child explicitly
advances this section's verified reference. Unloading is not implemented in the
first slice.

## Durable admission transition

Each `instructions.read` request has a replay-stable tool-request identity. In
the same transaction as its receipt-only tool result, a successful request
appends one `InstructionAdmission` naming the prior admitted-set hash, bundle,
rendered evidence, exact rendered wrapper bytes, and request identity. The
immutable admission row is the version-one plaintext authority for later
projections even if the workspace source changes or disappears. Replaying that
identity returns the same receipt; a conflicting replay is corruption. A failed
request appends no admission and does not change the set.

After a tool batch, preparation folds successful admissions in durable request
order and ignores idempotent repeats. The owning continuation transaction in
[tool-loop](tool-loop.md#result-authority-and-the-continuation-boundary) creates
exactly one successor manifest with the next model call. Thus several reads in
one batch aggregate into one boundary, and a crash after tool-result commit but
before continuation leaves admissions that the next preparation
deterministically folds. Process memory and the live workspace are never
authority for the admitted set.

## Projection rather than transcript append

Admitted instructions are a model-input **projection rebuilt each turn**, not
semantic transcript entries. The daemon holds the declared admitted set,
rebuilds its region from the immutable rendered bytes retained by each
admission, places it beside the context-frontier projection for a call, and
records the exact result in the manifest below. Instruction text never advances
a `ContextFrontier`, changes ancestry, or becomes user-role conversation.

Prepared model input contains exactly one typed `WorkspaceInstructionRegion`,
after the frozen daemon/session system prompt and before the ordered sequence of
actor-authored and tool-result frontier messages. Adapters serialize that region
as instruction/system input supported by their provider; they may not
reinterpret it as a user or tool message or invoke a native file loader. The
frozen session system prompt and explicit user request remain higher priority
than this repository-supplied region. When a provider exposes only a
system-instruction transport, the daemon wrapper states that subordinate
authority before the repository bytes rather than pretending they are daemon
policy.

The region orders admitted agent documents by authorizing root, increasing scope
depth, and relative path, then skills by bundle identity bytes. Each bundle is
wrapped as UTF-8 bytes. `root` is the closed authorizing-root kind and `source`
is its UTF-8 root-relative path; canonical absolute paths remain daemon-side
manifest provenance and never enter provider input.

```text
<signalbox_workspace_instruction>
{"bundle_id":"<lowercase UUID>","kind":"<closed kind>","root":"<closed root kind>","source":"<JSON-escaped root-relative path>","source_sha256":"<lowercase hex>"}
<content>
<XML-escaped budgeted source bytes>
</content>
</signalbox_workspace_instruction>
```

JSON string escaping follows RFC 8259, line endings shown above are LF, and
there is no implicit leading or trailing byte. After source-budget truncation,
content escaping replaces `&`, `<`, and `>` with `&amp;`, `&lt;`, and `&gt;` in
that order; repository bytes therefore cannot terminate or fabricate an
envelope. The rendered-content hash covers this complete escaped wrapper. These
labels distinguish untrusted repository text from daemon authority and make
adapter output byte-stable without disclosing host filesystem layout.

Two designs were considered:

- **Append to transcript.** Admission preserves a stable provider prompt-cache
  prefix and naturally survives later turns. But content cannot be removed
  without rewriting or abandoning the frontier, file edits leave stale text in
  history, and provenance must be reconstructed from past events.
- **Rebuild a projection.** The live set is explicit and can later support
  whole-bundle unload at a turn boundary. Each turn directly records what was
  rendered. It costs cache invalidation when the set or rendered bytes change,
  plus durable retained-byte storage and input work during preparation.

Signalbox chooses projection. The decisive invariant is that a context frontier
is immutable semantic conversation history (INV-015); instruction policy is
effective input configuration, not conversation authored by an actor. Append
would make unloading incompatible with that immutability. Projection lets an
append-only audit event change later effective input without altering an earlier
frontier or manifest. The comparative evidence reaches the same boundary:
[Aider's read-only-file commands](https://aider.chat/docs/usage/commands.html)
offer selective `/drop`, while
[Continue's rules](https://docs.continue.dev/customize/deep-dives/rules) are
selected again for each request; clients that append instruction messages offer
no equivalent selective removal. The daemon preserves cache stability by
rendering byte-identical instruction regions while the set and bytes are
unchanged and applying changes only at turn boundaries.

This reserves but does not implement unloading. A later foundation slice must
define unload authority, tombstone visibility, and admitted-set transition. Only
whole bundles may leave later projections, and unloading must add durable
history rather than delete an admission or manifest.

## Canonical digest bytes

All stored digests are the 32 raw SHA-256 bytes and display as lowercase
64-character hexadecimal. Version-one domain separators are literal UTF-8 with
no terminator. UUIDs are their 16 RFC 4122 network-order bytes; unsigned counts
and lengths are eight-byte big-endian values; text is an eight-byte byte length
followed by exact UTF-8 bytes; closed variants are the length-framed lowercase
names written on this page.

The eligibility hash is SHA-256 over `signalbox-instruction-eligibility-v1`
followed by eligible bundle UUID bytes in ascending UUID-byte order. The empty
hash is therefore the separator alone.

The admitted-set hash is SHA-256 over `signalbox-instruction-admitted-set-v1`,
an unsigned record count, then one record per effective admission in projection
order. Each record is bundle UUID, admission UUID, and the 32-byte
rendered-content hash. The empty-set vector is the separator followed by an
all-zero eight-byte count. Including admission identity distinguishes two
budgeted renderings of one registered bundle.

The manifest hash begins with `signalbox-turn-instruction-manifest-v1`, then
session UUID, turn UUID, the 32-byte eligibility hash, and its boundary: literal
`turn_start`, or literal `model_call` plus model-call UUID. Rendered bundle
records follow in projection order. Each is bundle UUID, length-framed kind,
length-framed canonical source path, length-framed authorizing-root kind,
length-framed root-relative source label, 32-byte source hash, 32-byte rendered
hash, rendered byte length, length-framed admission route, then one byte `0` for
no truncation or byte `1` plus the truncation boundary as an unsigned length.
Fixed-width identities and digests plus length framing make the representation
uniquely decodable. The empty turn-start vector ends immediately after literal
`turn_start`.

## Budgets and rendered content

Every admission has an explicit per-bundle source-byte budget. Rendering
preserves UTF-8 and emits the complete source or truncates at a character
boundary no later than the budget before applying the required content escaping
and wrapper. It never borrows a shared pool whose earlier entries can starve
later ones. Rendered byte length, source truncation boundary, and retained exact
wrapper bytes are evidence.

Version one has a fixed 65,536-byte aggregate workspace-instruction-region
budget, including every wrapper. A provider model with a smaller instruction
capacity is not eligible for this capability. A successful read serializes on
the admitted-set head and preflights the current region plus its candidate
against that aggregate budget before committing its receipt or admission.
Concurrent and same-batch reads therefore observe one ordered predecessor and
cannot commit a set whose instruction region alone is unrenderable. Aggregate
exhaustion is a typed failed read and changes no durable admitted set.

The durable rendered-content hash is SHA-256 over the exact bytes placed in
prepared model input **after** wrappers, labels, and budget truncation. It is
not the source-file hash. A manifest thus describes what the model saw when
source and rendered bytes differ, and wrapper or budget changes remain visible.

Overflow never silently drops an admitted bundle. If no nonempty valid rendering
fits, preparation fails before provider spawn. Context pressure does not
implicitly unload, summarize, or evict instructions.

## Durable per-turn instruction manifest

Every turn owns an append-only sequence of immutable `TurnInstructionManifest`
values, beginning with exactly one turn-start manifest even when the eligibility
and admission sets are empty. The initial manifest is fixed before the first
provider call and authenticated whenever that call is prepared or reconstituted.
A model-requested admission during a tool round appends admission evidence and
the next preparation atomically produces a successor manifest with its model
call; earlier call-boundary manifests remain addressable. The first
implementation slice has no admission and stores only the turn-start manifest
(INV-061).

Each manifest records:

- session and turn identities and the eligibility-set hash;
- its call boundary, or `turn_start` before a call-specific successor;
- for each rendered bundle in projection order, bundle identity, kind, canonical
  source path, provider-visible root kind and root-relative source label,
  registered source hash, rendered hash, rendered byte length, admission route,
  and optional truncation boundary; and
- a versioned hash of the canonical manifest representation.

The required audit tuple is source path, typed identity, and rendered hash; the
source hash diagnoses registration-to-admission changes. Comparing manifests
does not require the live workspace. The manifest proves exact equality and
provenance; the immutable admission row retains the rendered plaintext required
to reconstruct later projections.

Persistence is append-only. Constraints require every bundle row to name its
manifest and registered bundle, prohibit duplicate bundle identities in one
manifest, and prevent update or deletion. A prepared call cannot name a turn
without the exact manifest used for its instruction projection. Reconstitution
rejects a missing or mismatched manifest, a bundle identity/path/hash that
disagrees with registration, or rendered evidence violating its budget as typed
storage corruption.

Why: Claude Code's public
[instruction-loading hook](https://docs.anthropic.com/en/docs/claude-code/hooks)
records source provenance without a content hash, while transcript-oriented
clients retain content without a typed source address. Neither alone answers
which bytes a historical turn used; the manifest joins both at the model-call
boundary.

## Security and failure posture

Workspace instructions are repository-supplied untrusted input. Their text has
no authority to widen tools, reveal credentials, change sandbox placement,
modify eligibility, or bypass higher-priority system or user instructions. Skill
`allowed-tools` metadata is source metadata only and grants no Signalbox
permission in version one.

Filesystem errors, invalid metadata, root escapes, stale hashes, and budget
failures remain typed and visible. The daemon never falls back to an adapter's
ambient loader or substitutes a same-named bundle. A pre-spawn failure records
no claim that content was rendered; durable preparation retains its manifest for
recovery.

## Open edges

- Eligibility and model-facing operations remain committed unimplemented
  functionality in their owning sections above.
- Whole-bundle unload is reserved by the projection decision but deferred.
- Resource reads, file watching, rescans, ignore rules, symlink traversal,
  further vendor formats, search/ranking, eager and path-triggered admission,
  and later externalization of retained rendered plaintext are undecided and
  tracked in [open questions](../open-questions.md), never inferred from this
  baseline.
