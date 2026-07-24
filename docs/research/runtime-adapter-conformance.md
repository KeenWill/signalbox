# ModelRuntime adapter conformance template

> Dated research intake (2026-07-23), non-normative. This page records study
> findings as input to future adapter work; it states no requirements. Decisions
> live in the [decision ledger](../decisions.md); current requirements live in
> the [living specification](../spec/README.md) — in particular the
> [model-runtime substrate](../spec/runtime-substrate.md) page — which
> supersedes anything stated here.

- Date: 2026-07-23
- Status: research intake, proposal-grade input only; where this document and
  the [runtime-substrate spec](../spec/runtime-substrate.md) disagree, the spec
  wins
- Studied surface: `crates/model-runtime` (trait and evidence vocabulary),
  `crates/model-runtime-anthropic`, `crates/model-runtime-openai`, and the
  `crates/model-provider-runtime` bridge, read at the tree current on the study
  date (trait crate and both adapters `cargo check` clean together)
- Scope: a stability assessment of the `ModelRuntime` trait surface, a
  build-order conformance checklist for authors of new adapter crates, the
  adapter-PR versus wiring-PR split, and the loopback test pattern with a
  minimum test matrix. Per the one-owning-source rule in
  [AGENTS.md](../../AGENTS.md), the checklist names each obligation and cites
  its owning spec section or invariant row instead of restating the requirement,
  so reuse of this page cannot age into a divergent second contract
- Intended use: reusable body for the subscription-runtime tracks in the
  [backlog](../agents/backlog.md); pairs with the
  [Codex CLI subscription protocol notes](codex-cli-subscription-protocol.md)

File references below are repo-relative `crate/path:line` as verified on the
study date; line numbers drift with the tree.

## 1. Stability verdict

**Verdict: STABLE for the method signatures; the evidence vocabulary grows
additively. A pure adapter written today against the current trait is very
unlikely to need structural reshaping. Rate: LOW reshape risk on the trait
surface, LOW–MODERATE on the evidence enums — new variants may appear, but they
are additive, and the two enums differ in blast radius: `ProviderErrorKind`
growth leaves existing adapters untouched, while `TerminalEvidence` growth is a
compiler-guided arm addition in every adapter (detail in the residual-risk list
below).**

### Evidence

At the study date the runtime layer was about three days old (first
`model-runtime/src` commit 2026-07-20; head 2026-07-22). Within that window:

- **The trait's method signatures have been byte-stable since 2026-07-21.**
  `crates/model-runtime/src/runtime.rs` has exactly 4 commits. Only ONE
  (`e953834`, "separate request preparation from execution") changed the actual
  shape: it split a single-stage `execute(operation, …)` into the current
  two-stage `prepare(operation) -> PreparationOutcome<C, Self::Prepared>` +
  `execute(prepared, …)` with the associated `type Prepared: Send`. The other
  two post-creation commits are pure doc edits: `46de85b` renumbered a record
  reference (1 line), `b0d3292` rewrote doc comments to point at `docs/spec/*`
  paths (only comment lines changed; a grep for changed `enum`/`struct`/variant
  lines returned nothing).
- **That one structural change is already absorbed by both in-repo
  conformances.** The split reshaped the Anthropic adapter significantly (+440
  lines in its `runtime.rs`) as a one-time event, and the OpenAI adapter
  (created 2026-07-20) was likewise brought into conformance. Both now sit on
  the two-stage surface. There is no pending redesign visible in the log.
