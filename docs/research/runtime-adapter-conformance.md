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
  minimum test matrix
- Intended use: reusable body for the subscription-runtime tracks in the
  [backlog](../agents/backlog.md); pairs with the
  [Codex CLI subscription protocol notes](codex-cli-subscription-protocol.md)

File references below are repo-relative `crate/path:line` as verified on the
study date; line numbers drift with the tree.

## 1. Stability verdict

**Verdict: STABLE for the method signatures; the evidence vocabulary grows
additively. A pure adapter written today against the current trait is very
unlikely to need structural reshaping. Rate: LOW reshape risk on the trait
surface, LOW–MODERATE on the evidence enums (new variants may appear, but they
are additive, and a `match` written today keeps compiling if you avoid
over-tight exhaustiveness assumptions).**

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
  `signalbox-model-runtime` plus transport/serde. So even if the vocabulary
  grows, an adapter's blast radius is confined to its own crate.

**Residual risk (the honest caveats):**

- `type Prepared: Send` and the `impl Future … + Send` return positions mean the
  trait uses RPITIT/AFIT; a future MSRV move or a decision to box futures could
  nominally touch signatures, but nothing in the log suggests it.
- The bridge fixes `C = ModelCallId` and translates evidence to disposition in
  `classify_terminal` (`model-provider-runtime/src/lib.rs:444`). If a *new*
  evidence variant is added, that table must gain an arm — but that is the
  wiring layer's job, not the adapter's.
- The maintainers have flagged (in `model-runtime-openai/src/runtime.rs:1-6`)
  that once a third adapter exists they may extract a shared transport crate.
  That would be a *refactor of duplication*, not a trait change — a new adapter
  written today would just be the thing that motivates it, and would still
  compile.

## 2. Adapter-author checklist (build order)

Build a new adapter as a self-contained crate
`crates/model-runtime-<provider>/`. Depend **only** on
`signalbox-model-runtime = { path = "../model-runtime" }` plus `reqwest`
(default-features off; `rustls-tls-native-roots`, `stream`), `serde`,
`serde_json` (with `raw_value`), and `futures-util`. Copy the Anthropic crate as
the skeleton; consult the OpenAI crate where the provider's wire shape differs.
Exemplar paths below are in `crates/model-runtime-anthropic` unless noted.

01. **`Config` struct** — `base_url`, optional
    `connect_timeout`/`exchange_timeout`, `sse_record_limit` default 8 MiB.
    Carries **no credential**. Contract: reject a zero SSE limit; validate the
    base URL (http/https only, no query/fragment/user-info) at construction.
    Exemplar: `src/config.rs`.
02. **Wire serde structs** — request body plus response/stream/error
    deserialization. Response structs tolerant of unknown fields. Contract: the
    request mirrors the provider's schema; keep tool-call `arguments_json` as
    verbatim raw JSON (`Box<RawValue>`), never re-serialized. Exemplar:
    `src/wire.rs`.
03. **`translate.rs`:
    `build_request(&operation) -> Result<WireRequest, PreparationFailure>`** —
    the sole translation entry. Contract: FIRST call `operation.validate()`
    (core provider-neutral rules). Then adapter validation →
    `PreparationFailure::UnsupportedOperation` for anything unrepresentable
    (e.g. replayed reasoning a provider cannot carry). Send exactly
    `resolved_target.as_str()` as the model. Realize `output_contract` as a
    **forced single tool call** (`disable_parallel_tool_use` /
    `parallel_tool_calls: false`) — not native `response_format` — so core
    `decode_structured` applies unchanged. Exemplar: `src/translate.rs`
    (`tools_and_choice`); the OpenAI `translate.rs:231` shows the
    forced-function divergence.
04. **`status.rs`: two exhaustive single-`match` classifiers** — HTTP status →
    `ProviderErrorKind`, and native error token → `ProviderErrorKind`, both with
    an `Unrecognized` arm. Contract: HTTP **401 → `CredentialRejected` takes
    precedence** over any contradictory body token. Mutually exclusive, total.
    Exemplar: `src/status.rs` (`classify_error_status`, `classify_error_token`,
    `classify_error`).
05. **`response.rs`: `decode_buffered_response(...)`** — a pure map of a
    buffered success body to `TerminalEvidence`. Contract: success is **exactly
    HTTP 200**. Emit observations in order: `ProviderModelReported` → per-tool
    `ToolCallProposed` → `UsageReported` → `FinishReported`. A 200 body that
    does not parse as completion material →
    `BoundaryLoss(ResponseUnintelligible)`, never silent success. Refusal finish
    → `Refused`. Exemplar: `src/response.rs`.
