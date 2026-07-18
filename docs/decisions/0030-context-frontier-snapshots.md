# ADR-0030: Immutable context-frontier snapshots

- Date: 2026-07-17
- Owners: Repository owner
- Reviewers: Codex (architecture and acceptance consistency review); no specialist human reviewer assigned
- Supersedes: none
- Superseded by: none
- Depends on: the accepted foundation set ([ADR-0001](0001-domain-terminology-and-identity.md), [ADR-0003](0003-session-creation-and-transcript-ancestry.md), [ADR-0004](0004-turn-and-attempt-lifecycle.md), [ADR-0005](0005-model-call-retry-semantics.md), and [ADR-0027](0027-input-delivery-lifecycle.md))
- Refines: ADR-0001's identity boundary, ADR-0003's transcript-frontier boundary, and ADR-0027's context-frontier and eligibility rules
- Decision questions: context-frontier identity and resolved contents; identity versus semantic-content equality; immutability and prefix preservation; trusted construction and eligibility proof; domain, application, and persistence responsibilities; transcript-ancestry resolution

## Context

ADR-0027 defines a context frontier as an immutable reference to the exact ordered semantic content consumed by one model call. It also requires an accepted-input-origin turn's starting frontier and lineage to be fixed together at eligibility. ADR-0003 separately defines a transcript frontier as the immutable source boundary used by session ancestry. Those decisions intentionally do not define what either reference contains, how equality works, or what authority establishes that one exact frontier is correct for a particular turn or call.

That omission now blocks a safe domain slice. A bare identifier is stable but cannot expose the content needed to validate ordering. A bare ordered list exposes content but does not provide a durable identity for call, start, ancestry, and audit correlations. Letting arbitrary callers provide either and declare it valid would turn a storage lookup or structurally plausible list into lifecycle authority.

The missing boundary is not a cryptographic or formal proof system. Signalbox needs an immutable identified snapshot whose contents can be resolved for pure domain validation, plus a trusted aggregate transition that proves why that snapshot is the correct one for this start or call.

## Decision

### Identified, resolved snapshots

Each context frontier references one durable, session-owned immutable snapshot:

```text
opaque ContextFrontierId

ContextFrontier = {
    owning_session: SessionId,
    snapshot: ContextFrontierId
}

SemanticTranscriptEntryRef = {
    source_session: SessionId,
    entry: SemanticTranscriptEntryId
}

ResolvedContextFrontierSnapshot = {
    frontier: ContextFrontier,
    ordered_entries: OrderedDistinct<SemanticTranscriptEntryRef>
}
```

This is semantic pseudocode, not a Rust, storage, or wire API. `ContextFrontierId` is a new domain identity distinct from `SessionId`, `TurnId`, `ModelCallId`, `SemanticTranscriptEntryId`, and `TranscriptFrontier`. Matching backing bytes, if a later representation permits them, make none of those values interchangeable.

A context-frontier snapshot belongs to exactly one consuming session. A complete reference therefore carries that session together with the session-scoped snapshot identity. Its resolved contents are the exact ordered sequence of immutable semantic transcript-entry references represented by the frontier; each reference carries the entry's source session so ancestry does not depend on globally scoped entry identifiers. The sequence contains no duplicate entry reference. Repeated text or equivalent semantic payload remains representable through distinct entry identities; duplicating the same history entry is not.

An accepted baseline derivation is prefix-preserving. A later snapshot derived from an earlier context frontier retains every earlier entry reference in the same order and appends only semantic entries that the accepted lifecycle makes eligible. The accepted successor-start formula, safe-point guards, included outcome markers, and exclusions remain owned by ADR-0027; this ADR defines the value and authority boundary used to enforce that formula rather than restating it.

Once a complete `ContextFrontier` is committed, its owning session and the ordered entries of its resolved snapshot never change. Correcting or extending semantic history creates new semantic entries as required and a new snapshot identity. It never edits an existing snapshot.

Physical storage may materialize the complete ordered sequence, retain a parent plus appended entries, share immutable prefixes, or use another representation. Every representation must resolve to the same complete domain value and reject a stored session-and-identifier reference that maps to different contents.

### Equality

Ordinary context-frontier equality is identity equality:

```text
same_frontier(a, b) =
    a.owning_session == b.owning_session
    and a.snapshot == b.snapshot
```

Two independently created snapshots may contain the same ordered entry references without being the same frontier. When that comparison is needed, it is explicit:

```text
same_semantic_content(a, b) =
    resolve(a).ordered_entries == resolve(b).ordered_entries
```

Semantic-content equality compares the complete ordered source-session and entry-identity sequence, not rendered text, request bytes, a digest, or an unordered set. It does not authorize substituting one frontier for another in an already-fixed turn start, model call, or ancestry relation. Those records retain the exact snapshot identity and its owning-session correlation.

