# Model-runtime substrate

This page specifies the Layer-1 typed model-runtime boundary as implemented in
`crates/model-runtime`, `crates/model-runtime-anthropic`, and
`crates/model-runtime-openai`, verified against `main` at commit `bf39f5f`. It
covers the provider-neutral operation, observation, and evidence vocabulary; SSE
framing; structured-output and tool decode; `ScriptedModel`; the two provider
adapters; and the in-process credential-access boundary. Layer-2 authorization
and evidence classification ([model-call-execution](model-call-execution.md)),
credential channels, delivery, and rotation discipline
([configuration-and-credentials](configuration-and-credentials.md)), and the
authoritative transcript commit
([sessions-and-transcript](sessions-and-transcript.md)) are owned by those
companion pages.

## Boundary and crate layout

The runtime layer is three library crates, hand-rolled per the 2026-07-20
[decision-ledger entry](../decisions.md) that closed ADR-0047's
vendor-versus-hand-roll question: one provider-neutral core crate plus
separately named provider adapters, with SerdesAI as a design reference only.
`signalbox-model-runtime` is the shared vocabulary; the Anthropic and OpenAI
adapter crates' only workspace `[dependencies]` entry is
`signalbox-model-runtime` (their dev-dependencies add the workspace test helper
`signalbox-expect-table`, which is test-only and ships in no built artifact).
`crates/domain`, `crates/application`, and `crates/persistence` declare no
dependency on any runtime crate, and no runtime type appears in a domain or
application signature (INV-002, INV-005); the approved runtime consumers are the
adapter crates, the `crates/model-provider-runtime` bridge — whose
`RuntimeModelCallProvider` implements the application's `ModelCallProvider` port
over any `ModelRuntime<ModelCallId>`, depending on both crates so the dependency
arrow points from the bridge into application, never from application into the
runtime — and the hub composition root (see Open edges). The Cargo manifest is
the enforcement mechanism: an undeclared dependency fails the workspace build.
Why: manifest-visible boundaries make a boundary violation a reviewable diff
instead of a silent import.

Caller identity crosses the boundary as an opaque correlation parameter `C`
threaded through `ModelOperation<C>`, every `Observation<C>`, and the final
`TerminalReport<C>`. No domain identifier type is imported or redefined; a
runtime-generated identity is never authoritative correlation. The runtime holds
no durable state, makes no lifecycle decisions, and performs no logging.

## The operation

`ModelOperation<C>` carries the correlation value, a non-secret
`CredentialReference`, the two caller-supplied target facts (`RequestedTarget`,
`ResolvedTarget`), optional system text, typed conversation history
(`ConversationMessage` with text, replayed tool calls, tool results, and signed
or redacted thinking parts), `ModelSettings` (required `max_output_tokens`;
optional temperature, top-p, stop sequences), declared `ToolDefinition`s, a
`ToolChoice` (automatic/any/named), an optional `StructuredOutputContract`, and
a `DeliveryMode` (buffered or streamed).

`ModelOperation::validate` rejects, before any send: duplicate ordinary tool
names, a named tool choice matching no declared tool, and an ordinary tool
colliding with the output contract's name. Why: the contract name is reserved so
a returned proposal under that name is unambiguously the contracted value, never
an ordinary tool call.

Target identity stays three facts (`RequestedTarget`, `ResolvedTarget`,
`ProviderReportedModel`), but only the first two are operation fields: the
reported identity cannot exist when the operation is constructed, so it is an
adapter-produced fact surfaced through the `ProviderModelReported` observation
and the `reported_model` field of terminal evidence. Adapters send exactly the
resolved target as the provider model parameter, never the requested selection,
and surface a provider-reported identity as soon as observed without fabricating
a match or mismatch; comparison is the caller's classification work (INV-014).

## Two-stage execution

`ModelRuntime<C>` has two stages, conforming to the accepted
provider-interaction boundary whose caller side is
[model-call-execution](model-call-execution.md) scope:

- `prepare(operation, cancellation)` performs all validation, translation,
  serialization, credential access, and request construction with no provider
  traffic, returning a `PreparationOutcome`: `Prepared` (an opaque, one-shot,
  non-cloneable, non-serializable capability), `Cancelled`, `Failed` (a
  trustworthy ordinary failure: unsupported operation, credential unavailable,
  credential unusable), or `Defect` (an adapter fault: serialization or request
  construction failed).
- `execute(prepared, sink, cancellation)` consumes the capability, performs at
  most one provider interaction, emits observations synchronously and in order,
  and always returns a `TerminalReport` — failures are typed evidence, never
  exceptions.

