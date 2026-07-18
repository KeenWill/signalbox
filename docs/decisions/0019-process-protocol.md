# ADR-0019: Process protocol

- Date: 2026-07-17
- Owners: Repository owner
- Reviewers: pull-request review; no specialist human reviewer assigned
- Supersedes: none
- Superseded by: none
- Accepted with: [ADR-0021](0021-compatibility-and-negotiation.md) as one coupled pair — the negotiation and compatibility baseline every exchange of this protocol performs
- Depends on: the accepted foundation set ([ADR-0001](0001-domain-terminology-and-identity.md), [ADR-0003](0003-session-creation-and-transcript-ancestry.md), [ADR-0004](0004-turn-and-attempt-lifecycle.md), [ADR-0005](0005-model-call-retry-semantics.md), [ADR-0027](0027-input-delivery-lifecycle.md)), the accepted refinements ([ADR-0022](0022-persistence-representation.md), [ADR-0030](0030-context-frontier-snapshots.md), [ADR-0031](0031-direct-fatal-terminalization.md)), and the accepted scheduler pair ([ADR-0009](0009-dispatch-fencing.md), [ADR-0010](0010-initial-scheduler-mechanics.md)) for the fence and result-acceptance semantics its walkthroughs reference
- Decision questions: the authoritative-snapshot versus transient-stream read semantics (INV-032); the durable command envelope and typed rejection surfaces over INV-012's owner-global deduplication; the schema language and wire protocol across Swift, browser, and Rust terminal clients; what remains reserved for ADR-0020 and ADR-0023

## Context

