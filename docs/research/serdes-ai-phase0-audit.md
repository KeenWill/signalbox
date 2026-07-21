# SerdesAI Phase-0 audit

- Date: 2026-07-20
- Status: research intake, proposal-grade input only; where this document and a
  merged ADR disagree, the ADR wins
- Audited snapshot: `janfeddersen-wq/serdesAI` @
  `1424128b0c64d9c2403eb0896cde881777941669`, workspace version 0.2.6, MIT
- Assessed against: [ADR-0005](../decisions/0005-model-call-retry-semantics.md)
  (retry and acceptance-boundary rules) and
  [ADR-0043](../decisions/0043-provider-failure-classification.md)
  (failure-classification vocabulary)
- Scope: the ten research questions from the external exploration handoff,
  prioritized Q4, Q2, Q3, Q8, Q1, plus a retry-wrapper assessment. Audit
  coverage includes Anthropic + OpenAI adapters, streaming, typed structured
  output, and tool-call decoding; the smaller one-provider smoke implementation
  set is identified below. Embeddings, graphs, MCP, evals, and A2A were not
  audited.

File references below are `crate/path:line` at the pinned commit. Everything in
part (a) was read or executed directly in a local clone pinned to that commit.

## (a) Verified findings

### Build and test run

Toolchain: `rust-toolchain.toml` selects the moving `stable` channel; for this
audit that resolved to rustc 1.97.1 on macOS (aarch64). The manifests separately
declare MSRV `rust-version = "1.75.0"`. This run verifies the audited compiler,
not MSRV compatibility; Rust 1.75.0 was not tested.

`cargo test --workspace --no-fail-fast` compiled all 17 workspace crates and
exited 0:

| Suite                     | Result                                                         |
| ------------------------- | -------------------------------------------------------------- |
| Unit tests (17 crates)    | 1052 passed, 0 failed, 0 ignored                               |
| Doc-tests                 | 33 passed, 0 failed, 106 ignored (`ignore`-annotated examples) |
| Integration-test binaries | none exist in the workspace                                    |

Notable distribution: `serdes-ai-tools` 181, `serdes-ai-core` 154,
`serdes-ai-models` 105 (across 15 provider adapters, so per-adapter depth is
thin), `serdes-ai-agent` 90, `serdes-ai-streaming` 60. `serdes-ai-macros` has
zero unit tests. All provider-adapter tests are scripted/local; nothing calls a
live provider.

### Q1 — which crates work independently of `serdes-ai-agent`

Verified by building, not by manifest reading. A standalone consumer crate
outside the workspace was compiled and run against path dependencies
`serdes-ai-core`, `serdes-ai-models` (with
`default-features = false, features = ["anthropic", "openai"]`),
`serdes-ai-output`, `serdes-ai-tools`, and `serdes-ai-streaming`. It constructed
both target adapters, built a tool definition with schema, and set
structured-output request parameters. It compiled and ran; `cargo tree` on the
consumer shows eight SerdesAI workspace crates in the closure: `serdes-ai-core`,
`serdes-ai-macros`, `serdes-ai-models`, `serdes-ai-output`, `serdes-ai-retries`,
`serdes-ai-streaming`, `serdes-ai-tools`, `serdes-ai-toolsets`. The full cargo
closure also contains third-party dependencies; they were not enumerated for
this agent-independence question. Neither `serdes-ai-agent` nor
`serdes-ai-providers` appears.

Two consequences:

- The provider adapters live in `serdes-ai-models` (per-provider cargo features,
  `serdes-ai-models/Cargo.toml:14-33`), not in `serdes-ai-providers`, which is a
  separate registry/abstraction crate that is not required.
- The minimum usable SerdesAI workspace closure is still eight crates, because
  `serdes-ai-models` hard-depends on `serdes-ai-output`, `serdes-ai-retries`,
  `serdes-ai-streaming`, and `serdes-ai-tools` (which drags `toolsets` and
  `macros`) via `serdes-ai-models/Cargo.toml:36-40`. The retries crate is in the
  mandatory compile closure even when retry behavior is unused.

### Q4 — caller-supplied operation/run ID on every event (make-or-break)

Two distinct layers give two distinct answers.

