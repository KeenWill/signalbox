# ADR-0046: Typed model-runtime substrate boundary

- Date: 2026-07-20
- Supersedes: none
- Superseded by: none
- Depends on: [ADR-0004](0004-turn-and-attempt-lifecycle.md),
  [ADR-0005](0005-model-call-retry-semantics.md),
  [ADR-0017](0017-credential-lifecycle.md),
  [ADR-0022](0022-persistence-representation.md),
  [ADR-0032](0032-postgres-implementation-dependencies.md),
  [ADR-0042](0042-assistant-content-and-completion.md),
  [ADR-0043](0043-provider-failure-classification.md), and
  [ADR-0044](0044-hub-runtime-foundations.md)
- Refines: the architecture's dependency direction with an explicit
  provider-neutral model-runtime layer, its in-repository crate placement, and
  its crate-dependency rule
- Decision questions: the three-layer boundary; runtime-crate placement and the
  dependency rule with its enforcement mechanism; model-execution port
  ownership; binding identity, retry, boundary-evidence, classification, and
  target-identity rules; substrate candidacy and fork/vendor triggers;
  anti-goals

## Context

The model-call milestone needs provider-translation machinery the accepted
records deliberately do not build: request construction against a provider
contract, capability profiles, JSON-Schema generation and transformation, typed
structured-output parsing, tool-schema generation and tool-call decoding,
streaming normalization, and low-level usage and error evidence. A distilled
external exploration (2026-07-20) proposes building that machinery as a
provider-neutral typed model runtime distinct from the durable platform, names
SerdesAI (`janfeddersen-wq/serdesAI` at commit
`1424128b0c64d9c2403eb0896cde881777941669`, MIT, workspace version 0.2.6) as the
candidate substrate, and surveys the surrounding typed-LLM crate ecosystem as
thinner or narrower than that need. The exploration is proposal-grade input;
this record decides only the boundary it requires.

Without a recorded boundary, the first provider slice chooses one by accident. A
framework agent loop could decide continuation after tool results or output
failure — decisions ADR-0004 and ADR-0005 reserve to durable orchestration. A
same-model retry wrapper could repeat a request the provider may already have
accepted, which ADR-0005 prohibits. Framework stream-event or error types could
reach durable records, which ADR-0022 and INV-002 prohibit for storage and
framework representations. Each accident would be copied by every later provider
adapter.

The naming collision is deliberate to resolve:
[ADR-0044](0044-hub-runtime-foundations.md) owns the hub's *asynchronous*
runtime, observability facade, operator taxonomy, and composition root. The
*model runtime* in this record is a different thing — a library layer that
executes exactly one explicitly authorized model operation against a provider —
and this record leaves every ADR-0044 choice unchanged.

## Decision

### Three layers, one inward dependency direction

Signalbox's provider execution stack has three layers:

1. **Layer 1 — provider-neutral typed model runtime.** Library crates that
   translate one explicitly authorized model operation into at most one provider
   interaction and translate what the provider returns into typed evidence:
   provider request translation, capability profiles, JSON-Schema generation and
   transforms, typed structured output with typed failure classes, tool-schema
   generation and tool-call decoding, normalized streaming observations, and
   low-level usage and error evidence. The runtime holds no durable state, makes
   no lifecycle decisions, and never becomes a workflow engine.
2. **Layer 2 — the Signalbox durable platform.** The existing domain,
   application, persistence, and hubd crates. It owns sessions, turns, the
   `ModelCall`, tool-request, and tool-attempt state machines, approvals,
   recovery, provenance, the outbox, and budgets under the accepted records.
   Layer 2 owns every durable identity and every retry, fallback, continuation,
   and classification decision; Layer 1 observes and reports.