Nothing in this layer retries, falls back, or repeats a request after the
provider could have accepted it; there is no retry machinery to disable
(INV-025, INV-026). Why: a hidden second physical request would corrupt the
acceptance-boundary evidence that failure classification consumes.

`CancellationSignal` wraps any `Future<Output = ()> + Send`. In both stages the
pending work future is polled before the signal, so a result already available
in the same poll wins over cancellation. Why: a ready definitive provider
response must never be discarded in favor of ambiguous cancellation loss. During
execute, cancellation is best-effort: the adapter stops local work and reports
how far the request provably progressed; it never claims provider-side work
stopped.

## Observations

Observations are transient progress facts, never canonical transcript history
(INV-032; the authoritative commit is
[sessions-and-transcript](sessions-and-transcript.md) scope). The facts:
`SendCommenced` (the request is about to reach the transport; from here the
provider may have accepted it), `ExchangeEstablished` (a correlated response
began: proof the boundary was crossed; it carries `ExchangeFacts` — the HTTP
status plus the provider request id read from the `request-id`/`x-request-id`
response headers, the support/audit correlation fact that every exchange-bearing
terminal- evidence variant also retains), `ProviderModelReported`,
`TextDelta`/`ThinkingDelta`/`ToolArgumentsDelta` (indexed by provider part
order), `ToolCallProposed`, `UsageReported` (later reports supersede via
`TokenUsage::absorb`; reported fields replace, unreported fields never erase),
and `FinishReported`. Boundary-progress facts exist so the caller can durably
record how far an attempt provably progressed before a loss.

## Terminal evidence

`TerminalEvidence` is typed so the caller can classify without string matching;
strings appear only as retained detail inside already-classified variants:

- `Completed`: complete correlated response, terminal success status, valid
  completion material (`CompletionFinish` excludes refusal by construction).
- `Refused`: a complete exchange reporting the provider's refusal outcome. See
  the downgrade note below — no in-repo adapter surfaces this today.
- `ProviderError`: a complete, correlated definitive error response, classified
  into the shared `ProviderErrorKind` vocabulary (credential rejected,
  permission denied, invalid request, target not found, request too large, rate
  limited, quota exhausted, overloaded, provider internal, unrecognized; the
  kind lives in the core crate, and each adapter owns an exhaustive mapping into
  it) plus retained `NativeErrorFacts` that classification never reads. Retained
  native message text is credential-redacted, not verbatim: Anthropic truncates
  every native message at 2048 bytes (marked with the `… [truncated]` suffix) at
  the evidence-redaction boundary, and OpenAI captures non-envelope error bodies
  lossy-UTF-8 at the same 2048-byte bound. Why: audit evidence must be bounded
  and secret-free before it leaves the adapter. Quota exhaustion is distinct
  from rate limiting. Why: a billing condition must never be treated as
  retry-later backoff.
- `CancellationConfirmed`: a definitive provider cancellation response. No
  in-repository adapter constructs one; the variant keeps the vocabulary total
  so observing one never forces a misclassification.
- `ProvenUnsent`: acceptance was provably impossible — cancelled before send,
  connect failed before any request byte, or a provably unacceptable incomplete
  write (never constructed by the HTTP adapters, since an HTTP server can act
  before end-of-request framing).
- `BoundaryLoss`: the request crossed or may have crossed the acceptance-capable
  boundary and no definitive response exists, with a typed `LossCause`
  (cancellation after send, timeout, transport failure, response body lost,
  unintelligible success body, unexpected HTTP status, stream ended without
  terminal marker, stream protocol violation) and the partial facts observed
  before the loss.

A success-status response whose body is not valid completion material is
boundary loss, never completion. An unrecognized finish token is boundary loss
in both adapters, never silently completed. A finish reason observed before a
stream loss is retained as `finish_reported` but is not refusal or completion
evidence, because the exchange did not complete.

Refusal downgrade: both adapters' decoders construct `Refused` evidence, but
`execute` unconditionally converts it to `ProviderError { kind: Unrecognized }`
before returning, because a fully buffered HTTP request exposes no independent
proof that the response arrived only after the complete upload. Why: without
full-upload proof a refusal token cannot satisfy the completed-exchange
precondition for the refusal disposition, so the adapter fails toward known
failure rather than inventing evidence.

## SSE framing