06. **`stream.rs`: `StreamDecoder`** driving the shared `SseFraming` (from
    `model-runtime`). Contract: do NOT write your own SSE framer — push
    transport chunks into `SseFraming::new(sse_record_limit)`, apply each
    `SseRecord` to your decoder. Require the provider's terminal marker
    (Anthropic `message_stop`; OpenAI literal `[DONE]`); EOF without it →
    `BoundaryLoss(StreamEndedWithoutTerminalMarker)`. Protocol violations →
    `BoundaryLoss(StreamProtocolViolation)`. Recheck cancellation between
    coalesced records. Exemplar: `src/stream.rs`; the OpenAI `stream.rs` for a
    `[DONE]`-style protocol.
07. **`Prepared` capability struct** — holds the built `reqwest::Request`, a
    cloned `Client`, execution settings, and the captured `CredentialValue`.
    Contract: `#[must_use]`; implement **neither** `Clone`, `Debug`, nor
    serialization (opaque one-shot). Exemplar: `AnthropicPreparedRequest<C>` in
    `src/runtime.rs:59`.
08. **`Runtime::new(config, credentials) -> Result<Self, ConstructionError>`**
    with the transport-discipline triad. Contract:
    `Client::builder().redirect(Policy::none()).retry(reqwest::retry::never()).pool_max_idle_per_host(0)`;
    timeouts applied only if the caller set them. (No-idle-reuse is what
    licenses `ProvenUnsent(ConnectFailed)`.) Exemplar: `src/runtime.rs:198-207`.
09. **`impl<C: Clone + Send + Sync, A: CredentialAccess> ModelRuntime<C> for Runtime<A>`
    — `prepare`.** Contract order: clone correlation → `build_request` (Failed
    on err) → serialize (`Defect::SerializationFailed` on err) →
    `credentials.resolve(&op.credential_reference)` **raced against
    cancellation** (Cancelled if the signal wins;
    `Failed::CredentialUnavailable` on err) → credential-to-sensitive-header
    (`HeaderValue::set_sensitive(true)`; `Failed::CredentialUnusable` if
    unusable) → build request (`Defect::RequestConstructionFailed` on err) →
    `Prepared`. Resolve the credential **exactly once per request** (no caching,
    so rotation stays visible). Never log; never redact the reference (it is
    non-secret). Exemplar: `src/runtime.rs` (`prepare_request`).
10. **`execute`.** Contract: check `already_fired(cancellation)` →
    `ProvenUnsent(CancelledBeforeSend)`. Emit `SendCommenced` **before**
    `client.execute`. Emit
    `ExchangeEstablished(ExchangeFacts { provider_request_id, http_status })`
    after headers. Race the send against cancellation; classify send errors
    (connect → `ProvenUnsent`, timeout/other → `BoundaryLoss`). Wrap the sink in
    a **redacting sink** and run **`redact_evidence`** with the captured
    credential so any provider-reflected key is scrubbed (INV-035). Dispatch
    buffered/streamed on `DeliveryMode`. Thread the correlation `C` onto every
    observation and the `TerminalReport` verbatim. Exemplar: `src/runtime.rs`
    (`execute`, `exchange`, `RedactingObservationSink`, `redact_evidence`).

**Cross-cutting contracts (must all hold):**

- One operation → at most one physical request. No retry, no fallback, no
  redirect-follow, no stream resume.
- Failures are typed evidence, never panics/exceptions. Every path returns a
  `TerminalReport`.
- `prepare` does all work reachable without provider traffic; `execute` does no
  second credential access or preparation.
- INV-035: never `Display`/serialize/log `CredentialValue`; its `Debug` is
  `[REDACTED]`; access errors carry only the reference plus a failure class.

## 3. Clean split — adapter PR vs. wiring PR

### A new-adapter PR owns (its own crate only)

- The new crate `crates/model-runtime-<provider>/` (all of §2), depending only
  on `signalbox-model-runtime` plus transport/serde.
- Its own loopback tests (§4).
- One line in the workspace `members` list in the root `Cargo.toml` (so it
  builds in-workspace).

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
  to `http://127.0.0.1:<port>`. This exercises the *true* reqwest path —
  headers, redirect discipline, connect failure, SSE framing —
  deterministically. (`crates/model-runtime-anthropic/tests/loopback.rs`,
  `CannedServer`; OpenAI mirror in
  `crates/model-runtime-openai/tests/loopback.rs`.)
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

- Buffered completion end-to-end.
- Streamed completion end-to-end.
- Each `ProviderErrorKind`, including `CredentialRejected` by both 401 status
  and native token, with 401 precedence.
- Refusal.
- Tool-call proposal (buffered and streamed) with verbatim `arguments_json`.
- Structured-output forced-tool decode.
- Boundary-loss cases: redirect not followed, truncated body, stream without
  terminal marker, connect refused.
- Pre-send cancellation proving zero requests.
- INV-035 redaction of a provider-reflected credential.
