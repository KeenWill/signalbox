# Model-runtime substrate

This page specifies the Layer-1 typed model-runtime boundary as implemented in
`crates/model-runtime`, `crates/model-runtime-anthropic`,
`crates/model-runtime-openai`, `crates/model-runtime-codex-cli`, and
`crates/model-runtime-claude-cli`. The Codex CLI adapter's feature
classification, ambient-skill catalog probe, and pinned version follow the
version pinned in `tooling/codex-cli/package.json`. This page covers the
provider-neutral operation, observation, and evidence vocabulary; SSE framing;
structured-output and tool decode; `ScriptedModel`; the four provider adapters;
and their credential boundaries. Layer-2 authorization and evidence
classification ([model-call-execution](model-call-execution.md)), credential
channels, delivery, and rotation discipline
([configuration-and-credentials](configuration-and-credentials.md)), and the
authoritative transcript commit
([sessions-and-transcript](sessions-and-transcript.md)) are owned by those
companion pages. This page also owns the shared
[operator failure taxonomy](#operator-failure-taxonomy) — defined in
`crates/application` and consumed by signalboxd telemetry.

## Boundary and crate layout

The runtime layer is five hand-written library crates: one provider-neutral core
crate plus separately named provider adapters. `signalbox-model-runtime` is the
shared vocabulary; the Anthropic and OpenAI adapters additionally own their
HTTP, TLS, and serde dependencies, while the CLI adapters own only focused
subprocess, temporary-file, signal, and serde dependencies. Test helpers ship in
no built library artifact. `crates/domain`, `crates/application`, and
`crates/persistence` declare no dependency on any runtime crate, and no runtime
type appears in a domain or application signature (INV-002, INV-005); the
approved runtime consumers are the adapter crates, the
`crates/model-provider-runtime` bridge — whose `RuntimeModelCallProvider`
implements the application's `ModelCallProvider` port over any
`ModelRuntime<ModelCallId>`, depending on both crates so the dependency arrow
points from the bridge into application, never from application into the runtime
— and the daemon composition root (see Open edges). The Cargo manifest is the
enforcement mechanism: an undeclared dependency fails the workspace build. Why:
manifest-visible boundaries make a boundary violation a reviewable diff instead
of a silent import.

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
optional temperature, top-p, stop sequences, reasoning level, fast mode, and
provider-tagged service tier), declared `ToolDefinition`s, a `ToolChoice`
(automatic/any/named), an optional `StructuredOutputContract`, and a
`DeliveryMode` (buffered or streamed). Settings are provider-enforced request
controls unless an adapter's owning section records a capability-limited
advisory exception; an adapter never silently presents prompt instructions as
hard transport controls.

**Committed unimplemented functionality — workspace-instruction transport.** The
operation will carry
`workspace_instructions: Option<WorkspaceInstructionRegion>` beside `system` and
conversation history. The region is a validated nonempty exact UTF-8 byte value
bounded by the selected target's declared workspace-instruction byte capacity;
the runtime neither parses nor rewrites its daemon-authored wrappers. Validation
rejects a present region unless the resolved target and adapter mapping both
declare `typed_system` support and sufficient byte capacity. Each adapter maps
the field only to its provider's instruction/system transport, after the system
prompt and before conversation messages, and fails before send when that mapping
cannot preserve the boundary. It may not concatenate the region into ordinary
system text, emit a user/tool message, or enable a native project-file loader.
No present runtime operation carries this field.

The `RuntimeModelCallProvider` bridge sets every operation it prepares to
`Streamed`. Both HTTP adapters honor that mode by setting the provider-native
stream flag and decoding the response as SSE; `Buffered` remains available to
other direct runtime callers. Why: the application provider bridge is the one
composition point that can request live observations without changing the
provider-neutral application port or assigning progress facts terminal
authority.

`ModelOperation::validate` rejects, before any send: duplicate ordinary tool
names, a named tool choice matching no declared tool, and an ordinary tool
colliding with the output contract's name. Why: the contract name is reserved so
a returned proposal under that name is unambiguously the contracted value, never
an ordinary tool call.

Provider preparation also fails before any send when its wire representation
cannot preserve typed conversation-part order. In particular, Chat Completions
rejects assistant text following a replayed tool call because its assistant
message shape cannot encode whether that text preceded or followed the call.

Target identity stays three facts (`RequestedTarget`, `ResolvedTarget`,
`ProviderReportedModel`), but only the first two are operation fields: the
reported identity cannot exist when the operation is constructed, so it is an
adapter-produced fact surfaced through the `ProviderModelReported` observation
and the `reported_model` field of terminal evidence. Adapters send exactly the
resolved target as the provider model parameter, never the requested selection,
and surface a provider-reported identity as soon as observed without fabricating
a match or mismatch; comparison is the caller's classification work (INV-014),
under the provider-target identity rule of
[model-call-execution](model-call-execution.md).

Neither HTTP adapter ever requests server-side model fallback, so a provider
marker announcing that another model continued the turn is evidence that the
resolved target did not serve it. The Anthropic adapter therefore recognizes the
`fallback` content block explicitly rather than leaving it in the tolerated
additive-evolution branch: the buffered decoder reports the model the block
names as continuing the turn through the ordinary `ProviderModelReported` fact
and then closes the response as `ResponseUnintelligible` boundary loss, and the
stream decoder treats an opened `fallback` block as a protocol violation. Why:
the caller's identity rule must be able to tell an alias made concrete from a
different model substituted, and a signal the provider states explicitly should
not reach that rule as a generic unknown-block failure. The marker itself
crosses the boundary only through that reported identity — this layer has no
substitution variant of its own — so what the caller can conclude from it is
bounded by [model-call-execution](model-call-execution.md).

## Two-stage execution

`ModelRuntime<C>` has two stages; the caller side of the provider-interaction
boundary is [model-call-execution](model-call-execution.md) scope:

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

Nothing in this layer retries, falls back, or repeats its adapter-owned unit of
irrevocable dispatch after the provider could have accepted it (INV-025,
INV-026). That unit is one HTTPS request for a direct adapter and one process
spawn for a subprocess adapter. A subprocess adapter cannot observe or govern
the wrapped provider client's internal HTTP attempts; those are
provider-internal in the same sense as server-side attempts behind one direct
request. Why: a hidden second adapter dispatch would corrupt the
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

For a correctly correlated `TextDelta`, the provider bridge copies the
adapter-sanitized text unchanged to its injected best-effort presentation sink
and still retains the exact observation on the existing evidence path. A
cross-wired delta reaches no presentation sink. Presentation delivery neither
alters nor replaces the terminal report, and sink loss cannot change terminal
classification. The HTTP adapters perform credential redaction before emitting
the delta (INV-035); the bridge and daemon do not attempt a second redaction.

## Terminal evidence

`TerminalEvidence` is typed so the caller can classify without string matching;
strings appear only as retained detail inside already-classified variants:

- `Completed`: complete correlated response, terminal success status, valid
  completion material (`CompletionFinish` excludes refusal by construction).
- `Refused`: a complete exchange reporting the provider's refusal outcome. See
  the downgrade note below: the direct HTTP adapters do not surface it, while
  the Codex CLI adapter does because its structured response envelope and
  terminal process event jointly establish the complete exchange.
- `ProviderError`: a complete, correlated definitive error response, classified
  into the shared `ProviderErrorKind` vocabulary (credential rejected,
  permission denied, invalid request, target not found, request too large, rate
  limited, quota exhausted, overloaded, provider internal, unrecognized; the
  kind lives in the core crate, and each adapter owns an exhaustive mapping into
  it) plus retained `NativeErrorFacts` that classification never reads. Retained
  native message text is credential-redacted, not verbatim: Anthropic and OpenAI
  retain each complete adapter-bounded fallback body until the
  evidence-redaction boundary, sanitize literal and JSON-escaped representations
  with the exact prepared credential, and only then truncate native messages at
  2048 bytes with the `… [truncated]` suffix. Why: truncating inside an escape
  can make valid JSON unparseable and hide a reversible credential
  representation from format-aware redaction; audit evidence must be bounded and
  secret-free before it leaves the adapter. Quota exhaustion is distinct from
  rate limiting. Why: a billing condition must never be treated as retry-later
  backoff. HTTP adapters decode both `Retry-After` delay-seconds and HTTP-date
  forms; Codex decodes its bounded seconds/minutes retry phrase. The resulting
  duration is typed exchange evidence, never retained prose. Rate-limit and
  overload successors use it as a minimum beneath a five-minute cap; quota
  successors remain immediate.
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