`SseFraming` is a provider-agnostic incremental parser from transport byte
chunks to `SseRecord`s (WHATWG event-stream grammar subset: `event` and `data`
fields, multi-line data joined with `\n`, comment lines, one leading BOM,
`\n`/`\r\n`/`\r` terminators including a CR/LF pair split across chunks). The
`id` and `retry` fields are parsed and dropped. Why: they exist for stream
resumption, and resuming would be a second request.

Guarantees:

- Framing results never depend on how the transport fragments bytes into chunks.
- One configured limit bounds both any single line (checked while copying, so an
  unterminated line never buffers past it) and any record's retained content
  (joined data including separators plus the retained event value). Keep-alive
  comments never accumulate toward the bound; a replaced `event:` value stops
  counting.
- Records completed before a failure in the same chunk are still delivered
  alongside the failure. Why: evidence observed before a fault (a provider-model
  report, for example) must not depend on transport batching.
- A framing failure is terminal: later pushes frame nothing and repeat the same
  failure. `finish()` reports `Clean` or `TruncatedRecord`, which adapters
  surface as stream-integrity evidence.

## Structured output and tool decode

`StructuredOutputContract` (name, description, JSON Schema, generated from a
Rust type via schemars or supplied explicitly) is realized by both adapters as a
forced tool/function call with parallel tool use disabled. That is a request
constraint, not a response guarantee: a nonconforming or malformed response can
still carry zero or several proposals, and the provider-independent decode below
is what enforces the exactly-one contract. Why: one decode path across adapters
beats per-provider native output mechanisms that would return content-text
values and require schema transformation.

`decode_structured` and `decode_structured_json` are pure functions over
already-delivered response parts: exactly one proposal under the contract name
must exist, and failures are typed — `NoStructuredValue`,
`MultipleStructuredValues` (never silently picking one), `JsonSyntax`,
`SchemaMismatch`, and `DomainInvalid` carrying the caller's own
`DomainValidator` issues. Decoding never performs a model call; a repair attempt
is a new, explicitly authorized operation owned by the caller.

`decode_tool_arguments` decodes a `ToolCallProposal`'s raw argument JSON (kept
verbatim as produced, never re-serialized) into a typed value, distinguishing
`JsonSyntax` from `SchemaMismatch`. This layer contains no execution machinery:
a decoded proposal is data for a separately authorized tool request.

## ScriptedModel

`ScriptedModel` replays caller-declared `Script`s (observation facts plus exact
terminal evidence) through the real `ModelRuntime` surface: scripted fixtures
declare their result rather than simulate one. Preparation consumes the next
script and records the received operation under one lock; script exhaustion is a
preparation `Defect`, so it can never be mistaken for provider evidence. The
prepared capability is opaque and one-shot like a real adapter's; an unpolled
preparation consumes nothing, and a dropped capability emits nothing. Both
stages ignore the cancellation signal: a script that describes cancellation must
declare cancellation evidence explicitly, so an already-fired signal never
manufactures `Cancelled` or proven-unsent outcomes from a fixture. Why: nothing
is inferred from timing; scripted evidence is declared, never simulated.

## Provider adapters

Both adapters implement the same shape: at most one `POST` per operation
(`/v1/messages` for Anthropic with `x-api-key` and `anthropic-version` headers;
`/v1/chat/completions` for OpenAI with a bearer `Authorization` header),
hand-rolled serde wire types with no provider SDK dependency, and typed evidence
out. Construction validates configuration (absolute HTTP(S) base URL with no
user info, query, or fragment; positive SSE record limit); construction failure
is a configuration defect, not operation evidence.

Transport discipline (both adapters — one send is provably one physical
request):

- Redirect following disabled: a 307/308 replay would be a hidden second POST,
  so any redirect surfaces as `UnexpectedHttpStatus` boundary loss.
- Protocol-level retries disabled (`reqwest::retry::never()`).
- Idle-connection reuse disabled (`pool_max_idle_per_host(0)`), so every send
  opens a fresh connection. Why: this eliminates the stale-connection replay
  path and makes a connect failure provably precede any request byte, which is
  what lets `ConnectFailed` claim proven-unsent.
- Connect and whole-exchange timeouts default to none and are caller-owned
  configuration; a connect timeout is proven-unsent, a post-send timeout is
  boundary loss.