A digest may accelerate integrity checks or content comparison after the domain relationships are known. It is neither semantic identity nor proof that a frontier was correctly selected, and this decision does not require content-addressed identifiers or canonical deduplication.

### Construction authority and proof

Neither a raw `ContextFrontierId` nor an ordered entry list is proof that a frontier exists, resolves immutably, belongs to a session, or is correct for a lifecycle transition. Public command and protocol boundaries cannot construct a valid `ContextFrontier`, `ResolvedContextFrontierSnapshot`, or `AcceptedInputTurnStart` merely from those values.

The authoritative aggregate transition derives each new snapshot from complete, resolved domain state:

- for an accepted-input turn start, the durable queue order and slot state, the exact eligibility-selected lineage, the resolved session-ancestry or terminal immediate-predecessor frontier required by ADR-0027, and the new origin semantic entry;
- for a later model call, the call's owning turn and attempt, the resolved prior frontier, and every newly eligible committed semantic entry under ADR-0027's safe-point and outcome rules.

The pure domain transition validates the relationships and returns the new immutable snapshot together with the lifecycle values that bind it. It performs no repository lookup and does not consult a mutable global “frontier producer.” Application and persistence code load the required authoritative projections, call the domain transition, and commit its result. That commit revalidates the expected aggregate state and referenced source boundaries under the same serialization boundary, so a stale or incomplete proposal cannot become authoritative.

For accepted-input eligibility, the opaque, constructible `AcceptedInputTurnStart` is the durable correlation proof:

```text
AcceptedInputTurnStart = {
    lineage: AcceptedInputStartingLineage,
    frontier: ContextFrontier
}
```

Its trusted constructor is available only to the eligibility transition after that transition derives the lineage and snapshot together. The frontier identifier by itself is not the proof. “Proof” here means that durable state can be reconstructed and the typed correlations revalidated after restart; it does not mean a theorem, signature, hash, capability token, or assertion supplied by a caller.

The model-call creation transition similarly binds its exact frontier to the immutable call record. A resolved snapshot establishes what that reference contains, while the owning transition establishes why that snapshot is valid for that start or call.

### Atomic persistence boundary

The transaction that first fixes an accepted-input turn start commits all of the following atomically:

- every newly committed semantic entry used by the frontier;
- the immutable context-frontier snapshot;
- the fixed starting lineage and frontier; and
- activation with the initial prepared attempt, or the eligible terminal failure required by ADR-0027.

A later call-preparation transaction likewise commits newly consumed semantic entries, the new snapshot, the call's frontier binding, and every associated steering disposition or other atomic lifecycle fact required by the accepted ADRs.

There is no durable state in which a start or call references a missing or partially populated snapshot, an entry is reported consumed without appearing in the bound frontier, or one snapshot identifier can later resolve differently. A crash before commit exposes none of the new facts; a restart after commit reconstructs all of them.

### Transcript ancestry

`TranscriptFrontier` remains a purpose-specific domain boundary distinct from `ContextFrontier` and `ContextFrontierId`. A source-session ancestry reference therefore cannot be passed directly where a call frontier is required.

To construct the first context frontier of an ancestry-derived session, the hub validates the source session and transcript frontier, resolves that frontier to its immutable ordered semantic-entry prefix, and preserves those source semantic-entry identities in order before appending the new session's origin entry. Copying source rows, retaining references, or sharing physical prefixes does not remint or change the semantic source entries. The new context-frontier snapshot is nevertheless owned by the new consuming session and has its own `ContextFrontierId`.

This preserves ADR-0003's independent source provenance while allowing transcript and context frontiers to resolve to the same semantic-prefix model. It does not require them to share a storage table, identifier type, or boundary representation.

## Invariants

This record refines the enforcement boundary of:

- INV-001: `ContextFrontierId` becomes another distinct semantic identity and cannot substitute for an entry, session, turn, call, or transcript-frontier identity.
- INV-009: eligibility derives and atomically fixes one opaque `AcceptedInputTurnStart`; neither an identifier nor a list supplied by a caller establishes that authority.
- INV-015: every call frontier resolves to one immutable exact ordered semantic-entry sequence and is bound during the call-creation transition.
- INV-030: ancestry resolution preserves the selected source frontier and semantic-entry identities even if persistence copies or shares their physical representation.

The invariant catalog links these rows to this record without duplicating its normative rules.

## Strongest alternative

Store only the ordered semantic-entry identities on each turn start and model call, with structural list equality and no separate frontier identity. This is locally complete and avoids one identity type and lookup.

It is rejected because starts, calls, ancestry, audit evidence, and persistence would each embed or duplicate a potentially large sequence without one stable semantic reference. It would also make “equal contents” silently stand in for “the exact snapshot fixed by this transition,” losing provenance when independently created snapshots happen to contain the same entries.

