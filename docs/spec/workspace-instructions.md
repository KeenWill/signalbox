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
- `agent_skill`, one directory containing a `SKILL.md` whose frontmatter
  satisfies the closed version-one grammar below.

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
Workspace roots sort before configured roots and discovery within each kind
sorts by canonical path. This order governs filesystem discovery and primary
authority selection only; it is not the provider-safe root comparator below. The
first root whose kind-specific discovery rules actually yielded the candidate is
its primary authorizing root; mere path containment grants no candidate
authority. The complete ordered root inventory preserves every other root that
also yielded that source. Registration therefore assigns one identity, and
admission cannot render the same source twice through root aliases or observe
two versions from a mid-scan edit.

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

Each discovery snapshot separately retains its ordered candidate link to the
registered identity. A registration may therefore be observed by several scans
without losing which session and root authorized each observation.

For an agent document, source content is the file's exact bytes. For a skill,
version-one source content is the exact `SKILL.md` bytes. Supporting resources
are neither enumerated nor registered until a later contract defines their
relative identities, traversal, ordering, and hashes. Registration rejects a
non-UTF-8 source, invalid frontmatter, a skill name that differs from its parent
directory, a path escaping its root, or a root-relative source or scope path
longer than 4,096 UTF-8 bytes. A candidate whose canonical or root-relative
source path is not UTF-8 produces a typed `non_utf8_source_path` discovery
finding and never reaches registration. Rejection is a typed finding and creates
no partial bundle.

Version-one skill frontmatter is intentionally narrower than the evolving
external format. The source begins with a line containing exactly `---` and the
frontmatter ends at the next line containing exactly `---`; lines may end in LF
or CRLF, but mixed line endings are rejected. Between those delimiters every
nonempty line is one top-level `key: value` pair. A key is ASCII letters or
hyphen, a single ASCII space follows the colon, and a value is a nonempty
single-line UTF-8 plain scalar with no leading or trailing whitespace, NUL,
colon, number sign, quotation mark, reverse solidus, or YAML indicator.
Duplicate keys, comments, quoting, escapes, tags, anchors, aliases, flow
collections, multiline scalars, nested mappings, and sequences are rejected
rather than delegated to a library-selected YAML revision.