The partial facts include `ToolCallsAtLoss`: whether a tool call had opened in
the response material the adapter decoded before the loss — opened, none opened,
or unobserved. A tool call can open without producing any observation, because a
provider may announce a call's identity and name and then be cut off before any
argument fragment while the proposal observation is emitted only on
finalization; and `LossCause` answers only *how* the exchange was lost. Without
this fact the distinction is reachable only by reading a rendered violation
detail, which the terminal-evidence rule above forbids. Like `finish_reported`,
it reports the decoded prefix and nothing beyond it: none opened says no tool
call opened in what the adapter decoded, never that the provider sent none.
`Unobserved` is reported where an adapter is not positioned to know — a body
that never parsed, a decode abandoned with content blocks unexamined, a streamed
record or CLI event that failed its framing, JSON-bound, or typed-event decode,
a stream whose framing ended inside an incomplete record, material the runner
read off the transport but never delivered to a decoder — a framed record
dropped when cancellation lands mid-chunk, a CLI line past the event bound, a
partial line lost when a deadline drops the reader — a loss raised by a layer
that reads no response material, and every Codex CLI loss raised before the
agent-message envelope parses, since that adapter's item lifecycle carries no
tool item.

The dividing line is whether the adapter examined the material that could open a
tool call, not whether it accepted that material. A record read and then
rejected on semantics has been examined, so it states none opened; material
discarded before examination withholds. Examination does not require a full
parse: where a type discriminator alone precludes a tool call — an SSE event
name that is not `content_block_start`, a Claude CLI event type that is not
`assistant` — the question is settled without parsing the payload, and the
negative is stated. Nor does a rejection make the whole response unexamined:
rejecting the final content block of a buffered body leaves nothing unread and
states the negative, while the same rejection with blocks still behind it
withholds. A tool call an earlier record already established outranks the
withholding in every adapter.

**Implemented behavior — provider non-acceptance evidence.**
`ProviderErrorEvidence::non_acceptance_proven` is an adapter-owned typed fact,
never inferred from `ProviderErrorKind` or provider prose. This proof separates
the `successor` and `terminal` endings of
[the credential-availability machine](credential-availability.md); this page
owns the evidence algebra that carries it. Adapters set the proof alongside
`ProviderError` for the exact quota-exhausted, rate-limited, and overloaded
provider responses whose protocol semantics establish non-acceptance. Each
adapter owns its exhaustive native mapping; the provider bridge preserves the
proof without deriving it from `ProviderErrorKind`, status retryability, or
native prose. Classification alone remains insufficient, and absence of the
proof keeps the known failure terminal.

The admitting condition is fixed here because the two readings differ in whether
another provider call happens. A proof is admitted only when the adapter decoded
its own documented error envelope and the decoded native token names one of the
three causes in that adapter's exhaustive mapping — `rate_limit_error` or
`overloaded_error` for Anthropic, and `rate_limit_exceeded`, `rate_limit_error`,
or `insufficient_quota` for OpenAI. Codex additionally admits a classified
availability cause only when its JSONL lifecycle reaches the exact,
noncontradictory `turn.failed` closure. Every status-derived fallback carries no
proof: a response whose body is absent, undecodable, or names a token the
mapping does not cover keeps its status-classified kind and stays an ordinary
terminal known failure. A native token that contradicts its status carries none
either, and the credential-rejection precedence over a contradictory body
applies.

The proof is further restricted to an error *response* — an error-status
exchange whose body is that documented envelope, decoded before any stream
began. An SSE error record never carries it, whatever native token it holds.
Mid-stream and post-finish error records remain definitive `ProviderError`
evidence exactly as specified below, but by the time one arrives the provider
has demonstrably accepted the request and begun processing it: `message_start`,
content, reported usage, or a finish token is already observed. Non-acceptance
is precisely what such an exchange disproves, so attaching the proof there would
authorize a second paid call for work the provider already did. An availability
failure that arrives mid-stream therefore terminalizes the turn as any other
known failure does, with no successor. The two CLI adapters differ here. The
Claude Code CLI cannot supply the proof at all: it classifies from the rendered
failure message by substring, which is exactly the native prose this contract
already refuses as a derivation, and it surfaces no structured native code its
mapping could name. Admitting one under it would need that CLI to expose a
stable machine-readable discriminator first. The Codex CLI classifies from that
same rendered prose, but its JSONL lifecycle carries the machine-readable
closure this contract demands, so it does supply the proof — under the exact,
noncontradictory `turn.failed` closure stated above and never from the prose
alone. A trailer that contradicts the recorded stream error fails closed and
carries nothing. An under-decoded rejection loses a substitution the deployment
had configured, which costs one turn; a status-only or prose-derived inference
that the provider did not act would authorize a second paid call on evidence the
provider never gave.

A success-status response whose body is not valid completion material is
boundary loss, never completion. An unrecognized finish token is boundary loss
in both direct HTTP adapters, never silently completed. A finish reason observed
before a stream loss is retained as `finish_reported` but is not refusal or
completion evidence, because the exchange did not complete.

Refusal downgrade: both direct HTTP adapters' decoders construct `Refused`
evidence, but `execute` unconditionally converts it to
`ProviderError { kind: Unrecognized }` before returning, because a fully
buffered HTTP request exposes no independent proof that the response arrived
only after the complete upload. Why: without full-upload proof a refusal token
cannot satisfy the completed-exchange precondition for the refusal disposition,
so the adapter fails toward known failure rather than inventing evidence.

Post-finish error records (Anthropic adapter): once its stream has reported why
generation stopped, a later error record that classifies as `Unrecognized` is a
protocol violation rather than definitive provider evidence. Why: it supersedes
the reported finish with no classifiable failure, and it would otherwise reach
the caller in exactly the shape the refusal downgrade above produces — an HTTP
200 exchange, `Unrecognized`, and the same absent or fabricated native material
— leaving a genuine failure indistinguishable from a decoded refusal. An error
record that *does* classify still outranks the reported finish, because it
carries information the finish does not.

Unrecognized finish reasons (OpenAI adapter) record their verdict but defer it
to `[DONE]`, so records arriving after that finish are still examined and a
definitive error among them supersedes it. Why: returning at the finish chunk
would leave such a record unread, and the post-finish rule below could never
fire for this finish — a caller would accept the prefix as a clean stop at an
output bound. The two envelope checks that are already decidable — assistant
role established and model identity reported — still run at the finish rather
than at `[DONE]`, and a stream failing either reports the envelope defect and
carries no `finish_reported`, because a caller cannot otherwise distinguish a
well-formed response that stopped at an output bound from an envelope that was
never valid and also reported one. Accumulated tool content is not among those
checks: a tool-bearing request can legitimately exhaust the output ceiling
partway through a call, so the token is an observed fact rather than a
contradiction, and the buffered decoder retains it in exactly that case — the
two decoders must not disagree about the same response.