Success is specifically HTTP 200; another 2xx is not recognized terminal
success. 4xx/5xx responses are classified through each adapter's exhaustive
single-`match` native mapping. Anthropic: a 401 status classifies
credential-rejected regardless of any contradictory body token; otherwise a
recognized error-envelope `type` token refines the classification, and an
unrecognized or absent token falls back to the HTTP-status table, so
`Unrecognized` is reached only when token and status are both unmapped. OpenAI:
401 always credential-rejected, then recognized native code, then recognized
type, then status. Unknown material lands in `Unrecognized` with the native
facts retained rather than guessed at. Buffered response bodies and streamed
responses are each bounded at 8 MiB; exceeding a bound is loss/violation
evidence, not truncated success.

Stream integrity, Anthropic: the decoder enforces the Messages stream protocol —
`message_start` first with a complete envelope (discriminators, id, model, input
usage), content-block bookkeeping by index (no reopened or sparse indices, no
delta for an unopened block), thinking blocks must close with their integrity
signature, tool-use argument JSON must be a complete object, a `stop_reason`
with final output usage must precede `message_stop`, a reported stop sequence
must be one the request declared, and `message_stop` is the only terminal
marker. Unknown event names and delta types are tolerated (documented additive
evolution); an unrecognized content-block type or malformed known event is a
protocol violation. A stream ending any other way is explicit incomplete-stream
evidence — never silent success.

Stream integrity, OpenAI: the terminal marker is the literal `[DONE]` record,
and `stream_options.include_usage` is always requested so a conforming stream
reports usage before it. `[DONE]` yields terminal evidence only when the
assistant role, model identity, final usage, and a finish reason were all
observed. Chunks must agree on identity: a chunk without a completion id, with a
conflicting completion id, or with a conflicting reported model — including on a
mid-stream error record — is a terminal protocol violation, so a spliced stream
never completes under the first identity (INV-014). Refusal fragments and
`content_filter` finishes become refusal evidence (then downgraded as above); a
`stop` finish maps to end-turn only when the request declared no stop sequences,
and `length` stays unrecognized. Why: the adapter treats each shared token as
ambiguous — `stop` cannot prove a natural stop versus a stop-sequence hit,
`length` cannot prove the output ceiling versus a context limit — and collapsing
either would invent evidence. Mid-stream error records are definitive provider
errors classified by native code.

Usage is provider-stated only, never estimated; OpenAI's cache-read count comes
from `prompt_tokens_details.cached_tokens` and no cache-creation count is
fabricated.

## Credential-access boundary

The in-process boundary implements the access-port rules of the credential
lifecycle record (INV-035); channels, delivery, and rotation policy are
[configuration-and-credentials](configuration-and-credentials.md) scope.

- `CredentialReference` is the non-secret durable name; it is safe in errors and
  configuration. `CredentialValue` is the boundary value: no `Display`, no
  serialization, `Debug` always redacted. `expose_bytes` is the sole read path;
  the landed adapters call it for exactly two purposes — building request
  authentication and seeding the credential-redaction machinery that scrubs
  provider-controlled output.
- `CredentialAccess::resolve` is called during preparation of each physical
  request; nothing is cached. Why: per-request resolution makes mounted-secret
  rotation visible without a hub restart. Resolution races the cancellation
  signal so a blocked read cannot hold a cancelled operation. Failures are
  reference-only (`Unmapped`, `Unavailable`, `Unreadable`) and never contain
  secret bytes.
- The production implementation is hubd's `FileCredentialAccess`: each resolve
  rereads the key file named by `ANTHROPIC_API_KEY_FILE` and feeds the
  production `AnthropicRuntime`.
- The resolved value is scoped to the one prepared request as a
  sensitivity-marked HTTP header; execute performs no second lookup.
- Provider-controlled text is credential-sanitized before leaving the adapter:
  terminal-evidence text (error messages, raw bodies, transport detail, reported
  identifiers) is redacted with the exact preparation-time value, tool-argument
  JSON is redacted JSON-aware (including escaped representations), and streamed
  text/thinking deltas are redacted with a held-back trailing credential prefix
  so a secret split across provider chunks can never be emitted piecewise; when
  ordering forces a held prefix out, it is replaced with `[redacted]`. Why: fail
  closed — a possible secret prefix is destroyed rather than delivered.

## Open edges

- `Refused` terminal evidence never leaves either adapter: execute
  unconditionally downgrades it to a provider error because the buffered HTTP
  transport cannot prove complete request upload; surfacing refusal dispositions
  awaits an upload-proving transport or evidence source.
- `CancellationConfirmed` and `SendIncompleteProvenUnacceptable` are
  vocabulary-total variants no in-repository adapter constructs today.
- The three-kind consumer allowlist (provider adapters, the
  `model-provider-runtime` bridge, the hub composition root) is a review-time
  contract only; no manifest allowlist check enforces it.