- **Evidence growth is additive, not churn.** `evidence.rs` totals **522
  insertions / 80 deletions** over its whole life; the deletions are
  concentrated in doc-comment rewrites, not variant removal or rename. New
  vocabulary arrives as new closed-enum arms:
  `ProviderErrorKind::QuotaExhausted` was *added* in `6ab75af` ("add
  credential-unavailable and quota-exhausted vocabulary"); `CredentialRejected`
  has existed since the first commit. The design explicitly anticipates this
  ("each adapter owns an exhaustive, mutually exclusive native mapping" with an
  `Unrecognized` catch-all arm), so adding a provider condition later is a
  localized change.
- **The trait crate is provider-neutral by construction** (verified: no
  `anthropic`/`openai`/`reqwest`/HTTP-client imports; the only "http" tokens are
  the neutral `ExchangeFacts.http_status` field and
  `LossCause::UnexpectedHttpStatus`). Adapters depend only on
  `signalbox-model-runtime` plus transport/serde. So even as the vocabulary
  grows, an adapter's obligations stay expressible inside its own crate.

**Residual risk (the honest caveats):**

- `type Prepared: Send` and the `impl Future … + Send` return positions mean the
  trait uses RPITIT/AFIT; a future MSRV move or a decision to box futures could
  nominally touch signatures, but nothing in the log suggests it.
- The two evidence enums grow differently, and a stability estimate must not
  conflate them. A new `ProviderErrorKind` touches neither the bridge — its
  `classify_terminal` (`model-provider-runtime/src/lib.rs:444`) classifies
  `ProviderError(_)` without reading the kind — nor any adapter that does not
  map to it, because adapters only *construct* kinds through classifiers whose
  `Unrecognized` arm absorbs unmapped material. A new `TerminalEvidence` variant
  (or nested cause), by contrast, is a workspace-wide compiler-guided edit: the
  bridge's `classify_terminal` must gain an arm, and so must **every adapter's**
  `redact_evidence`, which matches the evidence exhaustively with no wildcard
  (`model-runtime-anthropic/src/runtime.rs:1205`; OpenAI mirror). Mechanical arm
  additions, not reshapes — but they land in every adapter crate, not only the
  wiring layer.
- The maintainers have flagged (in `model-runtime-openai/src/runtime.rs:1-6`)
  that once a third adapter exists they may extract a shared transport crate.
  That would be a *refactor of duplication*, not a trait change — a new adapter
  written today would just be the thing that motivates it, and would still
  compile.

## 2. Adapter-author checklist (build order)

Everything a conforming adapter must satisfy is owned by the
[runtime-substrate spec](../spec/runtime-substrate.md) and the
[invariant catalog](../invariants.md); this checklist restates none of it. What
the study adds is the build order — which file to write first, what each file's
single job is, and which exemplar to copy — with each step citing the owning
section for its rules.

Build a new adapter as a self-contained crate
`crates/model-runtime-<provider>/`. Its `[dependencies]` are
`signalbox-model-runtime = { path = "../model-runtime" }` plus transport and
serde only. Concretely, copy the exemplar manifest
(`crates/model-runtime-anthropic/Cargo.toml`): `reqwest` with default features
off and exactly the `rustls-no-provider` and `stream` features (the
workspace-pinned reqwest 0.13.4 exposes no `rustls-tls-native-roots` feature), a
direct `rustls` dependency enabling `ring`, `std`, and `tls12` — the
crypto-provider selection required by the TLS rules in
[Provider adapters](../spec/runtime-substrate.md#provider-adapters) — plus
`serde` (derive), `serde_json` (with `raw_value`), and `futures-util`. Copy the
Anthropic crate as the skeleton; consult the OpenAI crate where the provider's
wire shape differs. Exemplar paths below are in `crates/model-runtime-anthropic`
unless noted.

01. **`Config` struct** — the exemplar carries `base_url`, an optional
    `connect_timeout`, a required `exchange_timeout`, and `sse_record_limit`,
    and no credential. Base-URL admission (HTTPS, with the sole plain-HTTP
    exception for a literal loopback IP host), the required positive
    whole-exchange timeout and its default, and the positive-limit rules are
    owned by
    [Provider adapters](../spec/runtime-substrate.md#provider-adapters);
    validate them at construction as the exemplar does. Exemplar:
    `src/config.rs`, with the construction checks in `src/runtime.rs`.
02. **Wire serde structs** — hand-rolled request body plus response/stream/error
    deserialization; no provider SDK. Unknown-field tolerance and the bounds
    that apply to it:
    [Provider adapters](../spec/runtime-substrate.md#provider-adapters).
    Verbatim `Box<RawValue>` tool-call arguments, never re-serialized:
    [Structured output and tool decode](../spec/runtime-substrate.md#structured-output-and-tool-decode).
    Exemplar: `src/wire.rs`.
03. **`translate.rs`:
    `build_request(&operation) -> Result<WireRequest, PreparationFailure>`** —
    the sole translation entry. Call `operation.validate()` before any
    adapter-specific checks, then map anything the provider cannot represent to
    `PreparationFailure::UnsupportedOperation`. The provider-neutral validation
    rules and the resolved-target-as-model rule:
    [The operation](../spec/runtime-substrate.md#the-operation). The
    forced-single-tool-call realization of the output contract:
    [Structured output and tool decode](../spec/runtime-substrate.md#structured-output-and-tool-decode).
    Exemplar: `src/translate.rs` (`tools_and_choice`); the OpenAI `translate.rs`
    shows the forced-function divergence.
04. **`status.rs`: two exhaustive single-`match` classifiers** — HTTP status →
    `ProviderErrorKind` and native error token → `ProviderErrorKind`, each with
    an `Unrecognized` arm. The per-provider classification precedence (401
    first, then recognized native material, then status) is owned by
    [Provider adapters](../spec/runtime-substrate.md#provider-adapters).
    Exemplar: `src/status.rs` (`classify_error_status`, `classify_error_token`,
    `classify_error`).
05. **`response.rs`: `decode_buffered_response(...)`** — a pure map of a
    buffered success body to `TerminalEvidence`. Which status is success and
    what an unintelligible success body becomes:
    [Provider adapters](../spec/runtime-substrate.md#provider-adapters) and
    [Terminal evidence](../spec/runtime-substrate.md#terminal-evidence). The
    observation ordering the decode drives:
    [Observations](../spec/runtime-substrate.md#observations). Exemplar:
    `src/response.rs`.
06. **`stream.rs`: `StreamDecoder`** driving the shared `SseFraming` from
    `model-runtime` — do NOT write your own SSE framer. Framing guarantees and
    limit semantics: [SSE framing](../spec/runtime-substrate.md#sse-framing).
    The provider's terminal-marker protocol and every stream-integrity rule the
    decoder enforces: the per-provider stream-integrity paragraphs in
    [Provider adapters](../spec/runtime-substrate.md#provider-adapters). The
    decoder's record bound is only one of the response bounds: the same
    section's buffered-body and cumulative-stream caps apply before parsing and
    live in `runtime.rs` (`MAX_BUFFERED_RESPONSE_BYTES`,
    `MAX_STREAMED_RESPONSE_BYTES` in both exemplars), in addition to
    `SseFraming`'s per-record limit. Exemplar: `src/stream.rs`; the OpenAI
    `stream.rs` for a `[DONE]`-style protocol.
07. **`Prepared` capability struct** — holds the built `reqwest::Request`, a
    cloned `Client`, execution settings, and the captured `CredentialValue`. Its
    required properties (opaque, one-shot, non-cloneable, non-serializable):
    [Two-stage execution](../spec/runtime-substrate.md#two-stage-execution).
    Mark it `#[must_use]` as the exemplar does. Exemplar:
    `AnthropicPreparedRequest<C>` in `src/runtime.rs`.
08. **`Runtime::new(config, credentials) -> Result<Self, ConstructionError>`** —
    build the one `reqwest::Client` here. The transport discipline (no redirect
    following, no protocol retries, no idle-connection reuse, the timeout rules)
    and the TLS and no-proxy requirements are owned by
    [Provider adapters](../spec/runtime-substrate.md#provider-adapters);
    construction failure is a configuration defect, not operation evidence.
    Exemplar: client construction in `src/runtime.rs`.
09. **`prepare`** for the
    `impl<C: Clone + Send + Sync, A: CredentialAccess> ModelRuntime<C>`.
    Everything prepare must and must not do — all no-traffic work up front, the
    outcome vocabulary, per-request credential resolution and its failure
    typing, the work-first cancellation race — is owned by
    [Two-stage execution](../spec/runtime-substrate.md#two-stage-execution) and
    the
    [credential-access boundary](../spec/runtime-substrate.md#credential-access-boundary)
    (INV-035 in the [invariant catalog](../invariants.md)). The exemplar's
    internal order (clone correlation → `build_request` → serialize → resolve
    the credential raced work-first against cancellation → sensitivity-marked
    header → build the `reqwest::Request`) is a faithful sequencing of those
    rules to copy: `src/runtime.rs` (`prepare_request`, `with_cancellation`).
10. **`execute`** — consumes the capability; at most one provider interaction.
    Its obligations — observation emission and ordering, classifying send
    failures into proven-unsent versus boundary loss, the work-first
    cancellation bias, the refusal downgrade to
    `ProviderError { kind: Unrecognized }` when complete upload cannot be
    proven, and credential redaction of all provider-controlled output — are
    owned by
    [Two-stage execution](../spec/runtime-substrate.md#two-stage-execution),
    [Observations](../spec/runtime-substrate.md#observations),
    [Terminal evidence](../spec/runtime-substrate.md#terminal-evidence), and the
    [credential-access boundary](../spec/runtime-substrate.md#credential-access-boundary).
    The exemplar realizes them as `execute`/`exchange`, `with_cancellation`
    (work future polled first), `without_unproven_refusal`, and
    `RedactingObservationSink` + `redact_evidence` — copy all of them; none is
    optional. Exemplar: `src/runtime.rs`.

**Cross-cutting rules (owned elsewhere, all binding):** the
one-operation-one-physical-request rule and typed-evidence-never-exceptions
([Two-stage execution](../spec/runtime-substrate.md#two-stage-execution)); the
credential-hygiene rules (the
[credential-access boundary](../spec/runtime-substrate.md#credential-access-boundary);
INV-035 in the [invariant catalog](../invariants.md)). A goal prompt built from
this page should cite those sections rather than copy them: the copies age, the
sections do not.

## 3. Clean split — adapter PR vs. wiring PR

### A new-adapter PR owns (its own crate plus the records of it)

- The new crate `crates/model-runtime-<provider>/` (all of §2), depending only
  on `signalbox-model-runtime` plus transport/serde.
- Its own loopback tests (§4).
- One line in the workspace `members` list in the root `Cargo.toml` (so it
  builds in-workspace).
- The owning-spec update: [runtime-substrate](../spec/runtime-substrate.md)
  describes the implemented adapters, so the same PR updates its adapter
  coverage — or the bottom specification diff of the PR's stack supplies it —
  per the living-specification rule in [AGENTS.md](../../AGENTS.md). Enforcement
  columns in the [invariant catalog](../invariants.md) that gain the new crate's
  tests (INV-035 lists each existing adapter's loopback suite) are updated in
  the same change.

It does **NOT** touch: `model-provider-runtime` (the bridge is generic),
`crates/application` (the `ModelCallProvider` port is provider-agnostic),
`apps/hubd`, or any TOML config schema. Precedent: `crates/model-runtime-openai`
is a fully-authored adapter that is a workspace member but referenced nowhere
outside its own crate — the exact "adapter without wiring" shape.

### A separate wiring PR owns (composition/config)

The bridge `RuntimeModelCallProvider<R>`
(`crates/model-provider-runtime/src/lib.rs:227`) is **fully provider-generic** —
bounded only by `R: ModelRuntime<ModelCallId>`, contains zero per-provider code,
and needs no change. What the wiring PR must touch:

1. **`apps/hubd/Cargo.toml`** — add the `signalbox-model-runtime-<provider>`
   path dependency alongside the Anthropic one.
2. **`apps/hubd/src/main.rs`** `run_hub` — the composition root, today hardcoded
   `AnthropicRuntime::new(...)` wrapped by `RuntimeModelCallProvider::new(...)`.
   The Anthropic-specific credential wiring (`ANTHROPIC_CREDENTIAL_REFERENCE`,
   `FileCredentialAccess`, `ANTHROPIC_API_KEY_FILE`) also needs generalization.
3. **`apps/hubd/src/configuration.rs`** — the provider allow-list gate,
   currently a literal `!= "anthropic"` check →
   `HubModelConfigurationError::UnsupportedProvider`. Must admit the new
   provider string.
4. **`config/hubd.example.toml`** — add an example `[[models]]` stanza with the
   new `provider` value.

**Important caveat for the first second-provider wiring PR:** there is currently
**no provider enum / factory / registry**. Selection is two hardcoded points
(the `!= "anthropic"` string check and the single `AnthropicRuntime::new`). The
first real wiring PR must *introduce* the dispatch mechanism (enum plus
boxing/wrapper, since the `provider` value must resolve to one concrete type at
the `PostgresProviderModelExecution::new(...)` site in `main.rs`), not merely
add a match arm. Adapter PRs after that just slot into whatever mechanism that
PR establishes. This distinction is worth stating in a track's goal prompt so
the adapter author does not assume a registry exists.

## 4. Per-adapter test pattern to replicate

Both existing adapters use the identical harness — copy it verbatim into the new
crate's `tests/loopback.rs`.

- **Real local TCP server, not a mock-transport trait and not recorded HTTP
  fixtures.** `CannedServer::serving(responses: Vec<Vec<u8>>)` binds
  `127.0.0.1:0`, spawns a tokio task that accepts **one connection per queued
  canned response**, reads the full raw HTTP request (parsing `content-length`),
  records the raw request text into `Arc<Mutex<Vec<String>>>`, writes the canned
  bytes, and shuts down. Point the adapter at it by overriding `config.base_url`
  to `http://127.0.0.1:<port>` (the literal-loopback plain-HTTP admission of
  [Provider adapters](../spec/runtime-substrate.md#provider-adapters) exists for
  exactly this harness). This exercises the *true* reqwest path — headers,
  redirect discipline, connect failure, SSE framing — deterministically.
  (`crates/model-runtime-anthropic/tests/loopback.rs`, `CannedServer`; OpenAI
  mirror in `crates/model-runtime-openai/tests/loopback.rs`.)
- **No live calls / no real credential.** A `FixedKey` implementing
  `CredentialAccess::resolve` returns a canned key (`b"key_loop"`). Variants
  (`CountingKey`, `RotatingKey`, `UnavailableKey`, `EmptyKey`, `PendingKey`)
  cover the credential edges: resolve-exactly-once, rotation visible to the next
  prepare, unavailable → Failed, unusable → Failed.
- **"Exactly one physical request" is the load-bearing assertion.** Tests assert
  `server.recorded_requests().len() == 1` for a normal exchange, `== 0` for
  pre-send cancellation and preparation failures, and — critically — `== 1` on
  the redirect test even though a *second* canned response is queued (proving
  the 307/308 was not followed).
- **Feed canned bodies and SSE byte streams directly.** A
  `http_response(status_line, extra_headers, body)` helper frames a proper
  `HTTP/1.1` response with correct `content-length`; SSE tests pass raw
  `event:`/`data:` byte strings as the body. Hand-crafted raw bytes simulate a
  truncated body (declared `content-length` larger than bytes sent) and a stream
  cut before the terminal marker.
- **Connect failure** is simulated by binding then dropping the listener so the
  port refuses → assert `ProvenUnsent(ConnectFailed)`.
- **Per-concern unit tests** live inline (`#[cfg(test)] mod tests`) in
  `status.rs` (classification tables, ideally via `signalbox-expect-table`),
  `response.rs` (buffered decode plus finish-reason mapping), `stream.rs`
  (protocol-integrity cases driving the *real* `SseFraming` plus your decoder),
  and `runtime.rs` (INV-035 redaction across buffered content, streamed deltas,
  and fallback error bodies, plus the credential resolve-once / rotation
  behavior).

### Minimum test matrix every adapter must cover

Cases are grouped by the evidence taxonomy of
[Terminal evidence](../spec/runtime-substrate.md#terminal-evidence), which owns
each case's expected terminal variant:

- Completion: buffered end-to-end; streamed end-to-end; tool-call proposal
  (buffered and streamed) with verbatim `arguments_json`; structured-output
  forced-tool decode.
- Provider error: each `ProviderErrorKind` the adapter maps, including
  `CredentialRejected` via both the status route and the native-token route
  (precedence rules:
  [Provider adapters](../spec/runtime-substrate.md#provider-adapters)); refusal,
  asserting the spec-owned downgrade rather than surfaced `Refused` evidence.
- Proven unsent: pre-send cancellation proving zero requests; connect refused
  (`ProvenUnsent(ConnectFailed)` — the refusal proves no request byte was
  written).
- Boundary loss: redirect not followed, truncated body, stream ended without the
  terminal marker.
- Credential hygiene: INV-035 redaction of a provider-reflected credential.