Exactly one `name` and one `description` are required. `name` is 1 through 64
ASCII bytes, uses only lowercase letters, digits, and single interior hyphens,
and may neither begin nor end with a hyphen; it must equal the parent directory
name. `description` is 1 through 1,024 UTF-8 bytes. Version one additionally
recognizes optional `license`, `compatibility`, and `allowed-tools` scalar keys,
each at most 1,024 UTF-8 bytes; they are retained as source metadata but grant
no authority. Every other key is a typed unsupported-metadata rejection. These
local rules, rather than the unversioned external page, determine registration
inventory until a later contract deliberately widens the grammar.

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
workspace registrations, so it instead names the exact selectors owned by the
[static-template grammar](configuration-and-credentials.md#the-static-session-template-catalog).
Session creation copies those selectors as unresolved eligibility input; it does
not scan an unbound workspace or invent bundle identities.

A session-specific allow-list replacement may name a configured-root bundle from
any scan because its stable configured-root identity is the sharing authority.
It may name a workspace-root bundle only when the target session has a complete
discovery snapshot that used its fixed workspace binding as the workspace root
and linked that exact registered identity as a candidate. A canonical-path match
without that session-correlated discovery link is not authority. A mismatch is a
typed rejection and exposes no source metadata to the target session.

Before a session carrying selectors can activate its first turn, the daemon
resolves its configured-root selectors and, when a workspace selector is
present, establishes the session's workspace binding through the owning
[pre-activation binding contract](configuration-and-credentials.md#derived-session-workspace-roots).
It scans and registers only after that binding is fixed, then resolves every
selector to exactly one identity. The owning binding contract persists the
binding correlation and makes initial installation crash-atomic with first-turn
activation; restart never reuses identities under an uncorrelated workspace or
blindly rescans them. A missing, stale, or ambiguous selector grants nothing and
is recorded as a typed resolution finding; it never degrades to a path glob or
newest-content match. Runner-workspace selectors remain unresolved until the
runner discovery protocol exists.

The effective eligibility snapshot is immutable for one turn. The owning
[activation transaction](turn-lifecycle-and-scheduling.md#the-activation-transaction)
copies the exact ordered bundle-identity list effective under its session lock
and records its versioned SHA-256 hash. Replacement serializes on the same lock
and affects only later activations. A registered bundle absent from that
snapshot cannot be enumerated, previewed, or admitted.

Until whole-bundle unload is implemented, a replacement command rejects removal
of any currently admitted identity or any identity in the frozen eligibility
snapshot of the session's active turn. Replacement reads those sets while
holding the same session-scheduler lock used by activation and admission, so an
active turn cannot admit an identity whose authority was concurrently removed.
This makes replacement neither an implicit unload nor an authority revocation
that effective input ignores. Additions and removal of identities absent from
both sets can still take effect at the next turn boundary; the later unload
transition owns removal from both sets.

Replacement also rejects adding or retaining a registered identity whose
canonical source path is already represented in the session's admitted set by a
different bundle identity. `instructions.read` repeats that guard while
serialized on the admitted-set head. A changed source may create new
registration evidence, but contradictory versions of one source cannot become
simultaneously admitted until unload can retire the old admission.

**Committed unimplemented functionality — eligibility control.** The first
implementation slice records the empty snapshot and exposes no replacement
command. Template authoring, session replacement, and visibility variants such
as metadata-only or user-only have no present template field, process request,
or session command. Their implementation must preserve the allow-list default
and frozen turn snapshot.

## Enumeration, preview, and admission

Eligible inventory is progressively disclosed. `instructions.list` returns a
cursor-paginated catalog with bundle identity, kind, display name, description
when present, source byte length, source hash, provider-safe root reference, and
root-relative source and scope paths. The root reference is the closed
`workspace` kind or `configured` plus `ConfiguredInstructionRootId`; neither it
nor any other result field contains a canonical absolute path. For `AGENTS.md`,
an empty scope means the root and a nested scope applies only to that directory
and descendants. Catalog order is root, then increasing scope depth, then raw
relative path. The provider-safe root comparator orders `workspace` before
`configured`; all workspace bundles have the same root key, while configured
roots compare by the 32 raw bytes of `ConfiguredInstructionRootId` in ascending
lexicographic order. Canonical absolute paths never participate. This comparator
is used wherever this page says root or projection order, including catalog
order, projection order, admitted-set records, and manifest records. An ancestor
document precedes a descendant document, so a deliberately admitted descendant
is the later, more specific instruction; sibling scopes never apply to each
other.

Version one returns at most 32 identities and 524,288 catalog-result bytes per
page. Those bytes are compact UTF-8 JSON with object keys sorted by raw ASCII
bytes, no insignificant whitespace, unsigned decimal integers without leading
zeroes, and strings escaped by the canonical algorithm below. A description
longer than 512 UTF-8 bytes is shortened at a character boundary and reports its
full byte length plus truncation boundary. The cursor is the
eligibility-snapshot hash plus the zero-based ordinal of the next item in
canonical order. Each page reports the snapshot's total item count, the returned
ordinal range, and remaining count. It first shortens descriptions and then ends
the page before an item that would exceed the byte bound; the cursor continues
at that unreturned item, so budgeting never drops an identity. The registration
path bounds guarantee that one minimally encoded item fits. An absent next
cursor proves enumeration is complete; a cursor for another snapshot is a typed
stale-cursor failure.

`instructions.preview` returns bounded structure — headings for a document and
validated metadata plus headings for a skill — with full source byte length and
an estimated model-token cost. It reads at most the registered source byte
length plus one byte. EOF before the registered length, an extra byte, a changed
hash, or disappearance is typed `stale_source` evidence; hashing never requires
reading beyond that bound. It neither returns the full body nor admits content.

Version one splits the revalidated UTF-8 source on LF and removes one terminal
CR from each line only when the registered document consistently uses CRLF. It
recognizes only ATX headings: zero through three leading ASCII spaces, one
through six `#` bytes, then end of line or one ASCII space or tab. The returned
heading records are in source order and contain the one-based line number,
level, and heading text after removing leading and trailing spaces or tabs and
an optional closing run of `#` bytes that is preceded by whitespace. A heading
text longer than 512 UTF-8 bytes is shortened at a scalar boundary and reports
its full byte length and truncation boundary. Setext headings, headings inside
fenced blocks, and other Markdown constructs are not interpreted. Fence
recognition uses this version-one state machine over the normalized lines.
Outside a fence, zero through three leading ASCII spaces followed by at least
three identical backticks or tildes opens a fence. The remainder of a backtick
opener must contain no backtick; the remainder of a tilde opener is ignored.
State retains the marker byte and opening run length. Inside a fence, only zero
through three leading ASCII spaces, a run of the retained marker at least as
long as the opener, and then only ASCII spaces or tabs closes it. Every other
line remains fenced. EOF does not close an unmatched fence, so all later
apparent headings remain excluded.

Preview returns at most 128 heading records and at most 65,536 encoded result
bytes using the compact canonical JSON rules of `instructions.list`. It stops
before the first record that would exceed either bound and reports the total
heading count, returned count, and `headings_truncated`; the bounded source read
still counts all headings deterministically. The token estimate is
`ceil(source_byte_length / 4)` and is explicitly an estimator versioned as
`utf8_bytes_div_4_v1`, not a provider tokenizer claim. Skill previews prepend
the validated `name`, `description`, and present recognized optional metadata to
that same bounded heading projection.

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
content. This retains the Agent Skills progressive-disclosure economics without
unrecorded selection or budget outcomes.

Nothing is admitted merely because it is eligible, heuristically relevant, near
a touched file, or present in a template. Version one admits only by the closed
`model_requested` route. Path-triggered or template-eager routes need separate
variants and triggering evidence.

**Committed unimplemented functionality — model-facing operations.** No present
tool supplies list, preview, or read unless an implementing child explicitly
advances this section's verified reference. Unloading is not implemented in the
first slice.

## Durable admission transition

Each `instructions.read` request has a replay-stable tool-request identity. The
owning
[tool result-commit transaction](tool-loop.md#serialized-staged-execution)
atomically commits its receipt-only result and, for a successful fresh read,
appends one `InstructionAdmission` naming the prior admitted-set hash, bundle,
rendered evidence, exact rendered wrapper bytes, and request identity. The
immutable admission row is the version-one plaintext authority for later
projections even if the workspace source changes or disappears. Replaying that
identity returns the same receipt and admission linkage; a conflicting replay is
corruption. A failed request appends neither admission nor a changed set.

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
policy. The provider-neutral prepared-operation field and adapter bridge are
owned by [model-call execution](model-call-execution.md) and
[runtime substrate](runtime-substrate.md); this page owns the region's bytes and
authority, not a competing operation shape.

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

Wrapper metadata uses one canonical JSON-string escaping algorithm. Quotation
mark becomes `\"`, reverse solidus becomes `\\`, and U+0008, U+0009, U+000A,
U+000C, and U+000D become `\b`, `\t`, `\n`, `\f`, and `\r`. Every other scalar
from U+0000 through U+001F becomes `\u00xx` with lowercase hexadecimal digits.
Solidus is not escaped, and every other scalar, including non-ASCII, remains its
literal UTF-8 encoding. Line endings shown above are LF, and there is no
implicit leading or trailing byte. After source-budget truncation, content
escaping replaces `&`, `<`, and `>` with `&amp;`, `&lt;`, and `&gt;` in that
order; repository bytes therefore cannot terminate or fabricate an envelope. The
rendered-content hash covers this complete escaped wrapper. These labels
distinguish untrusted repository text from daemon authority and make adapter
output byte-stable without disclosing host filesystem layout.

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

Version one fixes every admission's per-bundle source-byte budget at 32,768
bytes. `instructions.read` has no caller-supplied budget field. Rendering
preserves UTF-8 and emits the complete source or truncates to the unique longest
UTF-8 prefix whose byte length does not exceed that fixed budget before applying
the required content escaping and wrapper. It never borrows a shared pool whose
earlier entries can starve later ones. Why fixed and per source: identical
registered evidence must render identically on replay, and one large ancestor
document must not consume the budget of a more specific bundle. Rendered byte
length, source truncation boundary, and retained exact wrapper bytes are
evidence.

Version one has a fixed 65,536-byte aggregate workspace-instruction-region
budget, including every wrapper. A provider model with a smaller instruction
capacity or no typed system-instruction transport is not eligible for this
capability. A successful read serializes on the admitted-set head and preflights
the current region plus its candidate against that aggregate budget and the
active turn's pinned model target before committing its receipt or admission.
Concurrent and same-batch reads therefore observe one ordered predecessor and
cannot commit a set whose instruction region alone is unrenderable. Aggregate
exhaustion is a typed failed read and changes no durable admitted set. The
owning model catalog declares transport support and its byte capacity for every
selectable and serving target; no token-window conversion or adapter inference
supplies this value.

Later session-default replacement cannot strand existing admissions. The owning
[session-default contract](sessions-and-transcript.md#session-defaults-and-replacement)
rejects a model selection unless every configured target it may select supports
the typed region and can carry the complete retained region. This check occurs
before the successor defaults epoch commits; a rejection leaves both defaults
and admissions unchanged.

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
implementation slice has no admission and stores only the turn-start manifest.

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