## Rejected alternatives

- **Store only `ContextFrontierId`.** An opaque lookup key does not give pure domain code the ordered content required to validate prefix preservation, steering consumption, or successor construction.
- **Use a turn, model-call, or presentation-message identity as the frontier.** A turn can have several call frontiers, a call is the consumer rather than its context, and one presentation message can project zero or several semantic history entries.
- **Make frontier identifiers content-addressed or canonical by default.** Hash equality would conflate identity, content comparison, and construction authority while selecting canonicalization and hash policy without need.
- **Let a stateful domain frontier service decide validity.** Hidden mutable state and I/O would make domain transitions dependent on lookup timing and obscure which aggregate facts authorize construction.
- **Remint inherited semantic entries in a fork.** Physical copying would then rewrite semantic provenance and make copy-versus-reference storage choose domain identity.
- **Allow silent removal or reordering in a later baseline frontier.** That would contradict the accepted successor and steering-retention rules; any future compaction or selective-context policy needs an explicit semantic marker and a foundation decision.

## Consequences

Domain transitions operate on resolved snapshots rather than arbitrary IDs, so application code must load the complete authoritative projection before calling them. Persistence must enforce immutable identifier-to-content mapping and atomic binding, but it remains free to choose an efficient normalized or prefix-sharing representation.

Identity equality stays cheap and unambiguous. Exact-content comparison is available when genuinely needed, but duplicate-content snapshots are legal and retain their independent creation and session provenance.

Snapshot records add durable identities and may increase record count. Prefix sharing can reduce physical duplication without changing domain semantics. Accepted [ADR-0022](0022-persistence-representation.md) maps context frontiers into normalized persistence records while preserving this domain boundary and its freedom to materialize complete membership, store parent plus append, or share immutable prefixes.

## Scenario walkthroughs

- **S01:** With no ancestry, eligibility commits the origin input's semantic entry, a new session-owned snapshot containing that entry, and an opaque start binding it to `FirstInSession`, together with the initial attempt. The client cannot submit the start or frontier.
- **S03:** Restart derives eligibility from durable order and resolves the same ancestry or exact terminal predecessor required by ADR-0027. A committed start already names its immutable snapshot; an uncommitted transition is safely recomputed and committed once with activation or eligible failure.
- **S09:** Turn B stores no frontier while queued. When it becomes eligible after every priority insertion, the transition resolves the exact immediate predecessor, creates a prefix-preserving snapshot with B's origin entry appended, and fixes that identity with `After { immediate_predecessor }`.
- **S17:** The fork keeps its distinct source-session `TranscriptFrontier`. The new session's first context snapshot resolves and preserves the selected source entries, appends its own origin entry, and cannot change when the source later advances or when storage changes between copy, reference, and hybrid forms.

## Extension implications

Future manual-regeneration or non-input turn origins must define their own source-frontier authority and new semantic contribution before using this construction boundary. Multiple-source ancestry must define source ordering, duplicate-entry treatment, and provenance before it can resolve a merged prefix.

Any future semantic compaction, selective omission, rebasing, or context-window policy that would stop producing prefix-preserving frontiers must define visible semantic replacement markers and revise the accepted successor rules through an ADR. It cannot reinterpret an existing snapshot or make presentation rendering the source of context identity.

## Open questions

- Semantic transcript-entry payload variants, commit granularity, and rendering remain open; this ADR uses their already-distinct identities without defining their contents.
- The [initial Rust representation](../decisions.md#2026-07-17--uuid-backed-context-frontier-values-and-sealed-prefix-derivation) selects private UUID-backed `ContextFrontierId` and `SemanticTranscriptEntryId` values. Identity generation, UUID version, caller supply versus hub minting, database and wire encoding, serialization, and public formatting remain open. ADR-0022 selects native Postgres columns only for the UUID-backed identities it names; the in-process choice does not silently extend that database mapping. Context-frontier semantic ownership remains session-scoped regardless of eventual representation.
- Which terminal semantic boundaries a client may select as a `TranscriptFrontier` remains separate from how a validated selection resolves.
- Physical snapshot layout, lookup interfaces, caching, integrity checks, and layout-specific migration details/tooling remain persistence concerns within ADR-0022's accepted migration discipline.
- Non-input origin frontiers, multi-source ancestry, and semantic compaction remain future foundation decisions.

## Explicit non-decisions

This ADR does not choose a database schema, event model, repository trait, cache, hash, serialization format, wire field, UUID version, or identity-minting component. It does not define semantic-entry payloads, assistant-content commit granularity, provider prompt rendering, token budgeting, transcript presentation, compaction, merge, regeneration, or any non-input origin. It adds no stateful service and grants no caller authority to select or construct lifecycle frontiers.