**Model-trait layer (what Signalbox would consume):** `Model::request` and
`Model::request_stream` take `(messages, settings, params)` only — no identity
parameter of any kind (`serdes-ai-models/src/model.rs:122-138`). Stream items
are `ModelResponseStreamEvent` =
`PartStart | PartDelta | PartEnd | StreamComplete`, which carry a part index and
content but no run, operation, or request identifier
(`serdes-ai-core/src/messages/events.rs:17-31`). So no caller ID can appear *on*
events — but none is needed for correlation at this layer, because the caller
invokes one request and exclusively owns the returned stream. Under
[ADR-0005's](../decisions/0005-model-call-retry-semantics.md)
one-authorization-one-call model, Signalbox holds the stream for exactly one
`ModelCallId` and can tag every observation itself. Correlation is by
construction, not by field.

**Agent layer:** `AgentStreamEvent` carries `run_id` only on `RunStart` and
`RunComplete`; text/tool/thinking/usage/error events have no identity field
(`serdes-ai-streaming/src/events.rs:13-123`). The run ID is generated internally
at every entry point — `serdes-ai-agent/src/run.rs:198,284`,
`serdes-ai-agent/src/stream.rs:193,921`, via `generate_run_id()`
(`serdes-ai-agent/src/context.rs:167`, `serdes-ai-core/src/identifier.rs:41`,
`run_{uuid4}`). No public API accepts a caller-supplied run ID (repo-wide
search: the only `with_run_id` setters are on an error type and on the tools
`RunContext` used for tool execution, not on any agent entry point).

**Answer:** fails at the agent layer, moot at the models layer. The handoff's
fork trigger "runtime cannot accept caller-owned durable operation IDs on every
event" is real for the agent loop, so the agent loop is unusable for Signalbox
regardless of other findings; the models layer does not block.

### Q2 — provider-boundary-crossed / request-accepted signal

There is no explicit boundary observation anywhere. What exists:

- **Success path (usable implicit signal):** both adapters build and send the
  HTTP request inside the method and check status before returning
  (`serdes-ai-models/src/anthropic/model.rs:598-623` non-streaming,
  `serdes-ai-models/src/anthropic/model.rs:643-671` streaming; equivalent
  structure in `serdes-ai-models/src/openai/chat.rs`). A returned
  `Ok(StreamedResponse)` therefore proves a success-status HTTP response header
  was received — the provider accepted the request. In streaming, Anthropic's
  `message_start` then confirms provider-side message creation
  (`serdes-ai-models/src/anthropic/stream.rs:236-242`).
- **Failure path (no signal):** every transport failure funnels through
  `From<reqwest::Error> for ModelError`
  (`serdes-ai-models/src/error.rs:454-470`), which maps `is_timeout()` →
  `Timeout(30s hardcoded)`, `is_connect()` → `Connection`, else `Other`. Nothing
  records whether the request body write completed —
  [ADR-0043's](../decisions/0043-provider-failure-classification.md) decisive
  full-request-send boundary is not observed. A pre-connect failure (provably
  unsent under that ADR) and a post-send connection loss (must be `Ambiguous`)
  can surface as the same `ModelError::Connection` / `ModelError::Timeout`
  values.
- The `RequestPrepared` vs `ProviderBoundaryCrossed` distinction the handoff
  sketches does not exist and cannot be added by wrapping: request construction
  and `send()` are fused inside each adapter method. The HTTP client is
  injectable (`serdes-ai-models/src/anthropic/model.rs:79`,
  `serdes-ai-models/src/openai/chat.rs:85` `with_client`), so a
  middleware-instrumented `reqwest::Client` could observe connect/write phases
  out-of-band, but correlating that side channel to a specific logical call from
  outside the adapter is fragile.

**Answer:** acceptance is provable on the success path only. A trustworthy
boundary signal on failure paths is per-adapter surgery on the send/error code.

### Q3 — error evidence vs ADR-0043's categories

The canonical classification is `ModelFailureKind`
(`serdes-ai-core/src/errors.rs:13-46`) reached via
`ClassifyModelFailure::model_failure()`
(`serdes-ai-models/src/error.rs:332-451`). It is a *retryability* taxonomy, not
an *evidence* taxonomy. Mapping against
[ADR-0043's](../decisions/0043-provider-failure-classification.md) dispositions:

| ADR-0043 category                         | SerdesAI representation                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   | Evidence preserved?                                                                                    |
| ----------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| `Completed`                               | `ModelResponse` with `finish_reason`, `usage`, `vendor_id` (`serdes-ai-core/src/messages/response.rs:14-37`); streaming `StreamComplete` only on provider-confirmed terminal (`serdes-ai-core/src/messages/events.rs:673-687`)                                                                                                                                                                                                                                                                                                            | Yes on the Anthropic path; OpenAI chat streaming cannot distinguish completion from truncation (below) |
| `KnownFailed` (proven unsent)             | Not distinguishable. `Connection`/`Timeout`/`Network` variants do not record send progress (`serdes-ai-models/src/error.rs:454-470`)                                                                                                                                                                                                                                                                                                                                                                                                      | No — the load-bearing gap                                                                              |
| `KnownFailed` (definitive provider error) | Good: `ModelError::Provider { provider, code, message, kind, status, retry_after }` retains native code and status (`serdes-ai-models/src/error.rs:37-50`); Anthropic native-code table at `serdes-ai-models/src/anthropic/error.rs:14-27`                                                                                                                                                                                                                                                                                                | Yes for typed HTTP/SSE error responses                                                                 |
| `Refused`                                 | Absent as a category. OpenAI refusal payloads become `ModelError::ContentFiltered` (`serdes-ai-models/src/openai/chat.rs:380-382`, `serdes-ai-models/src/openai/responses.rs:1015-1016`), which classifies as `ModelFailureKind::Other` (`serdes-ai-models/src/error.rs:441-443`). An Anthropic `refusal` stop reason falls through `_ => FinishReason::Stop` and is silently reported as normal completion (`serdes-ai-models/src/anthropic/model.rs:516-522`, `serdes-ai-models/src/anthropic/stream.rs:413-421`)                       | No; the Anthropic case is actively misleading                                                          |
| `Cancelled`                               | `ModelError::Cancelled` and `ModelFailureKind::Cancelled` exist (`serdes-ai-models/src/error.rs:84-85`), but no adapter constructs them from provider evidence; there is no cancellation-token input on the `Model` trait                                                                                                                                                                                                                                                                                                                 | Type exists, evidence path does not                                                                    |
| Premature EOF                             | Anthropic: excellent — EOF before `message_stop` → `IncompleteStream` (`serdes-ai-models/src/anthropic/stream.rs:174-188`), `message_stop` with open blocks → error (`serdes-ai-models/src/anthropic/stream.rs:371-380`), malformed event → error (`serdes-ai-models/src/anthropic/stream.rs:224-233`). OpenAI chat: absent — EOF without `[DONE]` ends the stream silently as if successful (`serdes-ai-models/src/openai/stream.rs:125-146`), malformed chunks are logged and dropped (`serdes-ai-models/src/openai/stream.rs:323-326`) | Anthropic yes, OpenAI no                                                                               |
| `Ambiguous`                               | No representation. Worse, the conditions [ADR-0043](../decisions/0043-provider-failure-classification.md) classifies as `Ambiguous` (timeout after send, connection loss, incomplete stream) are precisely what `is_transient()` marks retryable (`serdes-ai-core/src/errors.rs:57-72`: `Timeout`, `Connection`, `IncompleteStream` are all transient)                                                                                                                                                                                    | No — the taxonomy's polarity is inverted relative to ADR-0043                                          |

**Answer:** definitive provider error responses preserve enough typed evidence
to build [ADR-0043's](../decisions/0043-provider-failure-classification.md)
native-status mapping on top. Proven-unsent vs ambiguous is not recoverable from
the error types, refusal is collapsed or mislabeled, and the built-in
retryability semantics contradict that ADR's rule that SDK "transient" labels
never authorize repetition.

### Retry wrapper vs ADR-0005

`RetryingModel` (`serdes-ai-models/src/retry.rs`) is a decorator; the adapter
source performs one `.send()` per call
(`serdes-ai-models/src/anthropic/model.rs:616,659`), and nothing wraps models in
it implicitly. That establishes only the source-level wrapper behavior, not one
physical HTTP interaction. The injectable `reqwest::Client` may follow redirects
or carry other transport policy beneath `.send()`, and this audit did not verify
those lower layers. Signalbox compliance therefore requires a client built with
redirects disabled and an audit of any lower transport retry policy; omitting
`RetryingModel` alone does not satisfy
[ADR-0005](../decisions/0005-model-call-retry-semantics.md). If used:

- **Is every physical attempt observable?** No. The executor
  (`serdes-ai-retries/src/executor.rs:34-106`) exposes no per-attempt hook,
  callback, or event — only `tracing` debug/warn lines and a final attempt count
  inside `RetryExhausted` / `RetryDeadlineExceeded`
  (`serdes-ai-models/src/error.rs:127-148`). A successful second attempt is
  indistinguishable from a first-attempt success at the API surface. This fails
  [ADR-0005's](../decisions/0005-model-call-retry-semantics.md) requirement that
  every authorization to attempt a provider interaction be a distinct durable
  call.
- **Can retries be provably confined to a pre-send boundary?** No. The
  classifier retries anything `is_retryable()`
  (`serdes-ai-models/src/retry.rs:61-69`), which includes post-send evidence
  classes (`Server` 5xx responses, `Timeout`, `Connection`, `IncompleteStream`).
  For streams it retries when the *first stream item* is an error
  (`serdes-ai-models/src/retry.rs:133-153`) — "no caller-visible output yet" is
  a delivery fact, not proof the provider never accepted the request. A
  first-event connection reset after a fully sent request is
  [ADR-0043](../decisions/0043-provider-failure-classification.md) `Ambiguous`,
  and `RetryingModel` would silently re-send it — precisely
  [ADR-0005's](../decisions/0005-model-call-retry-semantics.md) prohibited
  duplicate-risk path. The suppression guard after visible output
  (`serdes-ai-models/src/retry.rs:461-496` test) limits duplication of
  *observed* output only.

The separate `FallbackModel` has the same shape: it stops falling back only
after a caller-visible stream event (`serdes-ai-models/src/fallback.rs:64`),
which is also not an acceptance boundary. Both wrappers must stay out of
Signalbox provider paths; because both are opt-in, that is a usage rule, not a
fork requirement.

### Q5 — where provider evidence lives

- Non-streaming:
  `ModelResponse { model_name, finish_reason, usage, vendor_id, vendor_details }`
  (`serdes-ai-core/src/messages/response.rs:14-37`). Both target adapters
  populate `vendor_id` from the provider message/completion ID
  (`serdes-ai-models/src/anthropic/model.rs:539`,
  `serdes-ai-models/src/openai/chat.rs:433`) and `model_name` from the
  provider-reported model — the raw material for
  [ADR-0005's](../decisions/0005-model-call-retry-semantics.md)
  `ProviderTargetObservation`.
- Usage: `RequestUsage` includes cache-creation/read token fields, populated by
  Anthropic (`serdes-ai-models/src/anthropic/model.rs:524-531`).
- Streaming (Anthropic): terminal `StreamComplete` carries finish reason plus
  input/output/cache token counts
  (`serdes-ai-models/src/anthropic/stream.rs:385-398`). But `message_id` and
  provider-reported `model` from `message_start` are stored in private parser
  fields with no accessor (`serdes-ai-models/src/anthropic/stream.rs:30-31`) and
  the parser is boxed into the type-erased `StreamedResponse`
  (`serdes-ai-models/src/anthropic/model.rs:668-671`), so in streaming mode the
  provider message ID and reported model identity are unobservable by any
  consumer. Target-mismatch detection during streaming —
  [ADR-0005's](../decisions/0005-model-call-retry-semantics.md) timing-sensitive
  rule — is currently impossible without modifying the parser.
- Streaming (OpenAI chat): `finish_reason` is consumed only to emit `PartEnd`
  events (`serdes-ai-models/src/openai/stream.rs:292-320`); usage is requested
  (`include_usage: true`, `serdes-ai-models/src/openai/chat.rs:360`) but the
  parser never reads chunk usage, and no `StreamComplete` is emitted (zero
  references in `serdes-ai-models/src/openai/stream.rs`). Finish reason, usage,
  and terminal confirmation are all dropped on the floor.
- HTTP `request-id` response headers are read nowhere in either adapter (only
  `retry-after` is parsed, `serdes-ai-models/src/anthropic/model.rs:545-551`).

### Q6 — extending structured-output validation

`serdes-ai-output` is agent-independent (verified in the Q1 build). Failure
classes are typed and roughly match the handoff's sketch: `OutputParseError`
distinguishes JSON-syntax (`JsonParse`/`NotJson`/`NoJsonFound`) from
schema-shape (`MissingField`/`InvalidField`/`UnexpectedTool`)
(`serdes-ai-output/src/error.rs:7-67`); `OutputValidationError` adds the domain
layer with an explicit retry-request channel
(`Failed { retry } | ModelRetry | Parse`,
`serdes-ai-output/src/error.rs:117-134`). Application validators are a public
extension point: `OutputValidator<T, Deps>` trait plus `ValidatorChain`
(`serdes-ai-output/src/validator.rs:17,67`). Parsing/validation itself performs
no model call; repair-by-re-prompt happens only in the agent loop (below), so
using this crate directly keeps repair a separate, explicitly authorized call as
the handoff's contract requires. Derives exist for schema generation
(`#[derive(OutputSchema)]`, `serdes-ai-macros/src/lib.rs:116`).

### Q7 — agent-loop continuation decisions

For completeness (Signalbox would not use this loop): each `step()` issues a
model request (`serdes-ai-agent/src/run.rs:388-392`); tool calls are executed
immediately and take priority over text output
(`serdes-ai-agent/src/run.rs:476-481`); output validation failure appends a
retry prompt and re-enters the loop up to `max_output_retries`
(`serdes-ai-agent/src/run.rs:495-503`) — i.e., hidden repair model calls with no
external authorization, conflicting with
[ADR-0005](../decisions/0005-model-call-retry-semantics.md). Failed tools are
retried in an inner loop up to a per-tool `max_retries`
(`serdes-ai-agent/src/run.rs:594-601`); that is a tool-lifecycle issue, not a
model-call retry. The loop exposes no Signalbox `ToolAttemptId` or independently
authorized, fenced dispatch for each execution as required by
[ADR-0009](../decisions/0009-dispatch-fencing.md). An OpenAI refusal surfaces as
a `ContentFiltered` error that aborts the run, and an Anthropic refusal is
invisible (Q3). The hidden model repair conflicts with
[ADR-0005](../decisions/0005-model-call-retry-semantics.md); tool re-execution
conflicts with the separate durable tool-attempt and dispatch lifecycle. This is
the expected result: the loop is the layer Signalbox replaces.

### Q8 — tool execution disabled, decoding and schemas retained

Yes, cleanly, by layer choice rather than by flag. Tool schema types and
definitions live in `serdes-ai-tools` (`ObjectJsonSchema`,
`serdes-ai-tools/src/definition.rs:16`; `ToolDefinition`,
`serdes-ai-tools/src/definition.rs:152-215`); definitions are passed to adapters
via `ModelRequestParameters.tools` (`serdes-ai-models/src/model.rs:22-23`);
adapters translate schemas outbound and decode provider tool-call payloads
inbound into `ToolCallPart { tool_name, args, tool_call_id }` parts and
streaming deltas (`serdes-ai-models/src/anthropic/stream.rs:256-264,299-313`;
Anthropic additionally validates accumulated tool-argument JSON at block end,
`serdes-ai-models/src/anthropic/stream.rs:340-359`). No execution machinery
exists at this layer — the executor lives in the agent crate and toolsets. The
Q1 consumer build proves the decode/schema path compiles and runs with the agent
crate absent. The tools crate also ships approval-flow types
(`DeferredToolCall`, `serdes-ai-tools/src/deferred.rs`) that are close in spirit
to Signalbox's ToolRequest-decode-without-execute contract.

### Q9 — stability and maintenance signals

From the pinned clone's history: 104 commits (88 non-merge) between 2025-12-27
and 2026-07-17; authors: Jan Feddersen 73 (two identities), acoliver 17, Sewer56
2, dependabot 12 — effectively two humans, one dominant. Repo-health facts from
the handoff (checked 2026-07-20, not re-verified here): 20 stars, 4 forks,
crates.io `serdes-ai` 0.2.6 published 2026-02-20 — five months behind repo
activity — with 2,105 lifetime downloads. In-repo signals verified directly:
README quick-start pins `serdes-ai = "0.1"` while the workspace is 0.2.6; no
integration-test binaries; no live-provider tests; per-adapter unit coverage
averages roughly seven tests across 15 adapters; `serdes-ai-macros` has zero
unit tests; the Anthropic streaming path received recent integrity-hardening
work (the pinned commit is that merge) that was not mirrored to the OpenAI chat
path. No API-stability policy or deprecation process is documented.

### Q10 — PydanticAI-derived behaviors: valuable vs conflicting

Valuable for Signalbox: the provider-neutral message/part vocabulary including
thinking, signatures, and redacted thinking (`serdes-ai-core/src/messages/`);
capability profiles with schema-transformer hooks
(`serdes-ai-models/src/profile.rs:40-83`); structured-output modes and typed
failure classes (Q6); tool schema and decode-only tool calls (Q8); the Anthropic
streaming-integrity pattern (`StreamComplete` + premature-EOF rejection) as a
design template; provider wire types and SSE parsing structure.

Conflicting with durable semantics: internal run-ID generation (Q4); the
retry/fallback wrappers' post-send repetition
([ADR-0005](../decisions/0005-model-call-retry-semantics.md));
retryability-first error taxonomy including `IncompleteStream`-is-transient
([ADR-0043](../decisions/0043-provider-failure-classification.md)); the agent
loop's hidden model repair and tool auto-retry (Q7); refusal collapsed into
errors or silence (Q3); usage-limit enforcement via in-process counters rather
than durable budgets.

## (b) Inferences (labeled as such)

- **Surgery cost.** Bringing the OpenAI chat streaming path to the Anthropic
  path's integrity level (terminal event, EOF detection, usage, malformed-chunk
  rejection), adding refusal and cancellation evidence, exposing streaming
  message ID/model identity, and re-basing error construction on a full-send
  boundary would rewrite most of the transport/error core of both adapters.
  Judgment, not measurement — but the affected regions
  (`serdes-ai-models/src/openai/stream.rs`,
  `serdes-ai-models/src/openai/chat.rs`, the `serdes-ai-models/src/anthropic/`
  error paths, and `serdes-ai-models/src/error.rs`) are the same files a
  from-scratch thin layer would consist of.
- **Upstreamability.** The changes Signalbox needs are semantic reversals
  (retryability polarity, ambiguity-first classification), not additions; with
  effectively one dominant maintainer and PydanticAI parity as the stated goal,
  upstream acceptance is unlikely, so vendored crates should be assumed to
  hard-fork immediately. Inference from Q9 signals; receptivity was not tested
  by filing an issue.
- **What is genuinely reusable.** Wire types
  (`serdes-ai-models/src/anthropic/types.rs`,
  `serdes-ai-models/src/openai/types.rs`), the SSE record parser
  (`serdes-ai-streaming/src/sse.rs`), the message/part vocabulary, and the
  profile/schema-transform design would transplant with little modification. The
  error, retry, and agent layers would not survive contact with the ADRs.

## (c) Recommendation

**Hand-roll a thin provider layer inside Signalbox, using SerdesAI as a design
reference and transplanting selected MIT-licensed fragments (wire types, SSE
record parsing, streaming-integrity pattern) with attribution — rather than
vendoring SerdesAI crates wholesale.**

"Depend on upstream releases" stays out on the maintenance signals (Q9:
effectively two contributors, stale crates.io release, no stability policy);
nothing in this audit rebuts that presumption, and the semantic-reversal
inference above makes it worse: Signalbox would need upstream to accept changes
that contradict the library's own retry/fallback design.

Why hand-roll beats vendoring selected crates:

1. The code Signalbox must trust most is exactly the code that needs rewriting.
   [ADR-0043](../decisions/0043-provider-failure-classification.md) compliance
   lives in the send/error/stream-terminal paths; those are deficient (Q2's
   unobservable boundary, Q3's inverted taxonomy, OpenAI's silent-truncation
   streaming) and rewriting them in a vendored tree is the same work as writing
   them in a Signalbox-owned module, minus the inherited surface.
2. Vendoring cannot be surgical. The minimum compiling closure is eight crates
   (Q1), importing the retry/fallback wrappers and retryability helpers whose
   semantics [ADR-0005](../decisions/0005-model-call-retry-semantics.md) and
   [ADR-0043](../decisions/0043-provider-failure-classification.md) prohibit.
   Keeping prohibited-but-present API surface (`is_retryable()`,
   `RetryingModel`, `FallbackModel`) inside the repo invites accidental use;
   deleting it is a fork with extra steps.
3. The parts worth keeping are small and stable. Wire structs, SSE framing, and
   the part vocabulary are a minority of the code and change slowly;
   transplanting them into Signalbox-owned modules captures most of the leverage
   at a fraction of the surface. The Anthropic stream parser's integrity
   discipline (`message_stop` gating, open-block validation, `IncompleteStream`
   on EOF) is the single best artifact to copy — as a pattern applied to both
   providers.

The models layer passed the make-or-break Q4 test (correlation by construction),
so vendoring `serdes-ai-core` + `serdes-ai-models` and rewriting in place is
*viable*; it is rejected on points 1–2, not on feasibility. If M3 implementation
reveals the hand-rolled wire layer ballooning past roughly the size of the
vendored closure's relevant code, revisit this choice — the audit evidence
supports either direction of that trade, and the decision belongs to the ADR
process.

### Implementation minimum and later audit coverage

The [real-provider smoke milestone](../target-model.md#priority-order) requires
one provider adapter, not both audited adapters or the later tool loop. The
actual minimum is `provider-core`, shared SSE framing, and one adapter;
`provider-anthropic` is the illustrative first choice below because its stream
integrity is the stronger audited design. OpenAI and schema/tool work remain in
the table to record Phase-0 audit coverage, but are not part of that smoke gate.
Names are illustrative.

| Module               | Milestone scope | Content                                                                                                                                                                                                                                                        | Design reference                                                                                                                 |
| -------------------- | --------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `provider-core`      | Smoke minimum   | Request/response message and part types, settings, usage incl. cache tokens, typed terminal evidence carrying [ADR-0043](../decisions/0043-provider-failure-classification.md) disposition + native status + `vendor_id` + reported model; caller-supplied IDs | `serdes-ai-core/src/messages/`, `serdes-ai-core/src/settings.rs`, `serdes-ai-core/src/usage.rs`                                  |
| shared SSE framing   | Smoke minimum   | Provider-agnostic SSE record parser with UTF-8/overflow/incomplete-record errors                                                                                                                                                                               | `serdes-ai-streaming/src/sse.rs`                                                                                                 |
| `provider-anthropic` | One adapter     | Request builder, exhaustive native-status classification, SSE parser with full-send observation, `message_start` identity surfacing, `message_stop`-gated terminal event, refusal handling                                                                     | `serdes-ai-models/src/anthropic/types.rs`, `serdes-ai-models/src/anthropic/stream.rs`, `serdes-ai-models/src/anthropic/error.rs` |
| `provider-openai`    | Later adapter   | Equivalent chat-completions shape: `[DONE]`/EOF distinction, finish-reason + usage surfacing, refusal payload as first-class evidence                                                                                                                          | `serdes-ai-models/src/openai/types.rs`, `serdes-ai-models/src/openai/stream.rs`, `serdes-ai-models/src/openai/chat.rs`           |
| `provider-schema`    | Later tool work | JSON-schema generation for output contracts and tools (evaluate `schemars` before writing derives), output parse/validation failure classes, application-validator hook                                                                                        | `serdes-ai-output/src/`, `serdes-ai-tools/src/definition.rs`, `serdes-ai-macros/src/lib.rs`                                      |

The smoke minimum is three modules and one provider. Full audited coverage is
four to five modules; neither set includes retry, fallback, agent-loop,
registry, or execution machinery. Rough size anchor (inference): the full
corresponding SerdesAI source is about 4–5k lines including tests, and the smoke
subset drops schema/tool work, media inputs, caching betas, and 14 of 15
providers.

## Sources

- Local clone of `janfeddersen-wq/serdesAI` at
  `1424128b0c64d9c2403eb0896cde881777941669` (build, test run, and all file:line
  citations)
- External exploration handoff, `rust-llm-runtime-signalbox-handoff.md`
  (2026-07-20): research-question charter and repo-health facts noted as
  unverified where used
- [ADR-0005](../decisions/0005-model-call-retry-semantics.md),
  [ADR-0009](../decisions/0009-dispatch-fencing.md), and
  [ADR-0043](../decisions/0043-provider-failure-classification.md)
