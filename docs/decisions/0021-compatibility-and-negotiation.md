# ADR-0021: Compatibility and negotiation baseline

- Date: 2026-07-17
- Owners: Repository owner
- Reviewers: pull-request review; no specialist human reviewer assigned
- Supersedes: none
- Superseded by: none
- Coupled with: [ADR-0019](0019-process-protocol.md), proposed together as one pair to be decided together — the process protocol whose every exchange performs this negotiation
- Depends on: the accepted foundation set ([ADR-0001](0001-domain-terminology-and-identity.md), [ADR-0003](0003-session-creation-and-transcript-ancestry.md), [ADR-0004](0004-turn-and-attempt-lifecycle.md), [ADR-0005](0005-model-call-retry-semantics.md), [ADR-0027](0027-input-delivery-lifecycle.md)) and [ADR-0009](0009-dispatch-fencing.md), whose open compatibility-window question this baseline frames
- Decision questions: the version-plus-capability handshake and its explicit-incompatibility rule (INV-033); what the version names and what capabilities may change; the compatibility window version one promises; the fixture discipline that enforces the window; which process boundaries the baseline governs

## Context

This record addresses two foundational questions listed under [protocols and persistence](../open-questions.md#protocols-and-persistence-reserved-adr-0019-through-adr-0023): protocol version and capability negotiation, whose recorded leaning is a version-plus-capability handshake with explicit incompatibility, and the cross-release compatibility policy, whose recorded leaning is a small documented compatibility window with fixtures and whose exact window is open (S12, S24). Both scenarios name the compatibility window among their remaining questions, and ADR-0009 leaves the window open for result delivery as well.

INV-033 is the catalog constraint this pair exists to make enforceable: unknown or incompatible protocol versions and required fields fail explicitly rather than being silently reinterpreted, with negotiation rules and compatibility fixtures as the anticipated enforcement. The consumers make a small window honest rather than restrictive: every version-one client — the native Swift applications, the Rust terminal client, and the browser client — is owner-controlled and released alongside the hub, so the window's job is to make skew during upgrades and development explicit and survivable, not to support an ecosystem of third-party clients.

## Decision

### One protocol version names the whole baseline

The process protocol carries a single integer **protocol version**. A version names the complete exchange semantics: ADR-0019's request families, envelope, rejection classes, and event typing, the message schema baseline, and this record's negotiation rules themselves. Version one is `1`. Anything that removes or changes the meaning of existing protocol content increments the version; a version is a statement about meaning, not a build number, and two releases that interoperate silently under the same version must mean the same thing by every exchange.

### Capabilities gate additions within a version

A **capability** is a named, documented, additive unit of optional protocol content — a new command kind, snapshot projection, event type, or field group — that a version-`n` speaker may support without a new version. Capabilities are only ever additive: removing or changing the meaning of anything, including a capability itself, is a version increment, not a capability change. Version one ships the mechanism with an **empty capability set**: everything in version one is required, and the first real capability arrives with whatever slice needs one.

### The handshake and the sending discipline

Negotiation is semantic, and its rules bind every transport mapping:

- A client presents the protocol versions it supports and the capabilities it requests. The hub answers with the negotiated version — the highest version both support — and the granted capability set (the intersection of requested and hub-supported), or with the typed incompatibility below. An explicit negotiation exchange is available to clients as its own unary request, and every request is attributable to one negotiated version and capability set; whether that attribution is carried per connection, per request, or both is fixed with the transport slice under ADR-0019.
- **Explicit incompatibility is a typed protocol result**, not a transport failure code alone: it names the versions the speaker presented, the versions the hub supports, any missing capabilities, and upgrade guidance. A hub receiving an unsupported version or a request using an ungranted capability answers with it; a client receiving it fails visibly and renegotiates or reports — neither side degrades silently or reinterprets (INV-033).
- **The hub never sends content outside the negotiated version and capability set** — no event kind, message, field group, or variant the negotiation did not establish. Under that discipline, unknown required content received by either side is a defect, and the rule for it is INV-033's: fail explicitly, never guess. A client that cannot interpret a durable-transition event does not skip it; it surfaces the failure and may re-snapshot only after renegotiating a compatible baseline.

### The version-one compatibility window

Version one promises exactly this window:

- Every hub release supports the newest protocol version it defines **and, when that version has a predecessor, the immediately preceding version**. The window is therefore always the newest version and its predecessor; support for an older version ends only when a still-newer version supersedes the pair, so no single upgrade strands a current client without an explicit, fixture-backed deprecation step.
- Capabilities never shrink within a supported version: a capability granted by a hub release is granted by that release lineage until a version increment retires it.
- Clients presenting only versions older than the window receive the typed incompatibility with upgrade guidance — an explicit refusal, never degraded or guessed operation.
- Durable-command deduplication is unaffected by the window: replay equality is structural domain equality over the canonical payload (ADR-0027), not wire-encoding equality, so a command submitted before an upgrade and replayed after it returns the recorded result. Every supported version's encoding of one command must construct the same canonical payload, and the fixtures below pin that.

The window is deliberately small, per the recorded leaning, and it is documented: the protocol schema documentation lists the supported versions and capabilities of each hub release, and that list is the promise of record.

### Fixtures enforce the window

Each supported protocol version has a frozen, language-neutral fixture corpus — the golden request, response, and stream transcripts ADR-0019 defines — and the hub's test suite replays every supported corpus, including the cross-version deduplication fixtures above and the incompatibility exchanges themselves. Retiring a corpus is the visible act of dropping a version's support and happens only together with the release step the window permits. Fixtures are how INV-033's row gains its enforcement links as slices land.

### One baseline for every hub process boundary

This baseline governs every process boundary the hub exposes. The client process protocol adopts it through ADR-0019. The reserved runner protocol records (ADR-0008, ADR-0016) must satisfy INV-033 with this same version-plus-capability shape and window promise for the runner boundary — with their own versions, capabilities, and fixture corpus — or explicitly justify divergence in the record that defines them; this record decides no runner protocol mechanics. That is the baseline S12's open compatibility-window question was waiting on: the window rule is fixed here once, and the runner records instantiate it.

## Invariants

This pair is the normative source INV-033's catalog row awaits; this record fixes the negotiation rules, sending discipline, window, and fixture enforcement, and relies on INV-012 for cross-version deduplication stability. The catalog row remains the statement of record and gains its enforcement links as the fixture corpora land.

## Strongest alternative

**Support the current version only, with atomic client-and-hub upgrades.** It is the simplest honest policy for owner-controlled clients and was seriously considered. It is rejected because it makes every hub upgrade a coordinated flag-day for four codebases and both app stores: an iOS review delay or a stale browser tab turns into hard refusal with no window at all, and development against a moving hub loses the one-step skew that makes incremental slices workable. One preceding version is the smallest window that removes the flag-day while staying trivially documentable and fixture-backed.

## Rejected alternatives

- **Semantic-version ranges over the schema.** Range arithmetic invites silent tolerance of meaning changes; a single integer whose increment is defined by meaning, plus named capabilities, states exactly what negotiation can establish.
- **Feature detection by probing or by tolerating unknown fields.** Silent reinterpretation by another name; INV-033 exists to exclude it. Additions are negotiated capabilities, so nothing arrives unannounced.
- **A time-based support window (for example, versions newer than N months).** Wall-clock promises decay invisibly and are not fixture-enforceable; a release-step window is checked by the corpora that exist in the tree.
- **Per-message versioning without a protocol-wide version.** Every consumer would carry a compatibility matrix per message; the protocol-wide version names one reviewable baseline, and capabilities carry the additive variation.
- **Deferring the window to the first public release.** S12 and S24 already name the window among their remaining questions, and the first cross-process slice needs the refusal and fixture rules before a second independently versioned process exists.

## Consequences

Every transport mapping carries version attribution, and the hub maintains fixture corpora for up to two protocol versions at a time — a real but bounded test-suite cost that is also the enforcement of record. Clients implement one incompatibility surface and can present it honestly to users instead of failing obscurely. Protocol evolution acquires a fixed rhythm: additive changes ride capabilities with fixtures in the same change; meaning changes pay for a version increment and a deprecation release. Nothing about the window weakens durable semantics: deduplication, replay, and recorded results are version-independent by construction.

## Scenario walkthroughs

- **S01 (create a session):** First contact is a negotiation: the client presents version 1 and no capabilities, the hub grants version 1, and the `CreateSession` and `SubmitInput` envelopes proceed under ADR-0019. A malformed negotiation or an unsupported version yields the typed incompatibility before any command is constructed; nothing claims a command identifier (ADR-0001's non-claiming boundary).
- **S02 (stream a provider response):** The subscription's event types are all version-1 required content, so the client interprets every durable transition and draft it receives. When a later hub adds a richer draft-progress event, it ships as a capability: this version-1 client never requested it, the hub never sends it, and the stream stays interpretable without the client knowing the capability exists.
- **S12 (stale or duplicated runner result):** The scenario's remaining compatibility-window question gets its rule here: when the reserved runner records define the runner boundary, they instantiate this baseline, so a runner reconnecting across a hub upgrade either negotiates a supported version — and ADR-0009's fence semantics, being version-independent durable semantics, classify its redelivered result envelope exactly as before — or receives the typed incompatibility instead of having its envelope silently reinterpreted. On the client side, the applied result reaches subscribers as the same durable-transition content under either supported protocol version, pinned by the cross-version fixtures.
- **S24 (reconnect during active streaming, across an upgrade):** The hub was upgraded while the client was disconnected mid-stream. On reconnect the client presents version 1; the hub, now also defining version 2, still supports version 1 under the window and grants it, and the client re-snapshots and resumes per ADR-0019 — including resubmitting its unresponded `SubmitInput` with the same identifier and receiving the recorded result, because structural payload equality survives the upgrade. A client older than the window instead receives the explicit incompatibility naming supported versions and upgrade guidance; it renders that refusal rather than guessing at a stream it can no longer interpret.

## Extension implications

The reserved runner protocol records (ADR-0008, ADR-0016) instantiate this baseline for the runner boundary with their own version line and corpus. Reserved ADR-0020 and ADR-0023 operate inside a negotiated baseline and add no second negotiation mechanism. Delegation, archival, regeneration, and future configuration categories arrive as capability-gated schema additions with fixtures in the same change, or as version increments when they change meaning; none of them may extend the window without superseding this record.

## Open questions

- The transport carriage of version and capability attribution (connection-scoped, per-request, or both) is fixed with ADR-0019's transport slice.
- Capability naming conventions and their documentation format are fixed when the first real capability ships.
- The runner boundary's version line, capabilities, and any justified divergence belong to the reserved runner records (ADR-0008, ADR-0016).
- Whether a future third-party-client posture needs a longer window is a question for the release that would invite one; it supersedes this record explicitly if so.
- Fixture storage layout and replay tooling are implementation-slice choices under the repository's dependency rules.

## Explicit non-decisions

This record does not decide the runner protocol's mechanics, message schemas, or transport (reserved ADR-0008, ADR-0016), the exact browser transport (reserved ADR-0020), Swift type-generation mechanics (reserved ADR-0023), or authentication and authorization (reserved ADR-0015). It selects no dependency and adds no code. It does not define persistence compatibility, database migration policy, or archival formats — ADR-0022 owns that boundary — and it does not promise any support window for anything other than the process-protocol boundaries named above.