3. **Layer 3 — an app-facing SDK.** A directional destination only, recorded in
   the [target model](../target-model.md#an-app-facing-sdk-target); this record
   decides nothing about it except its deferral gate under
   [Anti-goals](#anti-goals).

### Runtime crates live in this repository, sealed off from domain and application

Layer-1 crates live in this repository under `crates/`, so they share the
workspace toolchain, validation sequence, and review surface instead of adding a
second repository's release and version coordination.

Their dependency rule mirrors the persistence rule: runtime crates live in a
provider-runtime boundary outside `crates/domain` and `crates/application`, per
INV-002 and the architecture's dependency direction. The domain and application
crates must never depend on a Layer-1 crate, and no runtime type may appear in a
domain or application signature or value. Only two consumers may depend on
Layer-1 crates: the future provider-adapter crate that implements the accepted
application-side model-execution port, and the hubd composition root that wires
it.

Enforcement is the same mechanism that keeps the domain crate free of SQLx and
serde today: the Cargo manifest is the boundary. `crates/domain/Cargo.toml`
declares exactly one dependency (`uuid`) and `crates/application/Cargo.toml`
declares exactly two (`signalbox-domain` and `uuid`); a crate cannot name a
dependency its manifest does not declare, so any runtime import fails the
workspace build that CI runs on every pull request, and relaxing the rule
requires a manifest diff that review and the repository dependency guidance gate
explicitly. There is no additional allowlist script to maintain; the manifests
are the checked statement of the rule. Those manifest contents describe today's
code for falsifiability, not a frozen dependency set — accepted records may add
dependencies there (ADR-0044's `tracing` facade, for example) — while the rule
this record fixes is that no Layer-1 crate ever appears in the domain or
application manifests.

Layer-1 crates follow ADR-0044's library discipline: they may emit through the
`tracing` facade but never install a subscriber, never select the hub's runtime,
and never log or persist credential material — credential-value consumption
stays inside the adapter boundary [ADR-0017](0017-credential-lifecycle.md)
fixes.

### The model-execution port is owned by the orchestration record, not the runtime

The application-side model-execution port — the port shape through which
orchestration authorizes one provider interaction and receives observations — is
owned by the model-call orchestration record of the ADR process under the
model-call milestone (the [priority order](../target-model.md#priority-order)'s
third step), currently proposed as ADR-0045 in pull request #128. The runtime
track conforms to whatever port that process accepts; it never defines the port,
and runtime convenience is never a reason to widen it.

Any trait or interface the runtime spike defines locally — a one-operation
execute signature, an observer callback, an operation description — is draft
scaffolding, explicitly throwaway. It confers no authority, creates no
compatibility obligation, and is rewritten or deleted to conform when the
accepted port lands.

### Binding integration rules

Every integration between Layer 1 and Layer 2, including the first spike, obeys
these rules; each names its governing record:

1. **Caller-owned durable identity
   ([ADR-0005](0005-model-call-retry-semantics.md)).** Signalbox passes the
   durable `ModelCallId` into every runtime operation, and every runtime
   observation — preparation, boundary crossing, stream deltas, decoded tool
   calls, usage, target observations, terminal metadata — carries it. A
   runtime-generated run or request identity is never authoritative correlation.
2. **No hidden retries ([ADR-0005](0005-model-call-retry-semantics.md)).** The
   runtime and its provider adapters must not repeat a request after the point
   at which the provider could have accepted it. A retry is permitted only
   inside a provably pre-send preparation boundary, where it continues preparing
   the same durable call; any other repetition requires a new, explicitly
   authorized `ModelCallId` from Layer 2. Same-model retry wrappers and
   fallback-model wrappers are disabled on Signalbox paths unless each physical
   attempt surfaces for durable authorization.
3. **Prepared versus boundary-crossed evidence
   ([ADR-0005](0005-model-call-retry-semantics.md),
   [ADR-0043](0043-provider-failure-classification.md)).** Runtime evidence must
   distinguish "request prepared and provably never able to be accepted" from
   "the provider-acceptance boundary was or may have been crossed." ADR-0043's
   full-request-send classification consumes exactly this distinction; a runtime
   that cannot make it forces every failure into the ambiguous branch.
4. **Typed outcome classification
   ([ADR-0043](0043-provider-failure-classification.md)).** Outcome evidence is
   typed and sufficient to classify `Completed`, `KnownFailed`, `Refused`,
   `Cancelled`, or `Ambiguous` without string matching on rendered messages.
   Failure evidence collapsed to strings before classification is a defect, and
   an SDK's `retryable` or `transient` flag never becomes lifecycle authority.
5. **Three separate target facts
   ([ADR-0005](0005-model-call-retry-semantics.md)).** The requested selection,
   the hub-resolved exact pinned target, and the provider-reported model
   identity remain separate facts. The runtime reports a provider-reported
   identity as evidence when one is observable and fabricates neither a match
   nor a mismatch when none is.

### Substrate posture: candidate, audit-gated

SerdesAI is a **candidate** for vendoring selected crates, not an adopted
dependency. A Phase-0 audit, running separately, must establish whether its
crates can satisfy the binding rules above — caller-supplied operation
identities on every streaming and tool event, an observable acceptance boundary,
evidence preserved in typed form, an agent loop decomposable into externally
orchestrated one-operation steps. The vendor-versus-hand-roll decision is
deferred to that audit's evidence and lands as its own recorded decision under
the repository dependency gate.

Depending on upstream releases is not recorded as a durable option. The
maintenance signals checked on 2026-07-20: 20 stars, 4 forks, 2 human
contributors across roughly 43 commits, repository created 2025-12-27 with the
last push 2026-07-16; the crates.io release (0.2.6, published 2026-02-20, 2,105
lifetime downloads) is stale relative to repository activity; the README
quick-start pins version 0.1 against a 0.2.6 workspace; and the project's parity
matrix asserts streaming terminal integrity only for one audited provider path.
Code owning Signalbox's provider boundary cannot take retry and evidence
semantics from that release cadence.

If vendored crates are adopted, moving further — to a maintained fork or a
clean-room runtime — is triggered by demonstrated structural mismatch, not
convenience. The recorded triggers: the runtime cannot accept caller-owned
durable operation identities on every event; adapters hide retries or cannot
expose an acceptance boundary; failure evidence is collapsed to strings too
early for ADR-0043 classification; the agent loop cannot decompose into
externally orchestrated one-operation steps; event or schema versioning is
incompatible; or upstream rejects required changes or its cadence is
incompatible.

### Anti-goals

- The runtime is never the session aggregate root; the durable aggregate
  boundary stays with [ADR-0038](0038-session-aggregate-boundary.md).
- No automatic retry of ambiguous operations, at any layer
  ([ADR-0005](0005-model-call-retry-semantics.md), INV-026).
- Transient stream deltas are never canonical transcript history; only
  [ADR-0042](0042-assistant-content-and-completion.md)'s final-response commit
  makes content authoritative (INV-032).
- No universal agent abstraction that hides provider evidence or execution
  evidence behind a convenience surface.
- The app-facing SDK is deferred until at least one end-to-end application
  exists after the real-provider smoke test (the priority order's fourth step);
  generalizing earlier would encode guesses as public API.

## Invariants

This record's boundary depends on accepted rules indexed by the
[invariant catalog](../invariants.md): INV-002, INV-005, INV-014, INV-025,
INV-026, and INV-032. Their owning records remain normative; this section does
not duplicate them. No catalog row claims executable enforcement from this
record: the manifest boundary above becomes checkable only when the first
runtime crate lands.

## Strongest alternative

Depend on upstream SerdesAI releases directly as ordinary crates.io
dependencies. It is the fastest first spike and defers all maintenance to
upstream.

It is rejected because the maintenance signals above make upstream cadence an
authority over provider-boundary semantics: a semver-compatible upgrade could
change retry behavior at the acceptance boundary or reshape failure evidence,
and the stale crates.io release means the audited commit is not even installable
from a registry. Vendoring keeps the audited code under this repository's review
and validation.

## Rejected alternatives

- **Build provider translation directly inside the application crate.** It
  couples orchestration to provider wire evolution, invites provider and
  framework types across INV-002's boundary, and makes a second provider a
  second application rewrite.
- **Adopt the whole upstream agent framework as the execution engine.** Its
  agent loop, graph workflows, and retry conveniences decide continuation and
  repetition — decisions ADR-0004 and ADR-0005 reserve to durable orchestration.
  A convenient loop that bypasses durable call identity is the precise failure
  this record exists to prevent.
- **Fork immediately.** Signalbox has not landed its first provider-call slice,
  so friction with upstream internals is unmeasured; a premature fork encodes
  temporary assumptions into a public API and buys the whole maintenance surface
  before evidence demands it.
- **Hand-roll a clean-room runtime now.** It discards substantial existing
  provider, schema, macro, streaming, and test infrastructure with no
  demonstrated structural mismatch; the audit exists to measure that mismatch
  first.
- **A separate repository for runtime crates.** It adds release, version, and
  contribution coordination before the boundary is proven, while the
  in-workspace manifest boundary already provides the isolation that matters.

## Consequences

Runtime crates appear under `crates/` with manifests the domain and application
crates never reference, and the workspace validation sequence covers them from
the first commit. The Phase-0 audit produces the vendor-versus-hand-roll
evidence; whichever way it lands, the binding integration rules above already
constrain the result, so audit findings translate directly into accept, adapt,
or trigger decisions. Vendored MIT code retains its license and attribution.

The provider-adapter crate that implements the accepted port arrives with the
model-call slices, not with this record. Until then, runtime work can proceed as
an isolated spike whose local interfaces are explicitly throwaway, which keeps
the port review inside the orchestration record where it belongs.

## Scenario walkthroughs

- **S02:** Layer 2 prepares and authorizes a durable call under its accepted
  lifecycle, then hands the runtime one operation carrying the caller-owned
  `ModelCallId`. The runtime performs at most one provider interaction, streams
  observations correlated to that identity, and reports typed terminal evidence;
  Layer 2 classifies and commits under ADR-0043 and ADR-0042. No runtime
  component retries, reorders, or reinterprets the outcome.
- **S04:** After a crash, startup recovery classifies the issued call from
  durable state and evidence under the accepted records. The runtime holds no
  durable state to recover and is not consulted; whether its process-local
  observations were lost changes nothing about authority.

## Open questions

- The Phase-0 audit's outcome: which SerdesAI crates, if any, are vendored;
  which are replaced; and the exact crate decomposition and names of the runtime
  layer.
- The accepted shape of the application-side model-execution port remains with
  the model-call orchestration record and its slices.
- The app-facing SDK's identity, schema-versioning, and workflow decisions
  remain future records, gated as above.

## Explicit non-decisions

This record adds no code, crate, manifest, or dependency by itself; vendoring
any SerdesAI crate is a later recorded decision under the repository dependency
gate, informed by the Phase-0 audit. It does not define the model-execution
port, select a provider, SDK, or HTTP client, decide provider-identity
normalization (reserved ADR-0007), fallback (reserved ADR-0006), tool execution
(reserved ADR-0011 through ADR-0014), or any app-SDK surface. It changes no
semantics of ADR-0005, ADR-0042, ADR-0043, or ADR-0044.