This record addresses the foundational question of the process protocol — listed under [protocols and persistence](../open-questions.md#protocols-and-persistence-adr-0019-through-adr-0023) — which blocks cross-process implementation (S01, S02, S12, S24), and whose recorded leaning is to define semantics and fixtures before selecting transport. The record follows that leaning in its structure: the protocol semantics below are normative independently of transport, and the technology selection is stated afterward against those semantics.

The accepted records already fix most of what the protocol must carry. ADR-0001 and ADR-0027 fix owner-global durable-command identity, canonical construction before command lookup, first-committed-handling recording, replay, conflicting reuse, and the non-claiming boundary. INV-032 requires a reconnecting client to reconstruct authoritative durable state and replace transient drafts; S24 fixes that gaps cause another snapshot, never guessed tokens, and that reconnecting creates no logical work. INV-002 and INV-005 keep protocol messages distinct from domain types, storage records, and any universal event type, and the [architecture's dependency direction](../architecture.md#dependency-direction) states that shared protocols describe compatibility at a process boundary, not the canonical persistence schema or the domain model.

The consumers are fixed by owner constraint. The production clients are native Swift applications on macOS and iOS, and Swift consumer ergonomics are the first-class selection criterion. A Rust terminal/TUI client and a browser client are roughly co-equal second-tier consumers, and browser viability is an explicit evaluation criterion for the transport itself: a candidate that requires a proxy or degraded browser semantics must account for that cost in the comparison rather than defer it. The hub is a Rust process ([`crates/domain`](../../crates/domain/src/lib.rs) already fixes the domain types the boundary must map to), so Rust server cost is likewise a real criterion.

## Decision

The Signalbox process protocol is the hub's authoritative client-facing API boundary. Its semantics are decided first and are normative for any transport; the schema and wire protocol selection follows. Every connection or exchange of this protocol performs the version-plus-capability negotiation defined by [ADR-0021](0021-compatibility-and-negotiation.md).

### Three request families, one streaming shape

The protocol has exactly three domain-bearing request families in version one:

1. **Commands** — state-changing requests carrying the durable command envelope below. Commands are unary: one request, one response.
2. **Snapshot reads** — requests for an authoritative projection of durable state. Reads are unary, claim no command identifier, and create no logical work (S24).
3. **Subscriptions** — server-streaming requests delivering typed events after a stated durable point.

Alongside these, every connection performs the version-and-capability negotiation of [ADR-0021](0021-compatibility-and-negotiation.md) as its own unary exchange. Negotiation is a protocol-control exchange, not a fourth domain family: it carries no durable command and no snapshot projection, it rides the same unary request shape these families use, and — unlike them — it is version-independent so it can be performed before any protocol version is agreed. It is a required part of the fixture corpus below, and the walkthroughs assume it has already established the negotiated baseline.

The baseline therefore requires only unary requests and server-initiated streaming. No baseline interaction requires client streaming or bidirectional streaming; a future capability that needs them must be introduced under ADR-0021's negotiation rules. This restriction is deliberate: it keeps every candidate transport — including browsers, where request-body streaming is unavailable regardless of protocol — inside its undegraded subset.

### Authoritative snapshots and transient streams (INV-032)

The protocol distinguishes two read surfaces that never substitute for each other:

- **Snapshots are authoritative.** A snapshot response is a purpose-specific projection of durable state (the session list, or one session's accepted inputs, dispositions, turn and attempt lifecycle, provenance, and committed semantic content), together with an **observation cursor**: an opaque token naming the durable-state point the snapshot reflects. Cursors for one session are totally ordered; comparing cursors across sessions establishes nothing. Snapshots never contain transient drafts.
- **Streams are transient until stated otherwise.** A subscription from cursor `c` delivers two distinctly typed event classes. **Durable-transition events** report committed lifecycle facts and each carries the cursor it advances to. Every durable-transition event advances the cursor to a distinct, strictly greater value — no two events share a cursor — so within one subscription they arrive in strictly increasing cursor order with no gaps, and any facts that must be observed together are carried in one event; a client that stores a received cursor and reconnects resumes exclusively after it without losing a committed fact recorded at that point. The hub ends or refuses the subscription rather than skip. **Transient events** carry provider drafts and progress; each names its subject (for example the model call it drafts for), a draft identity, and a per-draft monotonically increasing sequence, and is explicitly replaceable. A transient event is never authoritative and never advances the cursor; the durable final content committed under the accepted lifecycle replaces the draft (S02, S24).

A subscription request names the cursor to resume from. If the hub cannot serve every durable transition after that cursor, it refuses with a typed `SnapshotRequired` response; it never silently resubscribes the client somewhere else. A client detecting any gap, regression, or doubt requests a new snapshot rather than guessing (S24). Because drafts are transient, a reconnecting client may simply miss draft content that was streamed while it was away; that is correct behavior, not loss — the authoritative outcome arrives as durable state.

How the hub derives cursors from committed state, and whether any draft content is checkpointed, remain open (the streaming-checkpoint question under ADR-0022). The cursor is a protocol value with the ordering and resumption semantics above, not a storage commitment.

### The durable command envelope (INV-012)

Every state-changing request is one durable command:

```text
CommandEnvelope = {
    command_id: DurableCommandId,        // the accepted payload's own durable identifier, carried on the envelope — not a second id
    command: DiscriminatedCommandPayload // one typed variant per command kind
}

CommandResponse =
    Applied { result: TypedAppliedResult }
  | RejectedByDomain { rejection: TypedDomainRejection }
  | RejectedAtBoundary { rejection: TypedBoundaryRejection }  // claims no identifier
  | ConflictingReuse { claimed: DurableCommandId }
```

This is semantic pseudocode, not a wire schema. The envelope adds nothing to the accepted command algebra: the discriminated payload is exactly the caller-supplied semantic fields ADR-0027 fixes (for `SubmitInput`: session, content, and the explicit `DeliveryRequest` with its expected active turn and version-bound configuration choices; for session creation: the [`CreateSession`](../../crates/domain/src/session.rs) payload), and the response is the recorded terminal applied-or-rejected result of first committed handling. Deduplication semantics are ADR-0001's and ADR-0027's by reference, not redefinition: owner-global identifier lookup precedes current-state validation, equal replay returns the recorded result, conflicting reuse is rejected without disturbing the recorded result, and failures before committed handling claim no identifier.

Three consequences are fixed here as protocol rules:

- **The caller supplies the durable command identifier, and it has a single authoritative source.** The identifier ADR-0027's `SubmitInput` and the `CreateSession` payload already carry *is* the envelope's `command_id` — never a second, independently supplied identifier. Surfacing it on the envelope does not change the established handling order: the boundary constructs the canonical typed command first, and owner-global identifier lookup then precedes current-state validation (ADR-0001, ADR-0027). A transport that encodes the identifier in both the envelope and the payload body carries the same value in both positions by construction, and any divergence is a boundary rejection claiming no identifier. An envelope without an identifier is unconstructible. The protocol requires caller supply of no other identity: commands reference existing identities (an expected active turn, a session), and identities created by an applied command are returned in its recorded result. Whether any future command may supply a new semantic identity stays with the open identity question.
- **The protocol adds no second concurrency layer.** Expected-state fields (`expected_active_turn`, the expected session-defaults version) live inside the typed payloads where the accepted algebra put them. The transport imposes and promises no cross-command ordering; clients sequence intent through those explicit fields.
- **Acknowledgement is the committed result.** The command response is produced only after the transaction that makes the result durable (INV-007). A connection lost between commit and response is indistinguishable from one lost before commit, and the client resolves it the same way: resubmit the same identifier and payload, and receive either the recorded result (committed) or first handling (not committed). This is the duplicate-submission-across-reconnect path walked under S24 below.

### Typed rejection surfaces

Every way a request can fail is one of four mechanically distinguishable classes, and the classes with different retry rules are never conflated:

| Class | Examples | Claims the identifier? | Client rule |
| --- | --- | --- | --- |
| Protocol error | Unsupported version, missing capability, malformed frame | No | ADR-0021 governs; renegotiate or fail visibly |
| Boundary rejection | Caller fields cannot construct the canonical typed command; owner authority not established; infrastructure failure before committed handling (which may surface as a transport failure rather than a typed payload) | No | Correct and resubmit — or resubmit unchanged after an infrastructure failure; the same identifier may be reused |
| Recorded domain rejection | Stale `expected_active_turn`; session-defaults version mismatch; interrupt rejected by stop state | Yes | Recorded and replayed under the identifier; corrected intent needs a new identifier |
| Conflicting reuse | Claimed identifier presented with a different kind, session, or canonical payload | Already claimed | Submit the other intent under a new identifier |

Command outcomes — applied results, recorded domain rejections, non-claiming boundary rejections, and conflicting reuse — are data: they travel as typed schema payloads in a successful transport exchange, never as bare transport status codes. Transport-level error signaling is reserved for the protocol-error class and for infrastructure failure before any typed response exists. This keeps the recorded-result replay of INV-012 representable (a replayed rejection is the same payload every time) and makes the boundary-versus-domain distinction, whose retry rules differ, impossible to blur in a client.

### Boundary discipline (INV-002, INV-005)

Protocol messages are a boundary representation. Generated or hand-written wire types never appear in a domain transition; the boundary maps each received payload through purpose-specific canonical construction before command lookup, and maps domain values outward deliberately. Within the negotiated version and capability set, a payload that cannot construct a canonical typed command is a boundary rejection; a discriminator that names a command kind, field group, or variant *outside* the negotiated set is not — it is the protocol-error class ADR-0021 governs (unsupported version or ungranted capability), so a newer client reaching an older hub receives the typed-incompatibility renegotiation contract rather than a boundary rejection's correct-and-reuse contract (INV-033). Snapshots and events are purpose-specific projections, not the ADR-0022 persistence schema re-served and not one universal event type. The wire encoding of a command is likewise not its deduplication identity: replay equality is structural domain equality over the canonical payload (ADR-0027), so two encodings of the same canonical command compare equal and a re-encoded replay still returns the recorded result.

### Fixtures are the protocol's statement of enforcement

The semantics above are captured as a language-neutral golden fixture corpus — recorded request, response, and stream transcripts for each family, including every rejection class — created with the first protocol slice and grown with each command kind and event type. The corpus is the compatibility baseline ADR-0021 replays across versions, and per [CONTRIBUTING](../../CONTRIBUTING.md#testing) it must exist before a second independently versioned process or language client merges.

### Schema and wire protocol selection

Message schemas are defined in **Protobuf** (proto3), the single schema source for all consumers; JSON encodings follow the proto3 JSON mapping. The wire protocol is the **Connect RPC protocol**, serving unary requests as plain HTTP POST exchanges (JSON or binary Protobuf) and subscriptions as Connect server-streaming responses.

Proto3's default handling of unknown fields — accept and preserve them unread — would let an older hub silently decode a newer client's ungranted-capability field or command variant as an older request, defeating INV-033 on the binary path. This selection therefore requires the opposite: every runtime and transport mapping must reject content outside the negotiated version and capability set rather than ignore it, and capability-gated additions are carried behind negotiated discriminators an older speaker classifies as the typed incompatibility (ADR-0021), never as optional fields it drops. Enforcing that on each mapping is a slice concern; the fixture corpus below includes the unknown-content exchanges that pin it.

The candidates were evaluated against the owner's criteria — Swift toolchain reality first, browser viability as an explicit transport criterion, and Rust server and TUI-client cost:

| Criterion | gRPC (tonic + grpc-swift-2, tonic-web for browsers) | Connect (connect-swift, connect-es, Rust runtime) | JSON/HTTP + SSE (hand- or generated types) |
| --- | --- | --- | --- |
| Swift codegen and API | `protoc-gen-grpc-swift-2` + SwiftProtobuf; async/await; SPM plugin | `protoc-gen-connect-swift` + SwiftProtobuf; async/await; interceptors; generated mocks | No IDL; hand-written `Codable` types per client or an OpenAPI layer that is weak for streaming |
| Swift transport reality | NIO-based transports only (no URLSession transport); platform floor macOS 15 / iOS 18; long-lived HTTP/2 streams outside the platform networking path carry the documented history of backgrounding and reconnect failures in Swift gRPC clients | URLSession-based default transport (platform-native networking path); floor iOS 12 / macOS 10.15; small runtime; streaming still pauses in background — reconnect-with-snapshot is this protocol's recovery model on every transport | URLSession works, but SSE has no native Swift client: hand-rolled or third-party event-stream parsing and reconnect |
| Browser reality | Native gRPC impossible in browsers (trailers, HTTP/2 framing); gRPC-Web translation required — `tonic-web` serves it in-process without a separate proxy, but the official JS client supports server streaming only in base64 text mode and XHR buffering makes long-lived streams fragile | Designed for `fetch`: unary is a plain POST (curl- and DevTools-debuggable JSON), server streaming works natively in binary mode, standard CORS, no proxy or translation layer | Native `EventSource` is GET-only with no request headers, so real deployments need fetch-based SSE anyway; response buffering by intermediaries is a known operational trap |
| Rust server | `tonic`: mature, widely deployed | A Tower-based runtime under the `connectrpc` organization exists (integrates with axum/hyper, serves Connect, gRPC, and gRPC-Web from one registration, passes the published conformance suite) but is pre-1.0; alternatively, the unary-plus-server-streaming subset of the published protocol spec is small enough to own in a thin layer over an HTTP server | axum serves JSON and SSE natively; lowest server cost, but every protocol rule above becomes hand-enforced convention in four codebases |
| Rust TUI client | tonic client, first class | Same Connect stack, or a plain HTTP/streaming client against the small spec; a Rust server that also exposes gRPC keeps tonic's client usable | Plain HTTP + SSE client |
| Fixture and debugging ergonomics | Binary framing; language-neutral fixtures require gRPC tooling | Unary JSON exchanges make golden fixtures human-readable; enveloped streaming frames are simple and documented | Human-readable, but schema drift is caught only by the fixtures themselves |

**Connect is selected** because it is the only candidate that is first-class on both client tiers the owner named: the Swift client library rides URLSession with a broad platform floor and production-grade codegen, and the browser client uses native fetch with no proxy, no translation layer, and no degraded streaming mode — the cost the evaluation was required to account for lands on gRPC, not Connect. The cost Connect does carry is on the Rust server, where the ecosystem is younger than tonic's; that cost is bounded by this record's own semantics-first structure: the protocol subset in use is small and publicly specified, the fixture corpus tests the wire rather than any library, and a hub implementation may use the existing runtime or own a thin conforming layer, with either choice made in the implementation slice under the repository's dependency rules.

This selection names a wire protocol and schema language, not dependencies: no crate, SwiftPM package, or npm package is selected here, and each implementation slice proposes its libraries under [CONTRIBUTING](../../CONTRIBUTING.md#contribution-rules), with owner approval for large dependencies. The selection is falsifiable: it stands only while the first cross-process slices pass the fixture corpus on all three client platforms — hub and TUI in Rust, Swift over URLSession, browser over fetch without a proxy — and failing that reopens the transport selection through a superseding record, with the tonic-plus-`tonic-web` hybrid below as the documented fallback.

**Input to reserved ADR-0023 (Swift client type generation):** the schema source is Protobuf, so the Swift boundary types are SwiftProtobuf-generated messages plus Connect-generated service clients. Those generated types are boundary representations under INV-002 — mapped into hand-written Swift client domain types, matching that question's recorded leaning — and ADR-0023 decides the generation mechanics: plugin and package layout, generated-mock policy, and the mapping conventions. Nothing in this record makes a generated type a client domain type.

## Invariants

This pair is the normative source INV-033's catalog row awaits; ADR-0021 defines those negotiation rules. This record fixes the enforcement approach for INV-032 (the snapshot/cursor/subscription semantics above), the protocol carriage of INV-012 (the envelope and rejection surfaces; the deduplication semantics remain ADR-0001's and ADR-0027's), and relies on INV-002, INV-005, and INV-007. The catalog rows remain the statements of record.

## Strongest alternative

**gRPC served by tonic, with `tonic-web` for browsers and connect-swift speaking gRPC-Web on Swift.** This keeps the most mature Rust server and still buys the URLSession-based Swift client, because the Connect client libraries can speak the gRPC-Web protocol. It is rejected as the primary selection because it standardizes the boundary on the translation protocol rather than a first-class one: browsers and Swift would both speak gRPC-Web with trailers folded into the body, the official browser client's streaming remains text-mode encumbered so the browser story quietly depends on the Connect client libraries anyway, and the debugging and fixture ergonomics of plain-JSON unary exchanges are lost. It remains the documented fallback if the Connect selection is falsified, and the Protobuf schema source is unchanged between them.

## Rejected alternatives

- **JSON/HTTP + SSE with hand- or generated types.** Cheapest server, viable everywhere — and it hand-enforces everything this record makes structural: discriminated command payloads, typed rejection classes, and event typing would be conventions maintained in four codebases, with drift caught only late by fixtures. The typed-boundary discipline the accepted records demand is exactly what an IDL automates.
- **Native gRPC only, browsers deferred.** Violates the owner constraint that browser viability is a transport criterion now, and the Swift platform floor (iOS 18 / macOS 15) plus NIO-only transports make the first-class criterion worse, not better.
- **WebSocket-based bespoke protocol.** Maximum semantic freedom, no IDL, no conformance corpus, and every ordering, framing, and reconnection rule becomes bespoke design this record would have to write and maintain; the baseline needs only unary plus server streaming.
- **GraphQL.** Query flexibility is not the problem shape: the boundary is a command-and-projection protocol with exact typed rejections and streams; schema-driven resolvers add an interpretation layer between clients and the accepted command algebra.
- **Making the persistence schema or domain crate types the wire contract.** Prohibited already (INV-002, ADR-0022's boundary); restated here only because reusing Protobuf for storage would be the tempting shortcut.

## Consequences

The repository gains a Protobuf schema source and a fixture corpus that every consumer builds against; protocol evolution becomes visible schema diffs plus recorded transcripts rather than convention. Clients get one recovery model — resubmit commands by identifier, re-snapshot on doubt — that is identical across transports and failure classes, which is what makes the iOS backgrounding reality tolerable rather than special-cased. The Rust hub takes on either a pre-1.0 runtime dependency (proposed in its slice, with tradeoffs) or ownership of a thin protocol layer; both are contained by the fixture corpus. Domain rejections traveling as data means client code always handles a typed result, and transport errors stay honestly transport-shaped.

## Scenario walkthroughs

- **S01 (create a session, first input):** After the version-1 negotiation exchange (ADR-0021) that every connection begins with, the client submits `CreateSession` in a command envelope with a fresh caller-supplied identifier; the hub constructs the canonical payload, finds the identifier unseen, commits first handling, and responds with the recorded applied result carrying the created session identity. `SubmitInput` with `StartWhenNoActiveTurn` follows as a second envelope; a malformed variant that cannot construct the typed command returns a boundary rejection claiming no identifier, and the corrected resubmission may reuse it. A defaults-version mismatch instead returns the recorded domain rejection, and replaying that envelope returns the same rejection even after the defaults change again — corrected intent needs a new identifier.
- **S02 (stream a provider response):** The client holds a snapshot at cursor `c` and a subscription from `c`. Turn activation, attempt creation, and call creation arrive as durable-transition events advancing the cursor; provider deltas arrive as transient events with the call's draft identity and increasing sequence. The client's disconnect neither cancels the call nor loses content: the hub's work continues, and the committed assistant content and call outcome arrive as durable transitions — to this client after it reconnects. No draft is promoted; the durable content replaces it.
- **S12 (stale or duplicated runner result):** The runner-facing envelope and fence live with ADR-0009 and the reserved runner protocol records, not here. What this protocol fixes is what the owner's clients observe: result application advances durable state at most once behind ADR-0009's compare-and-set, so subscribers see exactly one durable-transition event for the applied result at one cursor, and redelivered or stale runner envelopes produce no client-visible transition. The client-side analog is the command envelope itself: however many times a command is redelivered or resubmitted, first committed handling happens once and every response is the recorded result.
- **S24 (reconnect during active streaming, duplicate command across reconnect):** A client streaming drafts loses its connection mid-turn. On reconnect it negotiates under ADR-0021, requests a session snapshot (no command identifier, no new work), receives durable state at cursor `c'`, and subscribes from `c'`; the previously rendered draft is replaced by the snapshot's durable content plus any resumed transient events with the same draft identity and higher sequences — or, if the call finished while it was away, by final durable content with no further drafts. If the hub refuses the resume point with `SnapshotRequired`, the client re-snapshots; it never fabricates missed deltas. If the client had submitted `SubmitInput` just before the drop and never saw a response, it resubmits the same identifier and payload: if handling committed, it receives the recorded applied result and no duplicate accepted input or turn exists (INV-012); if it never committed, the identifier was never claimed and first handling proceeds now.

## Extension implications

Reserved ADR-0020 decides the exact browser transport within this selection — the Connect protocol over fetch is the default expectation, and that record chooses the concrete client configuration, cross-origin deployment posture, and any fallback mode — without reopening the snapshot/stream semantics. Reserved ADR-0023 decides Swift type-generation mechanics against the stated input. The reserved runner protocol record (ADR-0008) defines the runner-facing boundary, including the wire encoding of ADR-0009's fence fields, and adopts ADR-0021's baseline; nothing here presumes the runner boundary shares this protocol's schema or transport. New command kinds, snapshot projections, and event types arrive as schema additions under ADR-0021's negotiation rules, each with fixtures in the same change.

## Open questions

- Wire encoding of identities inside the Protobuf schemas (string versus bytes UUID forms, public formatting) remains the open identity question; the envelope treats every identity as an opaque required field until that decision lands.
- The canonical versioned record-family encoding used for durable-command structural comparison is decided by [ADR-0034](0034-durable-command-storage-and-equality.md); this record fixes only that wire encoding is not that comparison representation.
- Concrete snapshot projection schemas arrive with their slices; [ADR-0036](0036-initial-semantic-transcript-entries.md) fixes the first payload variants, while later semantic variants remain open.
- Cursor derivation from committed state, retention of resumable history, and any draft checkpointing remain open under ADR-0022's streaming-checkpoint question.
- Owner client authentication, authorization carriage, and revocation are reserved (ADR-0015); this protocol assumes established owner authority at the boundary ADR-0001 defines and selects no mechanism.
- Subscription backpressure, event batching, and resource limits on concurrent subscriptions remain open with first-release resource governance.

## Explicit non-decisions

This record does not decide the runner protocol (reserved ADR-0008), ADR-0023's code-generation mechanics beyond the stated input, or the exact browser transport beyond the viability evaluation (reserved ADR-0020). It selects no Rust crate, Swift package, npm package, or other dependency, adds no code, and defines no final message schema, field name, or endpoint path — pseudocode and tables above are semantic shapes for review, not a wire API. It does not choose authentication, credential storage, resource limits, client presentation, or client implementation order, and it does not alter the accepted deduplication, lifecycle, or frontier semantics it carries.
