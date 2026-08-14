# Workspace instructions and skills

The comparative evidence and foundation-proposal boundary on this page were
verified against PR #796 (`agent/agent-docs-skills-spec`). Its
implemented-behavior statements become verified with the first implementing
child.

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
resolved workspace and each instruction directory explicitly registered by
daemon configuration. The workspace and every configured root are separate
`DiscoveryRoot` values; a configured root is not silently folded into the
workspace's authority or relative-path namespace. A session without a resolved
workspace still discovers configured roots.

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
- for a skill, its validated portable name and description;
- source byte length; and
- a versioned SHA-256 source-content hash.

For an agent document, source content is the file's exact bytes. For a skill,
version-one source content is the exact `SKILL.md` bytes; resources are
enumerated but do not enter that hash until a later contract admits them.
Registration rejects a non-UTF-8 source, invalid frontmatter, a skill name that
differs from its parent directory, or a path escaping its root. Rejection is a
typed finding and creates no partial bundle.

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
never every discovered bundle. Lists name bundle identities, not display names
or globs, so a new file cannot silently widen authority.

The effective eligibility snapshot is immutable for one turn. A turn records a
versioned SHA-256 hash of the canonical ordered bundle-identity list effective
at start. Replacement affects only later turns. A registered bundle absent from
that snapshot cannot be enumerated, previewed, or admitted.

**Committed unimplemented functionality — eligibility control.** The first
implementation slice records the empty snapshot and exposes no replacement
command. Template authoring, session replacement, and visibility variants such
as metadata-only or user-only have no present template field, process request,
or session command. Their implementation must preserve the allow-list default
and frozen turn snapshot.

## Enumeration, preview, and admission

Eligible inventory is progressively disclosed. `instructions.list` returns a
bounded cursor-paginated catalog with bundle identity, kind, display name,
description when present, source byte length, and source hash.
`instructions.preview` returns bounded structure — headings for a document and
validated metadata plus headings for a skill — with full source byte length and
estimated model-token cost. Neither returns the full body or admits content.

`instructions.read` names one eligible bundle and requests deliberate admission.
The daemon re-reads and validates it under its registered root, compares the
source hash with registration evidence, applies the per-bundle render budget,
and returns rendered instructions as a typed tool result. A changed or missing
source fails with stale-source evidence rather than admitting unregistered
bytes. Skill-resource reads require a later resource-address and hash contract.

List and preview read registration metadata rather than bundle bodies. Their
bounds are independent of aggregate registered content. Catalog budgeting
shortens descriptions before omitting identities, and every shortening or
omission is explicit. This retains the Agent Skills progressive-disclosure
economics without unrecorded selection or budget outcomes.

Nothing is admitted merely because it is eligible, heuristically relevant, near
a touched file, or present in a template. Version one admits only by the closed
`model_requested` route. Path-triggered or template-eager routes need separate
variants and triggering evidence.

**Committed unimplemented functionality — model-facing operations.** No present
tool supplies list, preview, or read unless an implementing child explicitly
advances this section's verified reference. Unloading is not implemented in the
first slice.

## Projection rather than transcript append

Admitted instructions are a model-input **projection rebuilt each turn**, not
semantic transcript entries. The daemon holds the declared admitted set, renders
it beside the immutable context-frontier projection for a call, and records the
exact result in the manifest below. Instruction text never advances a
`ContextFrontier`, changes ancestry, or becomes user-role conversation.

Two designs were considered:

- **Append to transcript.** Admission preserves a stable provider prompt-cache
  prefix and naturally survives later turns. But content cannot be removed
  without rewriting or abandoning the frontier, file edits leave stale text in
  history, and provenance must be reconstructed from past events.
- **Rebuild a projection.** The live set is explicit and can later support
  whole-bundle unload at a turn boundary. Each turn directly records what was
  rendered. It costs cache invalidation when the set or rendered bytes change,
  plus re-read or retained-byte work during preparation.

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

## Budgets and rendered content

Every admission has an explicit per-bundle byte budget. Rendering preserves
UTF-8 and emits the complete source or truncates at a character boundary no
later than the budget. It never borrows a shared pool whose earlier entries can
starve later ones. Rendered byte length and truncation boundary are evidence.

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
produces a successor manifest for the next call boundary; earlier call-boundary
manifests remain addressable. The first implementation slice has no admission
and stores only the turn-start manifest.

Each manifest records:

- session and turn identities and the eligibility-set hash;
- its call boundary, or `turn_start` before a call-specific successor;
- for each rendered bundle in identity order, bundle identity, kind, canonical
  source path, registered source hash, rendered hash, rendered byte length,
  admission route, and optional truncation boundary; and
- a versioned hash of the canonical manifest representation.

The required audit tuple is source path, typed identity, and rendered hash; the
source hash diagnoses registration-to-admission changes. Comparing manifests
does not require the live workspace. Rendered plaintext may later move to blob
storage; until then the manifest proves exact equality and provenance but does
not claim offline plaintext reconstruction.

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
  and offline rendered-plaintext storage are undecided and tracked in
  [open questions](../open-questions.md), never inferred from this baseline.