Post-finish error records (OpenAI adapter): once its stream has reported why
generation stopped, a later error record carrying no native material at all is a
protocol violation rather than definitive provider evidence. Why: it supersedes
the reported finish with nothing a caller could act on, and it would otherwise
reach the caller in exactly the shape the refusal downgrade above produces — an
HTTP 200 exchange, `Unrecognized`, and empty native facts — leaving a genuine
failure indistinguishable from a decoded refusal. An error record carrying any
native material still outranks the reported finish, even when its type or code
is unfamiliar and it therefore classifies as `Unrecognized`: it carries
diagnostics the finish does not, and cannot be mistaken for the downgrade.

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
Rust type via schemars or supplied explicitly) is realized as one tool/function
proposal under the contract's reserved name. The direct adapters use their
native request tools; the OpenAI adapter forces that proposal through
`tool_choice`, the Anthropic adapter asks for it by instruction instead
([Direct HTTP adapters](#direct-http-adapters)). The Codex CLI adapter renders
the contract into the stateless prompt and requires the final CLI agent message
to satisfy an outer response schema whose one contract-named proposal carries
the value. In every case that is a request constraint, not a response guarantee:
a nonconforming or malformed response can still carry zero or several proposals,
and the provider-independent decode below is what enforces the exactly-one
contract. Why: one decode path across adapters is preferred over
provider-specific output values that require caller-side transformation.

`decode_structured` and `decode_structured_json` are pure functions over
already-delivered response parts: exactly one proposal under the contract name
must exist, and failures are typed — `NoStructuredValue`,
`MultipleStructuredValues` (never silently picking one),
`SuppressedStructuredValue`, `JsonSyntax`, `SchemaMismatch`, and `DomainInvalid`
carrying the caller's own `DomainValidator` issues. Decoding never performs a
model call; a repair attempt is a new, explicitly authorized operation owned by
the caller.

A contract-named proposal whose whole argument object a CLI adapter's credential
boundary suppressed is a proposal for the contract, counted by the exactly-one
guard exactly as an admitted one is: alone it fails as
`SuppressedStructuredValue`, and beside an admitted contract-named proposal it
fails as `MultipleStructuredValues`. Suppression withholds a proposal's
arguments, never its admitted tool name, so it can neither hide a second
conflicting value from a caller that decodes without a terminal classification
step nor satisfy a named tool choice under a foreign name.

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
clones share the same ordered script queue and received-operation receipts, so
an execution composition can retain a probe without cloning model-call work.
Both stages ignore the cancellation signal: a script that describes cancellation
must declare cancellation evidence explicitly, so an already-fired signal never
manufactures `Cancelled` or proven-unsent outcomes from a fixture. Why: nothing
is inferred from timing; scripted evidence is declared, never simulated.

## Provider adapters

### Direct HTTP adapters

The Anthropic and OpenAI adapters implement the same shape: at most one `POST`
per operation (`/v1/messages` for Anthropic with `x-api-key` and
`anthropic-version` headers; `/v1/chat/completions` for OpenAI with a bearer
`Authorization` header), hand-written serde wire types with no provider SDK
dependency, and typed evidence out. Construction validates configuration: the
base URL must be absolute HTTPS, except that plain HTTP is admitted for an
IP-literal loopback host used by deterministic tests; user information, a query,
or a fragment is forbidden; the SSE record limit must be positive, and a
configured whole-exchange timeout must be positive. Construction failure is a
configuration defect, not operation evidence.

Provider traffic uses reqwest 0.13 with default features disabled and only its
providerless rustls-platform-verifier and byte-stream features enabled. Both
adapter crates select rustls's ring crypto provider, matching the provider
selected by the process's SQLx TLS stack, so the unified rustls instance never
has two implicit provider candidates. Both clients force the rustls backend,
verify the server certificate and hostname against `rustls-platform-verifier`'s
platform trust roots, require TLS 1.2 or newer, and carry no custom-root or
verification-bypass surface. Ambient system and environment proxies are disabled
and the adapters expose no proxy configuration. Why: provider credentials and
content must not silently traverse an operator-unreviewed intermediary.

Transport discipline (both adapters — one send is provably one physical
request):

- Redirect following disabled: a 307/308 replay would be a hidden second POST,
  so any redirect surfaces as `UnexpectedHttpStatus` boundary loss.
- Protocol-level retries disabled (`reqwest::retry::never()`).
- Idle-connection reuse disabled (`pool_max_idle_per_host(0)`), so every send
  opens a fresh connection. Why: this eliminates the stale-connection replay
  path and makes a connect failure provably precede any request byte, which is
  what lets `ConnectFailed` claim proven-unsent.
- A configured whole-exchange timeout covers connection establishment through
  the complete buffered body or streamed terminal record. The daemon obtains it
  from the required `numeric_bounds.model_exchange_timeout` deployment policy;
  the exact value `"none"` makes the exchange unbounded. Callers may
  additionally configure a shorter connect timeout. A connect timeout is
  proven-unsent, while a whole-exchange timeout after send is boundary loss.

Success is specifically HTTP 200; another 2xx is not recognized terminal
success. 4xx/5xx responses are classified through each adapter's exhaustive
single-`match` native mapping. Anthropic: a 401 status classifies
credential-rejected regardless of any contradictory body token; otherwise a
recognized error-envelope `type` token refines the classification, and an
unrecognized or absent token falls back to the HTTP-status table, so
`Unrecognized` is reached only when token and status are both unmapped. OpenAI:
401 always credential-rejected, then recognized native code, then recognized
type, then status. Unknown material classifies as `Unrecognized` with the native
facts retained rather than guessed at.

All provider-controlled response input is bounded before it can accumulate into
parsed or retained output. A buffered body and the cumulative bytes of one
stream are each capped at 8 MiB. SSE framing independently bounds every line
while copying and every retained record; its default record bound is also 8 MiB.
Exceeding the buffered bound is response-body-loss evidence, while exceeding the
cumulative stream or SSE bound is stream-protocol-violation evidence. Complete
records inside the byte budget are processed before a coalesced over-budget
suffix, so transport batching cannot erase earlier evidence or a terminal
marker.

Before serde sees a buffered success body or JSON SSE record, a shared
allocation-free scanner rejects JSON nested beyond 127 containers, matching
serde_json's admitted recursion boundary and including unknown fields and
`RawValue` material. Unknown fields remain tolerated for additive provider
evolution, but they receive the same byte and nesting limits as known fields.
Malformed or over-depth HTTP-200 JSON is `ResponseUnintelligible` boundary loss.
Over-depth streamed material and malformed known-event JSON are terminal stream
protocol violations; unknown event names remain additively tolerated and their
bounded payloads are discarded without typed parsing. A malformed or over-depth
body attached to a definitive 4xx/5xx status cannot erase that definitive
exchange: the adapter falls back to status classification and retains only
bounded, credential-sanitized native material. Why: hostile provider output may
consume only fixed memory/depth budgets and can never be upgraded into
completion by truncation or permissive parsing.

Stream integrity, Anthropic: the decoder enforces the Messages stream protocol —
`message_start` first with a complete envelope (discriminators, id, model, input
usage), content-block bookkeeping by index (no reopened or sparse indices, no
delta for an unopened block), thinking blocks must close with exactly one
non-empty integrity signature (a Claude 5-family stream opens the thinking block
with an empty-string signature placeholder and delivers the real signature
through a later signature delta, so an empty opening value counts as absent,
never as a first signature), tool-use argument JSON must be a complete object, a
`stop_reason` with final output usage must precede `message_stop`, a reported
stop sequence must be one the request declared, and `message_stop` is the only
terminal marker. Unknown event names and delta types are tolerated (documented
additive evolution); an unrecognized content-block type or malformed known event
is a protocol violation. A stream ending any other way is explicit
incomplete-stream evidence — never silent success.

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

The Anthropic adapter emits no sampling control and no forced tool choice.
`MessagesRequest` carries no `temperature`, `top_p`, or `top_k` field, and
preparation and `validate_model_settings` refuse settings that set one rather
than dropping it. `tool_choice` is emitted only as `{"type":"auto"}`, with
`disable_parallel_tool_use` for the structured-output contract, and the demand
travels as an adapter-authored instruction after the caller's system text.
`ToolChoice::AnyTool`, `ToolChoice::Named`, and the contract are therefore
advisory here — the capability-limited exception to the provider-enforced
settings rule. No top-level `thinking` configuration object is emitted; replayed
`thinking` and `redacted_thinking` content blocks are unaffected.

Anthropic preflight input counting preserves the generation request's
prompt/cache-affecting `output_config` and same-target `speed` fields. A mapped
fast serving identity consumes the fast toggle during preparation, so neither
the `speed` field nor its beta header is emitted for the counting or generation
request in addition to that alternate target.

### Compatibility smoke

Both direct HTTP adapters carry a gated live compatibility smoke — the Anthropic
adapter's in `.github/workflows/anthropic-smoke.yml`, the OpenAI adapter's in
`.github/workflows/openai-smoke.yml`. Each spends one real exchange against the
cheapest model its provider currently advertises (`claude-haiku-4-5` and
`gpt-5-nano` respectively), run through that crate's own `ModelRuntime`
implementation with a small fixed prompt and no provider-side prompt caching: at
one exchange per gated run a cache write is never amortized by a later read, so
caching would raise the cost of the run it is meant to cheapen. Unlike the Codex
CLI smoke, there is no locally installed executable and therefore no version to
verify beforehand; each adapter targets its provider's stable public API
directly, so spending the one exchange is the whole smoke rather than a second
gate behind a credential-free version probe.

Everything below applies to both smokes except where a paragraph names one
adapter, which marks a difference in what that provider's wire protocol makes
observable.

Each smoke asserts only the protocol surfaces a provider-side change would move:
a definitive HTTP 200, and the response decoding as `Completed` evidence, or as
the adapter's downgraded-refusal `ProviderError` shape (`kind: Unrecognized`
from that same 200 exchange — see the refusal-downgrade rule above; a raw
`Refused` never leaves either adapter). That downgraded shape is accepted only
when it carries exactly the native material its adapter's downgrade fabricates
and nothing else — for Anthropic the stable `native.error_token: "refusal"`
discriminator with no code and no message, for OpenAI no native material at all
— and the execution also observed a reported refusal finish. A mid-stream native
error inside a 200 SSE body reaches the caller as `Unrecognized` from the same
status, and those two further facts are what a genuine streamed failure cannot
present. Either accepted shape must carry provider-reported input usage present
*and positive* (a request that reached the model always billed at least one
input token). Neither smoke asserts anything about answer quality. Output usage
is only required to be present, not positive. A valid `Completed` response can
legitimately report zero output tokens, and a downgraded refusal can be blocked
before any completion token is produced, so both accepted shapes share one
output usage-presence check without demanding it be nonzero.

The OpenAI smoke also accepts one loss shape: the exchange that stopped at its
own output ceiling. Chat Completions reports that as `finish_reason: "length"`,
which this adapter deliberately leaves unmapped, so the decoder ends the stream
as `BoundaryLoss` carrying the token verbatim. That is an accurate report about
answer length, not a protocol break — the request was accepted and the body
framed and decoded — so failing the smoke on it would assert something about
answer quality, which this smoke does not do. The acceptance is keyed to that
exact token from a 200 exchange; any other unrecognized finish, and every other
loss cause, still fails, as does a `length` finish from a stream that never
reported a model identity: that check runs at the finish chunk rather than at
`[DONE]`, so the reported identity is established before the verdict is
deferred.

That shape carries usage like the other two, and is held to the same
requirements above. The trailing usage-only chunk is valid only after a finish,
and the decoder defers the unrecognized-finish verdict to `[DONE]` precisely so
that chunk is consumed first; a stream that never sends it fails the
`include_usage` contract and reports that missing chunk instead of an
output-bound stop.

Accepting that shape is what makes the OpenAI smoke's reasoning-capable target
safe as a required check. Hidden reasoning tokens bill against the same ceiling
as visible content, and Chat Completions offers no control that caps them below
it — `reasoning_effort` is a qualitative hint, so even its lowest setting leaves
the worst case unbounded. No effort is pinned there: this repository's own
OpenAI catalog records that the `"minimal"` effort is listed by no current model
page and appears on no row, so pinning it would assert a capability the
repository does not claim. That operation therefore sets no explicit provider
control, and the smoke's `ModelCapabilityCatalog` is correspondingly empty.

The Anthropic smoke pins no explicit reasoning effort either, and needs no
equivalent loss shape. Extended reasoning is requested per operation — the
adapter emits `output_config.effort` only when `ModelSettings.reasoning_level`
is set explicitly, and that smoke never sets it — so the exchange spends no
hidden reasoning tokens against the output ceiling. Its ceiling is therefore a
pure cost cap: every token billed against it is visible output that the fixed
one-word prompt already bounds, and the exchange cannot truncate into a
`BoundaryLoss` a required check could not distinguish from a real compatibility
break.

This smoke's required aggregate is merge-gating for a pull request that changes
the adapter crate or the workflow itself — an exception to `CONTRIBUTING.md`'s
general testing-strategy default that a credentialed live-provider smoke is
never the merge gate. Provider unavailability, rate limiting, or a misconfigured
credential therefore reports as a temporary red rather than a verdict on the
change's correctness.

The workflow reports on every pull request without a path filter. GitHub
independently withholds secrets from ordinary fork `pull_request` runs
regardless of environment policy. Its secretless eligibility job then checks the
complete pull request file list: no change to the adapter crate or to the
workflow's own definition is an immediate success; for a qualifying change it
compares `github.event.pull_request.head.repo.full_name` with
`github.repository`, fails a mismatch with a manual-dispatch instruction, and
admits the live job only for a same-repository head. The credentialed job
condition independently repeats that comparison. A final always-running job
folds the eligibility and conditional live results into the required check, so a
skipped or failed required smoke cannot appear green; for a pull request that
requires the smoke, the aggregate also repeats the same-repository comparison.
Manual dispatch remains available, and a path-filtered push to `main` — gated on
the adapter crate and the workflow file itself, so an edit to the workflow's own
definition cannot land unexercised — reruns the smoke after merge.

A twice-daily schedule (`0 13 * * *` and `0 1 * * *` UTC) also triggers the
workflow as a provider-drift canary between adapter-touching pull requests,
spending one more real, paid exchange per run. A scheduled trigger is not a
`pull_request` event, so the eligibility gate's non-`pull_request` branch marks
it required unconditionally — the same branch a manual dispatch or a qualifying
push takes — and the workflow-level concurrency group falls back to the run id
for a non-`pull_request` event on `main`, so each scheduled run keeps its own
slot; a manual dispatch on a non-main ref instead shares a per-event-and-ref
group that cancels superseded runs. GitHub only fires `schedule` events from a
repository's default branch, so the schedule takes effect only once a change
lands on `main`.

The `anthropic-smoke` and `openai-smoke` environments are configured for all
branches, for the same reason the `codex-smoke` environment is: GitHub evaluates
an environment used by `pull_request` against `GITHUB_REF`, the synthetic merge
ref rather than the head branch. That setting admits fork and same-repository
merge refs alike and supplies no security boundary. Forks are excluded, in
order, by GitHub secret withholding and the three explicit repository-name
comparisons above.

The workflow's own concurrency is a single group keyed to the pull request ref
(the run id for any other event on `main`; a per-event-and-ref group, canceling
superseded runs, for a manual dispatch on any other ref), so a slot is released
only when the same ref is superseded: a pull-request run by a newer update to
its pull request, a non-main dispatch by a newer dispatch on that ref — never a
push to `main`, which keeps every run. The Codex smoke's own workflow-level
group behaves identically. There is no additional job-level group serializing
the live exchange itself: a fixed inner group shared across every ref would let
an unrelated smoke-required run evict this job's queued slot even though
`cancel-in-progress: false` does not protect it, because GitHub keeps at most
one running and one pending member per concurrency group and replaces the
pending one when a third arrives. That would fail a required check that never
tested its own head. A concurrent real exchange costs a small fraction of a
cent; required-check integrity is worth more than serializing that spend.

Each credential (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`) is referenced only in
the step that spends the exchange, scoped to that step's environment alone,
never echoed and never passed in argv. The crate is compiled before that step
runs, and the compiled test binary's path is captured from that credential-free
build and invoked directly rather than through a second `cargo test`, so no
build-freshness check — and therefore no build script or procedural macro — ever
runs while the key is readable.

## Codex CLI provider adapter

`signalbox-model-runtime-codex-cli` wraps the Codex CLI event protocol as the
offline fixture corpus records it; that recorded version is distinct from the
installation pin in `tooling/codex-cli/package.json`, and the adapter's exported
version constant is the contract a later composition must pin before wiring the
adapter. The daemon composition runs a bounded, credential-free version probe
before opening its socket; model dispatch performs no separate version probe.
Preparation validates and renders the complete operation, writes the non-secret
response-envelope schema to a private temporary file, and returns a one-shot
capability without starting a process. Admitted schemas and replayed tool
arguments remain raw JSON through prompt serialization; a shallow raw member
scan still requires each schema to declare an object root. Execution consumes
the capability as exactly one `codex exec --json --ephemeral` spawn on Unix,
passes the full rendered frontier on stdin, requires absolute configured
executable and working-root paths, selects the exact resolved model, ignores
user configuration and rule files, and explicitly disables every feature in the
pinned CLI inventory that can add a model-visible tool, external interaction,
instruction source, or delegated execution surface outside the declared tools,
or that can replace the pinned executable the version contract names. It
independently disables configured agents, ambient skill-instruction injection,
MCP servers, and web search, sets the project-instruction byte budget to zero,
and uses the read-only CLI sandbox; prompt text is never a capability boundary.
Strict configuration turns an unavailable control into a closed failure instead
of silently relaxing this invocation boundary. Before spawn it clears the parent
environment, then copies only its explicit home/Codex-home, executable and
temporary path, XDG, locale/terminal, certificate, and proxy allowlist;
unrelated service variables do not reach the CLI. A proxy variable whose URL
authority embeds userinfo (`scheme://user:secret@host`) is refused before
`SendCommenced` as `ProvenUnsent(ConnectFailed)` naming only the variable — the
CLI could reflect its proxy configuration in output the adapter can only
shape-redact, so an inherited proxy credential never reaches the child; a proxy
value that is not UTF-8 cannot be verified credential-free and is refused the
same way. A `HOME` or `CODEX_HOME` the parent cannot resolve to an absolute
directory — empty, or relative with no resolvable current directory to resolve
it against — is refused the same way, because the child would otherwise read its
login store from beneath its own configured working root and select an
unintended ambient login; a resolvable one is absolutized against the parent's
directory before spawn. It neither resumes nor persists a Codex thread. Why: a
fresh ephemeral invocation keeps provider session state out of memory, and the
caller supplies the complete conversation frontier instead of an in-memory
resume pointer. The read-only sandbox and working root are the adapter's
filesystem boundary. Unix supervision contains the process group the adapter
creates, so construction rejects hosts where process-group control is
unavailable; a descendant that deliberately leaves that group is outside the
adapter's boundary. Host isolation owns containment beyond the created group —
specifically the runner sandbox in build-out — and is not an adapter claim.

The rendered prompt opens with a preamble whose tool-authority statement is
singular and positionally first: the serialized `tools` array is named the
single authority on which tools exist, `tool_choice` and `structured_output` are
named as the only statements that narrow or add to what may be proposed, the
harness's own disabled facilities are distinguished from the declared tools, and
the preamble states no categorical tool prohibition. Why: a categorical
prohibition such as "Do not use shell, file, web, MCP, or collaboration tools" —
aimed at the wrapped CLI's native facilities — names exactly the categories a
caller's tool catalog populates, so the prompt would carry two competing
authority statements and a model could obey the wrong one, refusing work its
declared tools authorized. The native facilities need no prompt-level
prohibition because the invocation disables them mechanically, and prompt text
is never a capability boundary. The residual the adapter cannot remove: the
pinned CLI injects its own agent-identity instructions around the stdin prompt
at runtime, so the preamble's authority statement is the first tool statement
the adapter controls rather than the first text the model sees. The translation
unit test pins the rendered prompt — no categorical prohibition, exactly one
authority statement, positioned before the serialized request.

**Committed unimplemented functionality — Codex file delivery.** The present
adapter supports only its ambient credential home and keeps `OPENAI_API_KEY` and
every other direct credential value outside the cleared child environment. The
configuration grammar admits `file`, but the present composition rejects it as
undelivered. File delivery must resolve the selected profile during preparation.
The adapter admits only the exact `OPENAI_API_KEY` `env_key`; every forwarded or
process-control name is invalid configuration. It adds the selected value as an
operation-scoped child override after clearing the parent environment. The value
must be absent from argv, logs, debug output, retained evidence, and every later
spawn, and must seed the adapter's exact-value redaction before any
provider-controlled output leaves the crate. This override does not weaken
ambient mode's credential exclusion.

**Committed unimplemented functionality — Codex OAuth redaction.** OAuth
delivery gives the adapter a daemon-minted access token, the identity token
issued with it, and the account metadata in a scratch credential home rather
than through the child environment. Before anything is written or the child
starts, the adapter must seed the exact-value redaction boundary with every
value that
[the `oauth` delivery](configuration-and-credentials.md#the-oauth-delivery)
requires the redactor to be seeded with. That contract decides *which* values
those are and why; this page owns *how* the adapter installs and applies the
scrub. Each such value is seeded both as the raw token and as the JSON string
representations whose escapes decode to that same token, because the adapter is
the layer that sees both forms. Possible token prefixes are retained across
stdout and stderr chunks, and all child-controlled text passes through that
scrub before JSON decoding, truncation, debug rendering, observations, or
durable evidence. Ambient-mode shape redaction remains defense in depth; it
cannot replace exact-value redaction when preparation knows the token. Failure
to install the scrub is a typed pre-spawn delivery failure.

`SendCommenced` immediately precedes spawn. Spawn failure is
`ProvenUnsent(ConnectFailed)`; after successful spawn no path respawns the CLI.
The first `thread.started` establishes the exchange and its thread id becomes
the provider request id. Unknown top-level events and unsupported item kinds are
additively tolerated within the byte and JSON-depth bounds. Repeated object
members are ambiguous by construction, never additive: the adapter rejects them
in both the outer JSONL event and its escaped response envelope before JSON
projection as `BoundaryLoss(StreamProtocolViolation)`. Known item lifecycle
events must carry a nonempty item identity and type even when the adapter does
not otherwise interpret them. Known events with invalid shapes, non-UTF-8 or
undecodable JSONL, nonzero or signal process exits, and `turn.failed` fail
closed as provider error evidence; the rendered CLI message classifier gives
credential rejection first precedence and maps only explicit native phrases,
with all other material `Unrecognized`. The CLI reports a failed exchange as a
stream-level `error` event followed by its `turn.failed` lifecycle echo; the
decoder accepts exactly that one trailer and keeps the stream-level message as
the typed provider error, while any other post-terminal event — including one
contradicting the recorded failure — remains a fail-closed protocol violation.
Exit zero without `turn.completed` is
`BoundaryLoss(StreamEndedWithoutTerminalMarker)`, never completion.

`turn.completed` is success evidence only when the last completed agent-message
item decodes as the adapter's response envelope and satisfies the declared-tool
constraints. The invocation also asks the pinned CLI to retain its final message
in a private temporary file. When JSONL delivered no agent-message item, the
adapter reads that independent representation after process exit under the same
event-size bound and applies the identical envelope and redaction checks; a
missing, oversized, non-UTF-8, or invalid retained message still fails closed.
An agent message delivered in JSONL remains authoritative, so the second channel
does not overwrite contradictory streamed evidence. A named ordinary-tool choice
admits at least one proposal and requires every proposal to carry that selected
name. For a structured-output contract, zero or several contract-named proposals
remain definitive completion material for the provider-independent structured
decoder above to classify. The decoded envelope is checked against the shared
JSON nesting bound independently of the escaped outer event; envelope decode
errors are content-silent. The envelope distinguishes completion from refusal.
Within the envelope each tool call carries its provider-supplied argument text
inside a string: strict structured-output validation refuses any schema object
that does not supply `additionalProperties: false` and require all its
properties, so a free-form argument object is not expressible in the output
schema and the live API rejects one as `invalid_json_schema`. The adapter
requires the contained text to stay within the shared JSON nesting bound, which
the line-level and agent-message-level checks cannot see because string content
does not nest the outer JSON, and reports over-depth text as boundary loss; it
judges neither syntax nor shape, so malformed and non-object argument text
passes onward byte-verbatim when it is credential-shape clean rather than
becoming boundary loss. Preserved text becomes proposal material the
provider-independent decoders classify, and those decoders impose no
argument-size ceiling of their own: a direct runtime caller decodes the
preserved text at any size this adapter's event limit admits. The 1 MiB
normalized-argument ceiling [tool-loop](tool-loop.md) states is the
`RuntimeModelCallProvider` bridge's, applied as it normalizes each runtime
proposal while classifying a terminal report; this adapter's event limit is the
looser of the two, so on that path argument text above the ceiling fails its
model call as unrepresentable tool material before any tool round instead of
reaching one as `invalid_arguments`. `decode_tool_arguments` returns exactly its
typed `JsonSyntax` or `SchemaMismatch` failure, and `decode_structured_json`
returns `JsonSyntax`, `SchemaMismatch`, or — where the caller supplies a domain
validator — `DomainInvalid`. Neither performs a model call or a repair round,
and it is the tool loop that projects an ordinary proposal's typed failure as
its `invalid_arguments` result for the next model round. Caller JSON remains raw
through serialization, preserving deep admitted values and their numeric
lexemes. Buffered delivery retains its content without deltas; streamed delivery
feeds raw bounded CLI reasoning and final-envelope text through the stateful
redactor before emitting ordered deltas and the same terminal evidence. A
provider failure message consults the same held lookbehind state before it
enters provider-error evidence: a message that extends a held credential
candidate, or that arrives during oversized-credential suppression, is
suppressed whole rather than statelessly re-redacted. Usage comes only from
`turn.completed`. The adapter maps `input_tokens`, `output_tokens`,
`cache_write_input_tokens`, and `cached_input_tokens` exactly to Signalbox
input, output, cache-creation input, and cache-read input axes. Each decoded
field is independently optional: an omitted field remains unreported rather than
becoming zero. A partial event records only its present axes, and a total-only
event records none because the adapter never distributes a total. The pinned
CLI's separate `reasoning_output_tokens` counter and additive `total_tokens`
siblings have no existing Signalbox usage axis; neither is folded into output or
another field.

The pinned CLI exposes no argv, configuration, or subscription request controls
for output-token ceiling, temperature, top-p, or stop sequences. This adapter is
therefore the narrow exception to the provider-enforced settings rule: it
renders all four values into the model-visible operation prompt as advisory
context, and neither claims nor supplies hard enforcement. A caller that
requires provider-enforced generation settings must not select this adapter.
Preparation still rejects a zero output-token limit, malformed replayed
tool-call JSON, non-finite sampling values, temperature outside zero through
two, and top-p outside zero through one as unsupported caller input. The offline
fake CLI verifies the advisory rendering and applies the same strict-schema
validation to every spawned exchange, so a schema shape the live API refuses
cannot pass the fixture corpus.

Reasoning level, fast mode, and service tier are enforced through the explicit
preparation mappings owned by
[model/session settings](model-session-settings.md); they are not part of the
advisory exception above. The adapter validates the exact target capability
record before checking its ambient-login reference whenever an operation carries
an explicit catalog-governed control, and never delegates validation to the CLI.
Provider-default-only operations need no catalog lookup.

The adapter bounds every stdout event while copying and drains stderr while
retaining only a bounded prefix. Streamed credential lookbehind retains at most
64 KiB; exceeding that bound emits redaction under each held observation's
original metadata and suppresses later text through the terminal flush. A
credential-bearing JSON member at the start of CLI-controlled text is recognized
without requiring an enclosing object delimiter. Construction rejects a zero or
runtime-clock-unrepresentable process timeout. Cancellation before spawn is
proven unsent. After spawn it sends an interrupt to the dedicated process group,
retains the unreaped leader through a positive grace, and kills the group before
reaping the leader; cancellation is `BoundaryLoss(CancellationRequested)` and
never causes another spawn. Timeout starts immediately after successful spawn,
governs stdin transfer, stdout decoding, and process exit, then force-kills the
original process as typed boundary loss; interrupt grace is capped at the time
remaining before that deadline. Dropping or aborting execution synchronously
kills the still-owned original process group before the direct child drops. A
stdin write failure continues draining and decoding bounded JSONL stdout
alongside bounded stderr, then observes process status under the same controls,
preserving definitive CLI evidence instead of discarding it as transport loss or
blocking on a full stdout pipe. A provider failure remains definitive after such
a write failure, but a nominal completion is boundary loss because the adapter
cannot prove the full authorized frontier reached the CLI. Ready stdout is
polled before simultaneous control signals, then the decoder drains only the
current bounded reader batch before synchronously rechecking control, so
continuously ready stdout cannot starve it. Once a provider terminal marker is
observed, a later cancellation cannot replace that definitive evidence: it
terminates the group at once and the exchange returns that evidence, while the
process deadline continues to govern exit and cleanup. The adapter also bounds
stderr cleanup before reaping the direct child and terminates the original
process group when an inherited stdout or stderr handle outlives the deadline;
at that deadline a leader that already exited on its own keeps its definitive
evidence — an observed terminal marker or its exit status — and a pre-existing
kill-signal exit is observed on the still-waitable leader before cleanup signals
the group, so it stays distinguishable from a cleanup kill, while a leader
cleanup itself must kill remains typed timeout loss. On every ordinary exit it
likewise keeps the leader waitable until it has killed remaining group
descendants, then reaps the leader, so cleanup never signals through a reusable
process identity.

### Version pin and compatibility smoke

The wrapped CLI is an external program on its own release cadence, so the
adapter's exported supported-version constant is only a claim until three
statements agree: the version pinned for installation, the version the adapter
covers, and the version actually invoked.

`tooling/codex-cli/package.json` is the pin of record — an npm manifest naming
the CLI's distribution package at an exact `major.minor.patch` version, with a
committed lockfile so the installed artifact is integrity-checked. It is a
Renovate-tracked manifest, so a new CLI release arrives as an ordinary pull
request rather than as silent drift on whatever machine last installed the tool.
The manifest carries no minimum-release-age gate and never automerges. Why: for
a dependency that releases this often, calendar age is not evidence, and the two
gates below are.

The adapter build reads that manifest and derives its exported supported-version
constant from the exact dependency value, so the manifest is the single source
of truth and a Renovate change is mechanically complete. An unconditional
offline test still rejects a range, tag, alias, prerelease, or any shape other
than exactly three numeric components. The daemon composition and the live smoke
both verify that the installed executable reports the derived version; the
composition refuses startup before socket admission when the bounded probe
cannot prove equality.

One live exchange proves that the installed CLI still works through the adapter,
but does not prove that the recorded offline fixture corpus still represents
every current CLI event shape.

The compatibility smoke is the second gate: one exchange against the cheapest
model the smoke credential can address, run through this adapter with the real
pinned executable. Which models a credential may address is account-scoped, so
the model is a configured value with the cheapest advertised model as default.
Before spending anything it asserts that the executable's reported version
equals the supported version, then compares the CLI's complete feature list —
including stage and default — with an exact inventory that classifies every
entry as a hard-disabled capability or as non-capability behavior. A new,
removed, or changed entry fails the smoke until the version bump classifies it.
An isolated synthetic ambient skill must contribute the skills block plus its
name and description to the pinned CLI's ordinary model-visible catalog, while
its on-demand body remains absent; the complete catalog must disappear when the
production `skills.include_instructions=false` control is applied. An
unreadable, unparsable, or mismatched version likewise fails rather than
skipping. These three real-CLI controls also have a separate ignored,
credential-free entry point so they can run locally before the gated workflow
authenticates. Why: evidence recorded against a version that never ran, or whose
capability gap nobody reviewed, is worse than no evidence.

The smoke then asserts only the protocol surfaces a version bump moves — the
thread identifier reaching the exchange facts, the terminal usage counters, and
the response envelope decoding as a completed or refused terminal outcome — and
nothing about answer quality. The workflow reports on every pull request without
a path filter. GitHub independently withholds secrets from ordinary fork
`pull_request` runs regardless of environment policy. Its secretless eligibility
job then checks the complete pull request file list: no change to the CLI pin or
to the workflow's own definition is an immediate success; a pull request that
changes the pin, the workflow file, or both compares
`github.event.pull_request.head.repo.full_name` with `github.repository`, fails
a mismatch with a manual-dispatch instruction, and admits the live job only for
a same-repository head. The credentialed job condition independently repeats
that comparison. A final always-running job folds the eligibility and
conditional live results into the required check, so a skipped or failed
required smoke cannot appear green; for a pull request that requires the smoke,
the aggregate also repeats the same-repository comparison. Manual dispatch
remains available, a path-filtered push to `main` — gated on the pin manifest
and the workflow file itself, so an edit to the workflow's own definition cannot
land unexercised — reruns the smoke after merge, and a twice-daily schedule
(13:00 and 01:00 UTC, drifting an hour under standard time) spends two further
live exchanges a day as a paid provider-drift canary independent of any code
change.

The `codex-smoke` environment is configured for all branches because GitHub
evaluates an environment used by `pull_request` against `GITHUB_REF`, which is
the synthetic merge ref rather than the head branch. That setting admits fork
and same-repository merge refs alike and supplies no security boundary. Forks
are excluded, in order, by GitHub secret withholding and the three explicit
repository-name comparisons above. The model dispatch still performs no version
probe: this check lives in the smoke, never in the hot path.

The smoke authenticates the CLI through its own non-interactive API-key login,
piped from an environment-scoped secret into the CLI's credential store, which
the adapter then never reads — the same ownership split as production. Because
that is an API-key login rather than a subscription one, the smoke proves the
process, event-protocol, and envelope compatibility that a version bump breaks;
it does not exercise subscription login itself, which the CLI offers no durable
unattended path for.

## Claude Code CLI provider adapter

`signalbox-model-runtime-claude-cli` wraps the Claude Code print-mode JSONL
protocol at the exact version its crate-local npm manifest pins, from which the
adapter build derives its exported supported-version constant. Preparation
validates and renders the full `ModelOperation`, creates a private temporary MCP
catalog and isolated settings files, and returns a one-shot capability without
spawning Claude. Execution consumes it as one fresh Unix process using
`--print --verbose --output-format=stream-json --no-session-persistence`; it
passes the rendered frontier on stdin, selects the resolved model, and never
resumes a CLI session. `SendCommenced` immediately precedes spawn and no
execution path respawns. The CLI-process supervision contract above applies: the
adapter owns the created process group, bounds stdout events and retained
stderr, and treats cancellation, timeout, incomplete upload, child exit, and
group cleanup as typed evidence rather than logging them.

Caller tools are MCP tools, not prompt-embedded schema arrays. The adapter-owned
stdio bridge publishes exactly the operation's tool definitions plus any
structured-output contract under the private `signalbox_tools` server. It never
executes a caller tool. For each `tools/call` it returns the fixed
acknowledgement that Signalbox recorded the proposal; Claude consequently emits
a typed assistant `tool_use` followed by a user `tool_result`, and the adapter
returns the proposal for external authorization and execution. A controlled
`SessionStart` hook waits for a private readiness marker written only after the
bridge has answered `tools/list`. The bridge accepts exactly the MCP
`2025-11-25` initialization protocol; its `tools/call` request carries the
declared name and object arguments, and its fixed result returns one text
content block. This closes the print-mode discovery race: the accepted
`system/init` must report that server `connected` and its `tools` set must equal
the qualified declared MCP surface before any assistant content is admitted.

The invocation excludes ambient settings, sessions, slash commands, browser
integration, plugins, and built-in tools. `--tools` selects an empty built-in
surface; `--disallowedTools` also names every built-in the pinned executable
reports or documents (`Task`, `Bash`, the `Cron*` tools, `DesignSync`, `Edit`,
worktree, file search, cross-session discovery and messaging, agent-to-user
messaging, monitoring, notebook, notification, shells and the code-running
`REPL`, read/remote/report/scheduling/feedback, skill, `Task*`, `ToolSearch`,
web, workflow, and write); and `--allowedTools` contains only the qualified
declared MCP names. The cross-session and agent-to-user built-ins are denied
because they address other Claude Code sessions on the host, or the user
directly, rather than this adapter's own event stream. The inventory is the
second of two independent controls, so it names built-ins this invocation could
not otherwise reach. `dontAsk` is used because no undeclared capability may
become an interactive permission question. The initial event must also report no
slash commands, skills, or plugins and must identify the pinned Claude Code
version; any mismatch is stream-protocol boundary loss, not a relaxed
invocation.

The pinned stream establishes correlation and reported-model evidence through
`system/init`. Nonterminal `system/status`, `system/hook_started`,
`system/hook_progress`, `system/hook_response`, `system/api_retry`, and
`system/thinking_tokens` lifecycle events are discarded, so they are neither
mistaken for initialization nor mask the later typed terminal or process-exit
classification. Their remaining members become dropped redaction context, and
the two envelope fields that are not provider content — `type` and `subtype` —
are removed before that. A lifecycle `session_id` is removed on the same ground
only where it equals the identity `system/init` retained: a differing value is
stream-protocol boundary loss exactly as it is on a `result` event, and a value
carried before any `init` has correlated a session is not a repeated identity at
all, so it stays provider content and seeds the lookbehind. Dropping it on an
unchecked claim would let a credential prefix spelled there escape the shape
redactor through a later field (INV-035). Assistant `text`, `thinking`,
`redacted_thinking`, and `tool_use` blocks become typed observations and
assistant parts. A tool proposal must name the private MCP namespace, match a
declared schema name, carry a unique nonempty id and object arguments, and
receive exactly one matching user `tool_result` whose sole text block is the
fixed acknowledgement. Only a terminal `result` event can establish success or
refusal. The selected alias in `system/init` remains the provider-reported
model; the first assistant event may name the provider-resolved model, but every
later assistant event must repeat that same value. That resolved model is
retained only for this comparison and reaches no record, so it seeds a redaction
lookbehind of its own — a credential prefix ending it would otherwise escape the
shape redactor through a text block continuing it, which under ambient delivery
no exact-value redaction downstream can catch (INV-035). Every assistant
envelope repeats and discards the field beside its own content, so each one
re-seeds that lookbehind ahead of its content blocks rather than only the first:
content that spends the lookbehind in one event must not leave the next event's
text unguarded. A repeat re-seeds only once the previous registration has been
spent; while it is still live it already governs. Each discarded source holds an
independent lookbehind: the emitted identifier's adjacency to its record, the
chronological dropped provider text, and this discarded field are judged
separately, so bytes from one never sit between another's credential marker and
the continuation completing it. An error `result` and a nonzero process exit
produce typed provider-error evidence. Exit zero without it is
`BoundaryLoss(StreamEndedWithoutTerminalMarker)`; malformed or contradictory
JSONL is `BoundaryLoss(StreamProtocolViolation)`; and prose alone never becomes
terminal evidence. A success must satisfy the operation's any/named tool choice,
with a structured-output contract represented as the required named MCP tool.
Provider usage is retained only where the CLI reports it.

Ambient delivery leaves subscription-login resolution inside Claude Code, and an
ambient adapter accepts only its one configured non-secret
`CredentialReference`. File delivery is not so limited: the adapter holds the
complete adapter-scoped catalog of declared `claude_cli` file profiles and
resolves whichever reference the operation pins, so a historical session keeps
the profile it was created with even when the configured default has changed,
and two Claude families may prefer different profiles. It resolves that
reference during cancellable preparation, rejects an empty, non-UTF-8, or
NUL-bearing value, and writes the exact value to a mode-0600 credential file in
a private request-scoped settings store. That store's mode-0600 `settings.json`
configures Claude's `apiKeyHelper` to invoke a mode-0600 request-scoped script
through the fixed `/bin/sh` interpreter. The script preserves the exact file
bytes using only shell builtins and resolves no executable through `PATH`. The
credential, script, settings, and existing MCP support files share the temporary
directory; the adapter replaces only the already allowlisted `CLAUDE_CONFIG_DIR`
value with that directory and still never adds `ANTHROPIC_API_KEY` itself to the
child process environment. Dropping the prepared capability removes the
directory. The exact value remains in the one-shot capability so
provider-controlled observations and terminal evidence receive exact-value
redaction in addition to the CLI credential-shape and cross-fragment discipline.
Proxy userinfo and unusable credential-home paths still fail before spawn.

The output-token ceiling is enforced by the cleared child environment, while
reasoning level and fast mode use the explicit preparation mappings owned by
[model/session settings](model-session-settings.md). Temperature, top-p, and
stop sequences are the capability-limited advisory exception for this adapter.
When an operation carries an explicit catalog-governed control, exact-target
capability and mapping validation precedes the ambient-login reference check. A
service tier is always rejected.

The crate itself defines no provider-selection or configuration mapping.
signalboxd composes it from the deployment-owned `claude_cli` adapter mapping
and its three absolute process paths, described in
[configuration-and-credentials](configuration-and-credentials.md#the-static-model-alias-and-web-fetch-catalog).

The wrapped CLI is an external program on its own release cadence, so the same
three statements the Codex adapter binds must agree here too: the version pinned
for installation, the version the adapter covers, and the version actually
invoked.

`crates/model-runtime-claude-cli/package.json` is the pin of record — an npm
manifest naming the CLI's distribution package at an exact `major.minor.patch`
version, with a committed lockfile so the installed artifact is
integrity-checked. It is a Renovate-tracked manifest under the same policy as
the Codex pin: no minimum-release-age gate and no automerge. The adapter build
reads that manifest and derives its exported supported-version constant from the
exact dependency value, so the manifest is the single source of truth and a
Renovate change is mechanically complete. An unconditional offline test still
rejects a range, tag, alias, prerelease, or any shape other than exactly three
numeric components, and separately requires the committed lockfile to install
the version the manifest pins — the build reads only the manifest, so a lockfile
that disagreed would install an executable the adapter does not claim to
support.

The compatibility smoke is the second gate: one exchange against the cheapest
model the smoke credential can address, run through this adapter with the real
pinned executable. Which models a credential may address is account-scoped, so
the model is a configured value with the cheapest advertised model as default.
Before spending anything it asserts that the executable's reported version
equals the derived supported version; an unreadable, unparsable, or mismatched
version fails rather than skipping. That version gate also has a separate
ignored, credential-free entry point so it can run locally before the gated
workflow supplies a credential. The smoke requests no prompt caching: at one
exchange per pin bump a cache write is never amortized by a later read, so
caching would raise the cost of the run it is meant to cheapen.

Unlike the Codex smoke, there are no credential-free capability probes before
spend. The Claude Code CLI exposes no equivalent of a built-in feature registry
or a prompt-input dump, so the invocation-isolation surfaces are proven inside
the exchange instead: the adapter refuses a `system/init` that reports any slash
command, skill, or plugin, or a tool inventory differing from the declared MCP
surface. That is a weaker gate than the Codex one because it fails after spend
rather than before it.

The smoke then asserts only the protocol surfaces a version bump moves — the
session identifier reaching the exchange facts, the reported model, the terminal
usage counters, and the response envelope decoding as a completed or refused
terminal outcome — and nothing about answer quality. Reaching a decoded response
is itself the version-handshake evidence, because the adapter turns a
`system/init` reporting a different version into stream-protocol boundary loss.
The workflow reports on every pull request without a path filter, and its
secretless eligibility job, the credentialed job condition, and the
always-running required aggregate apply the same three repository-name
comparisons and the same fork exclusions the Codex smoke section describes,
against a path gate of `crates/model-runtime-claude-cli/**` together with the
shared `crates/model-runtime/src/cli_process.rs`. That shared file is in the
gate because the child environment assembly the credential delivery depends on
lives there rather than in the adapter crate, so a change to it moves the
surface this smoke proves. Manual dispatch remains available, and a
path-filtered push to `main` reruns the smoke after merge.

Installation differs from the Codex smoke in one respect. The Codex package
ships its platform binary as an optional dependency and needs no lifecycle
script, while this package's launcher is a stub until its own installer places
the native binary. The workflow therefore keeps `npm ci --ignore-scripts` and
runs that installer as its own named step, so the same package-authored code
runs explicitly and reviewably rather than as an implicit side effect of
installation, and still before any step carries a credential.

The gated workflow writes the environment-scoped API key to a mode-0600 source
file using shell builtins before the live test process starts. The test receives
only that non-secret path and supplies a file-backed `CredentialAccess`; the
adapter performs the request-scoped settings-store delivery above. The source
file is removed by an always-running cleanup step. The live exchange therefore
exercises the same file-delivery boundary as signalboxd without placing the key
in the test process or CLI child environment.

## Credential-access boundary

The in-process boundary implements the credential access-port rules (INV-035);
channels, delivery, and rotation policy are
[configuration-and-credentials](configuration-and-credentials.md) scope.

- `CredentialReference` is the non-secret durable name; it is safe in errors and
  configuration. `CredentialValue` is the boundary value: no `Display`, no
  serialization, `Debug` always redacted. `expose_bytes` is the sole read path;
  the direct HTTP adapters call it for exactly two purposes — building request
  authentication and seeding the credential-redaction machinery that scrubs
  provider-controlled output.
- Direct HTTP adapters and Claude CLI file delivery call
  `CredentialAccess::resolve` during preparation of each physical request;
  nothing is cached. Why: per-request resolution makes rotation visible without
  a daemon restart. Resolution races the cancellation signal so a blocked read
  cannot hold a cancelled operation. Failures are reference-only (`Unmapped`,
  `Unavailable`, `Unreadable`) and never contain secret bytes.
- The production implementation is signalboxd's `FileCredentialAccess`.
  Composition supplies each adapter with the complete map of every `file`
  profile reference declared for that adapter to its catalog path. A profile
  declared for another adapter remains unmapped. Each resolve rereads the mapped
  file and feeds the selected runtime.
- A direct HTTP adapter scopes the resolved value to the one prepared request as
  a sensitivity-marked HTTP header; execute performs no second lookup. Claude
  file delivery instead retains it in the one-shot capability for the private
  settings write and exact-value redaction described above.
- Provider-controlled text is credential-sanitized before leaving the adapter:
  terminal-evidence text (error messages, raw bodies, transport detail, reported
  identifiers) is redacted with the exact preparation-time value before any
  fallback-body truncation, tool-argument JSON is redacted JSON-aware (including
  escaped representations), and streamed text/thinking deltas are redacted with
  a held-back trailing credential prefix so a secret split across provider
  chunks can never be emitted piecewise; when ordering forces a held prefix out,
  it is replaced with `[redacted]`. Why: fail closed — a possible secret prefix
  is destroyed rather than delivered. The guarantee is bounded to exactly these
  representations — the exact value, its JSON-string-escaped form, and
  chunk-split prefixes of it; a reflection the provider re-encodes in any other
  form (base64, say) passes through unscrubbed, because no path here decodes one
  before matching.
- The Codex CLI adapter accepts only the configured non-secret
  `CredentialReference` and delegates resolution to the CLI's ambient
  subscription login on every fresh spawn. It never locates, reads, copies,
  logs, or transports the CLI credential store. Because no credential value
  crosses the adapter boundary to seed exact-value redaction, CLI-controlled
  text and JSON are recursively scrubbed by credential-bearing member names and
  credential token shapes before observations or evidence leave the crate;
  credential-bearing authorization and cookie header shapes consume their whole
  line value; quoted credential values consume through their matching unescaped
  quote; object- or array-shaped credential values consume through their
  balanced structural close, and a container still open at the end of the
  controlled text is suppressed through that end rather than released piecewise;
  a tool argument object suppressed as a whole crosses the adapter as typed
  non-executable material retaining only its admitted tool name; the application
  records a fixed `RuntimeSafety` denial and continues the same turn, never
  dispatching sentinel JSON to an executor; a private-key PEM block is consumed
  through its matching end marker whether or not an assignment introduces it;
  credential labels are recognized in their space-separated spellings as well as
  their underscore, hyphenated, and concatenated ones; and JSON
  identity/session-token members are included. Envelope-decode errors are
  content-silent rather than embedding a rejected provider value. Why:
  subscription authentication remains wholly inside the intended CLI control
  surface while credential-shaped reflection still fails closed.

### Codex CLI shape-redaction scope

The singular `token` name and composite names ending in singular `token` are
credential-bearing while plural usage counters such as `input_tokens` and
`output_tokens` are not. The additional covered name policies are `passphrase`,
`passwd`, a normalized name ending in `pwd`, and the exact normalized names
`signing_key`, `encryption_key`, `ssh_key`, `hmac_key`, and `license_key`;
arbitrary names ending in `key` are outside this rule. ASCII-case-insensitive
`--password`, `--api-key`, and `--passphrase` long options at text start or
after whitespace consume a token argument separated by one or more spaces or
tabs.

Within any `://` authority-shaped span, without validating its scheme, the
userinfo password between the first colon before the last `@` and that `@` is
redacted. The authority-shaped span ends at whitespace, `/`, `?`, `#`, a quote,
a comma, or a semicolon. The case-sensitive curl options `-u` and `--user`, at
text start or after whitespace and separated from their possibly quoted argument
by spaces or tabs, redact the password after that argument's first colon. A
double-quoted credential key remains subject to the raw assignment scan whenever
malformed JSON prevents the structural JSON scanner from claiming it.

For a delta fragmentation of one text stream, including Unicode-escaped markers,
the concatenated streamed output is never less redacted than the stateless scan
of the concatenated provider text, except for the fragmentations the defect
ledger names. That exception is not a covered limit: it is one shape — a quoted
credential key at a position JSON would not admit, reached after an earlier
delta released a clean prefix — and it is a defect awaiting a fix in the
emitted-context path. Its reach grows with the number of delta boundaries rather
than with the number of shapes, so the ledger pins thirty single-split
fragmentations exactly and 1,095 two-split ones by count and digest. Every
committed corpus line is cut at every UTF-8 boundary on the default test path,
and the enumeration matches the leaking splits against a named ledger exactly: a
new one is a regression and a repaired one must shrink the ledger, so the set
cannot drift in either direction. A shape the contract covers that the sink
still leaks is carried as `KNOWN-FAILING`, which is a defect ledger and never an
`ACCEPTED-UNCOVERED` classification — that status records only what this
contract explicitly declines to cover. Fail-closed suppression is absorbing for
the sink's lifetime: usage reports, other fact boundaries, and terminal flushes
never re-enable provider-controlled bytes. Streamed lookbehind's 64-KiB memory
bound is independent of its work bound. One initial unsafe-suffix classification
decides whether a prefix is held; it is not charged as reclassification. After a
streamed hold or a live dropped-provider match-only suffix at length L,
reclassification occurs only when its length reaches at least twice L, while
crossing the cap forces one final round. Each streamed post-hold round invokes
the two top-level whole-buffer classifiers; a dropped suffix round invokes its
suffix classifier but receives the same conservative two-classifier charge.
Before invocation, the sink charges the full joined input, including emitted or
dropped lookbehind context, at that rate. A live dropped suffix is extended in
place between those checkpoints, empty dropped items are no-ops, and obsolete
prefixes are compacted only at a checkpoint; suffix-copy work therefore follows
the same geometric linear bound rather than growing with the square of the
provider-event count. A per-continuously-unresolved-candidate budget fails
closed before a round would make cumulative reclassification input exceed
393,216 bytes (six times 64 KiB), independent of delta count. Without external
context, the geometric held lengths presented to each classifier sum to at most
196,608 bytes (three times 64 KiB). The 66,000 one-byte continuation shape
performs its initial unsafe-suffix classification once, then thirteen
reclassification rounds: 188,387 aggregate held bytes and 376,774 charged
rescanned bytes.

This is a text-shape contract, not cross-field semantic correlation. It does not
associate a credential name in one structural position with a value in another:
examples outside the contract include CSV header/value rows, SQL column/`VALUES`
positions, XML element content or sibling elements, name/value objects encoded
as array siblings, and Kubernetes or Actions `name:`/`value:` pairs. A
format-aware boundary must sanitize those forms before they become independent
text units. Why: named credential-shaped reflection fails closed without
claiming general secret detection.

## Operator failure taxonomy

`crates/application/src/operator_failure.rs` defines the one closed
operator-facing failure classification shared by application services, the
persistence adapters, and signalboxd telemetry: six scheduling, model-call,
tool-loop, and liveness error families (startup scan, turn activation,
eligibility sweep, model-call repository, tool-loop repository, and turn
liveness) map into `OperatorFailureClass` through the `ClassifyOperatorFailure`
trait, exposing a user-content-free classification to shared telemetry while the
underlying error keeps its diagnostic detail internally. The turn-liveness
family separates a failed inventory read, which is a pass that decided nothing,
from a failed terminalization, which is a decision that could not be carried
out, and forwards the shared failed-turn transition's own classification
unchanged when that transition is what refused. The four classes:

- **`Infrastructure { commit_ambiguous }`** — the operation could not complete;
  the flag marks failures whose transaction fate is unknown (commit-ambiguity
  handling: [persistence-protocol](persistence-protocol.md)).
- **`FailClosedCorruption`** — committed rows cannot construct the accepted
  domain value (fail-closed reconstitution:
  [persistence-protocol](persistence-protocol.md)).
- **`IdentityCollision`** — a fresh daemon-minted candidate identity collided
  with a durable identity (per-stage retry rule:
  [model-call-execution](model-call-execution.md)).
- **`CallerOrHubBug`** — a request or internal guard that can fail only because
  of a defect, kept distinct from corruption.

The class states only a failure's severity. The orthogonal sanitized cause code
stating *what happened* is owned by whichever page owns the behavior that raises
it: for provider and model-call failures — carried by the model-call bridge,
reusing this page's `ProviderErrorKind` vocabulary verbatim for definitive
provider errors — the owning page is
[model-call-execution](model-call-execution.md), and the turn-liveness causes
are owned by [turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md).

Concurrent staleness is not a class: a guarded write that matches zero rows is
consumed inside adapters by reload-and-rederive
([persistence-protocol](persistence-protocol.md)) unless the transaction's own
premises made a match mandatory, so the taxonomy classifies only genuine
failures after staleness handling.

## Open edges

- `Refused` terminal evidence never leaves either direct HTTP adapter: execute
  unconditionally downgrades it to a provider error because the buffered HTTP
  transport cannot prove complete request upload; surfacing refusal dispositions
  awaits an upload-proving transport or evidence source.
- `CancellationConfirmed` and `SendIncompleteProvenUnacceptable` are
  vocabulary-total variants no in-repository adapter constructs.
- The three-kind consumer allowlist (provider adapters, the
  `model-provider-runtime` bridge, the daemon composition root) is a review-time
  contract only; no manifest allowlist check enforces it.
- [Identity, credentials, and resource governance](../open-questions.md#identity-credentials-and-resource-governance)
  owns controlled provider-proxy and private-root support.
- [Codex CLI fixture validation](../open-questions.md#codex-cli-fixture-validation)
  owns how a pin bump will prove that the recorded offline event-shape fixtures
  still represent the installed CLI.
