# Model-runtime substrate

This page specifies the Layer-1 typed model-runtime boundary as implemented in
`crates/model-runtime`, `crates/model-runtime-anthropic`,
`crates/model-runtime-openai`, `crates/model-runtime-codex-cli`, and
`crates/model-runtime-claude-cli`, verified against the implementing stack
through PR #183 (`agent/provider-call-security-parser`). The Claude Code CLI
adapter implementation was verified through PR #320
(`agent/claude-cli-adapter`). The Codex CLI adapter stack comprises PR #264
(`agent/codex-cli-wrap`) and PR #268 (`agent/codex-cli-pin-smoke`); its
escalation closeout is PR #317 (`agent/escalation-closeout`). The Codex CLI
compatibility-smoke automation was verified through PR #333
(`agent/ci-tells-truth`). The `signalboxd` names this page states for the
composition root, its telemetry, and the production `FileCredentialAccess` were
verified through PR #258 (`agent/signalboxd-rename`); the Anthropic adapter's
server-side `fallback`-block recognition was verified through PR #280
(`agent/provider-identity-normalization`). The five persistence-repository
families in the operator-failure inventory were verified through PR #288
(`agent/audit-fix-docs-coherence`). The streamed-delivery bridge and ephemeral
text-delta projection were verified through PR #300
(`agent/token-level-streaming`); the Claude 5-family thinking-signature stream
shape was verified through PR #305 (`agent/sonnet-streamed-tool-use`). The Codex
CLI redaction contract was verified through PR #316
(`agent/redaction-hardening`; shape coverage, absorbing suppression, enumerated
single-split parity, and geometric work bound). It covers the provider-neutral
operation, observation, and evidence vocabulary; SSE framing; structured-output
and tool decode; `ScriptedModel`; the four provider adapters; and their
credential boundaries. Layer-2 authorization and evidence classification
([model-call-execution](model-call-execution.md)), credential channels,
delivery, and rotation discipline
([configuration-and-credentials](configuration-and-credentials.md)), and the
authoritative transcript commit
([sessions-and-transcript](sessions-and-transcript.md)) are owned by those
companion pages. This page also owns the shared
[operator failure taxonomy](#operator-failure-taxonomy) — defined in
`crates/application` and consumed by signalboxd telemetry.

## Boundary and crate layout

The runtime layer is five hand-rolled library crates: one provider-neutral core
crate plus separately named provider adapters, with SerdesAI as a design
reference only. `signalbox-model-runtime` is the shared vocabulary; the
Anthropic and OpenAI adapters additionally own their HTTP, TLS, and serde
dependencies, while the CLI adapters own only focused subprocess,
temporary-file, signal, and serde dependencies. Test helpers ship in no built
library artifact. `crates/domain`, `crates/application`, and
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
optional temperature, top-p, stop sequences), declared `ToolDefinition`s, a
`ToolChoice` (automatic/any/named), an optional `StructuredOutputContract`, and
a `DeliveryMode` (buffered or streamed). Settings are provider-enforced request
controls unless an adapter's owning section records a capability-limited
advisory exception; an adapter never silently presents prompt instructions as
hard transport controls.

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
[model-call-execution](model-call-execution.md#provider-target-identity).

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
bounded by
[model-call-execution](model-call-execution.md#provider-target-identity).

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
Rust type via schemars or supplied explicitly) is realized as one forced
tool/function proposal. The direct adapters use their native request tools. The
Codex CLI adapter renders the contract into the stateless prompt and requires
the final CLI agent message to satisfy an outer response schema whose one
contract-named proposal carries the value. That is a request constraint, not a
response guarantee: a nonconforming or malformed response can still carry zero
or several proposals, and the provider-independent decode below is what enforces
the exactly-one contract. Why: one decode path across adapters beats
provider-specific output values that require caller-side transformation.

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
`Authorization` header), hand-rolled serde wire types with no provider SDK
dependency, and typed evidence out. Construction validates configuration: the
base URL must be absolute HTTPS, except that plain HTTP is admitted for an
IP-literal loopback host used by deterministic tests; user information, a query,
or a fragment is forbidden; and the SSE record limit and whole-exchange timeout
must both be positive. Construction failure is a configuration defect, not
operation evidence.

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
- Every request has a positive whole-exchange timeout, covering connection
  establishment through the complete buffered body or streamed terminal record.
  The provisional default is ten minutes; callers may configure another positive
  budget, and may additionally configure a shorter connect timeout. A connect
  timeout is proven-unsent, while a timeout after send is boundary loss. Why: a
  provider that stalls forever must not hold a turn attempt forever, while the
  deliberately generous first budget accommodates long streamed generations
  until production latency data supports a tighter provider/model-specific
  policy.

Success is specifically HTTP 200; another 2xx is not recognized terminal
success. 4xx/5xx responses are classified through each adapter's exhaustive
single-`match` native mapping. Anthropic: a 401 status classifies
credential-rejected regardless of any contradictory body token; otherwise a
recognized error-envelope `type` token refines the classification, and an
unrecognized or absent token falls back to the HTTP-status table, so
`Unrecognized` is reached only when token and status are both unmapped. OpenAI:
401 always credential-rejected, then recognized native code, then recognized
type, then status. Unknown material lands in `Unrecognized` with the native
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

## Codex CLI provider adapter

`signalbox-model-runtime-codex-cli` wraps the locally installed Codex CLI event
protocol captured by the offline fixture corpus at version `0.145.0`; its
exported version constant is the contract a later composition must pin before
wiring the adapter. The model dispatch itself performs no separate version
probe. Preparation validates and renders the complete operation, writes the
non-secret response-envelope schema to a private temporary file, and returns a
one-shot capability without starting a process. Admitted schemas and replayed
tool arguments remain raw JSON through prompt serialization; a shallow raw
member scan still requires each schema to declare an object root. Execution
consumes the capability as exactly one `codex exec --json --ephemeral` spawn on
Unix, passes the full rendered frontier on stdin, requires absolute configured
executable and working-root paths, selects the exact resolved model, ignores
user configuration and rule files, and explicitly disables every feature in the
pinned CLI inventory that can add a model-visible tool, external interaction,
instruction source, or delegated execution surface outside the declared tools.
It independently disables configured agents, ambient skill-instruction
injection, MCP servers, and web search, sets the project-instruction byte budget
to zero, and uses the read-only CLI sandbox; prompt text is never a capability
boundary. Strict configuration turns an unavailable control into a closed
failure instead of silently relaxing this invocation boundary. Before spawn it
clears the parent environment, then copies only its explicit home/Codex-home,
executable and temporary path, XDG, locale/terminal, certificate, and proxy
allowlist; unrelated service variables do not reach the CLI. A proxy variable
whose URL authority embeds userinfo (`scheme://user:secret@host`) is refused
before `SendCommenced` as `ProvenUnsent(ConnectFailed)` naming only the variable
— the CLI could reflect its proxy configuration in output the adapter can only
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
constraints. A named ordinary-tool choice admits at least one proposal and
requires every proposal to carry that selected name. For a structured-output
contract, zero or several contract-named proposals remain definitive completion
material for the provider-independent structured decoder above to classify. The
decoded envelope is checked against the shared JSON nesting bound independently
of the escaped outer event; envelope decode errors are content-silent. The
envelope distinguishes completion from refusal. Within the envelope each tool
call carries its argument object as JSON text inside a string: strict
structured-output validation refuses any schema object that does not supply
`additionalProperties: false` and require all its properties, so a free-form
argument object is not expressible in the output schema and the live API rejects
one as `invalid_json_schema`. The adapter parses the string, requires exactly
one JSON object within the provider nesting bound, and passes the contained text
onward, so tool argument JSON still reaches the caller byte-verbatim when it is
credential-shape clean. Caller JSON remains raw through serialization,
preserving deep admitted values and their numeric lexemes. Buffered delivery
retains its content without deltas; streamed delivery feeds raw bounded CLI
reasoning and final-envelope text through the stateful redactor before emitting
ordered deltas and the same terminal evidence. A provider failure message
consults the same held lookbehind state before it enters provider-error
evidence: a message that extends a held credential candidate, or that arrives
during oversized-credential suppression, is suppressed whole rather than
statelessly re-redacted. Usage comes only from `turn.completed`; an omitted
cache counter remains unreported rather than becoming a reported zero.

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
process identity. The offline test binary exercises all process and evidence
paths without a live CLI or network.

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
than exactly three numeric components. The live smoke verifies that the
installed executable reports the derived version.

This mechanical binding deliberately removes the old human-attestation tripwire.
One live exchange proves that the installed CLI still works through the adapter;
it does not prove that the recorded offline fixture corpus still represents
every current CLI event shape. A fixture-regeneration or fixture-validation step
against the installed CLI is required to close that residual gap.

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
a path filter. Its secretless eligibility job checks the complete pull request
file list: no pin change is an immediate success; a fork changing
`tooling/codex-cli/**` fails with a manual-dispatch instruction; the live job is
admitted only for a changed same-repository head. GitHub independently withholds
secrets from ordinary fork `pull_request` runs. A final always-running job folds
the eligibility and conditional live results into the required check, so a
skipped or failed required smoke cannot appear green. Manual dispatch remains
available, and a path-filtered push to `main` reruns the smoke after merge.

The `codex-smoke` environment's selected branch patterns must include `main`,
`renovate/openai-codex-*.x`, and `refs/pull/*/merge`. GitHub evaluates an
environment used by `pull_request` against `GITHUB_REF`, which is the synthetic
merge ref rather than the head branch; omitting the third pattern denies the
environment to every automatic pull-request smoke even when the Renovate head
matches the second. The explicit same-repository gate and GitHub's fork secret
withholding remain the fork boundary because the merge-ref pattern itself
matches both same-repository and fork pull requests. The model dispatch still
performs no version probe: this check lives in the smoke, never in the hot path.

The smoke authenticates the CLI through its own non-interactive API-key login,
piped from an environment-scoped secret into the CLI's credential store, which
the adapter then never reads — the same ownership split as production. Because
that is an API-key login rather than a subscription one, the smoke proves the
process, event-protocol, and envelope compatibility that a version bump breaks;
it does not exercise subscription login itself, which the CLI offers no durable
unattended path for.

## Claude Code CLI provider adapter

`signalbox-model-runtime-claude-cli` wraps the Claude Code print-mode JSONL
protocol at the exact crate-local npm pin `2.1.220`. Preparation validates and
renders the full `ModelOperation`, creates a private temporary MCP catalog and
isolated settings files, and returns a one-shot capability without spawning
Claude. Execution consumes it as one fresh Unix process using
`--print --verbose --output-format=stream-json --no-session-persistence`; it
passes the rendered frontier on stdin, selects the resolved model, and never
resumes a CLI session. `SendCommenced` immediately precedes spawn and no
execution path respawns. The existing CLI-process supervision contract above
applies: the adapter owns the created process group, bounds stdout events and
retained stderr, and treats cancellation, timeout, incomplete upload, child
exit, and group cleanup as typed evidence rather than logging them.

Caller tools are MCP tools, not prompt-embedded schema arrays. The adapter-owned
stdio bridge publishes exactly the operation's tool definitions plus any
structured-output contract under the private `signalbox_tools` server. It never
executes a caller tool. For each `tools/call` it returns the fixed
acknowledgement that Signalbox recorded the proposal; Claude consequently emits
a typed assistant `tool_use` followed by a user `tool_result`, and the adapter
returns the proposal for external authorization and execution. A controlled
`SessionStart` hook waits for a private readiness marker written only after the
bridge has answered `tools/list`. The bridge accepts exactly the MCP
`2025-11-25` initialization protocol observed from Claude Code `2.1.220`; its
`tools/call` request carries the declared name and object arguments, and its
fixed result returns one text content block. This closes the print-mode
discovery race: the accepted `system/init` must report that server `connected`
and its `tools` set must equal the qualified declared MCP surface before any
assistant content is admitted.

The invocation excludes ambient settings, sessions, slash commands, browser
integration, plugins, and built-in tools. `--tools` selects an empty built-in
surface; `--disallowedTools` also names every built-in reported by isolated
2.1.220 (`Task`, `Bash`, the `Cron*` tools, `DesignSync`, `Edit`, worktree,
monitoring, notebook, notification, read/remote/report/scheduling/messaging,
`Task*`, `ToolSearch`, web, workflow, and write); and `--allowedTools` contains
only the qualified declared MCP names. `dontAsk` is used because no undeclared
capability may become an interactive permission question. The initial event must
also report no slash commands, skills, or plugins and must identify Claude Code
`2.1.220`; any mismatch is stream-protocol boundary loss, not a relaxed
invocation.

The pinned stream establishes correlation and reported-model evidence through
`system/init`. Assistant `text`, `thinking`, `redacted_thinking`, and `tool_use`
blocks become typed observations and assistant parts. A tool proposal must name
the private MCP namespace, match a declared schema name, carry a unique nonempty
id and object arguments, and receive exactly one matching user `tool_result`
whose sole text block is the fixed acknowledgement. Only a terminal `result`
event can establish success or refusal; an error `result` and a nonzero process
exit produce typed provider-error evidence. Exit zero without it is
`BoundaryLoss(StreamEndedWithoutTerminalMarker)`; malformed or contradictory
JSONL is `BoundaryLoss(StreamProtocolViolation)`; and prose alone never becomes
terminal evidence. A success must satisfy the operation's any/named tool choice,
with a structured-output contract represented as the required named MCP tool.
Provider usage is retained only where the CLI reports it.

The adapter accepts only its configured non-secret `CredentialReference` and
leaves subscription-login resolution inside Claude Code. It clears the child
environment and forwards only home/Claude-config, executable and temporary path,
XDG, locale/terminal, certificate, and credential-free proxy values; proxy
userinfo and unusable credential-home paths fail before spawn. It never locates,
reads, copies, or logs a credential store. Provider-controlled text,
identifiers, errors, reasoning, and tool JSON pass through the same
credential-shape and cross-fragment redaction discipline as the Codex CLI
adapter before observations or terminal evidence leave the crate.

The adapter crate does not compose itself into signalboxd and defines no
provider-selection or configuration mapping.

## Credential-access boundary

The in-process boundary implements the access-port rules of the credential
lifecycle record (INV-035); channels, delivery, and rotation policy are
[configuration-and-credentials](configuration-and-credentials.md) scope.

- `CredentialReference` is the non-secret durable name; it is safe in errors and
  configuration. `CredentialValue` is the boundary value: no `Display`, no
  serialization, `Debug` always redacted. `expose_bytes` is the sole read path;
  the direct HTTP adapters call it for exactly two purposes — building request
  authentication and seeding the credential-redaction machinery that scrubs
  provider-controlled output.
- Direct HTTP adapters call `CredentialAccess::resolve` during preparation of
  each physical request; nothing is cached. Why: per-request resolution makes
  rotation visible without a daemon restart. Resolution races the cancellation
  signal so a blocked read cannot hold a cancelled operation. Failures are
  reference-only (`Unmapped`, `Unavailable`, `Unreadable`) and never contain
  secret bytes.
- The production implementation is signalboxd's `FileCredentialAccess`: each
  resolve rereads the key file named by `ANTHROPIC_API_KEY_FILE` and feeds the
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
  a private-key PEM block is consumed through its matching end marker whether or
  not an assignment introduces it; credential labels are recognized in their
  space-separated spellings as well as their underscore, hyphenated, and
  concatenated ones; and JSON identity/session-token members are included.
  Envelope-decode errors are content-silent rather than embedding a rejected
  provider value. Why: subscription authentication remains wholly inside the
  intended CLI control surface while credential-shaped reflection still fails
  closed.

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
contract openly declines to cover. Fail-closed suppression is absorbing for the
sink's lifetime: usage reports, other fact boundaries, and terminal flushes
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
persistence adapters, and signalboxd telemetry: five scheduling, model-call, and
tool-loop error families (startup scan, turn activation, eligibility sweep,
model-call repository, and tool-loop repository) map into `OperatorFailureClass`
through the `ClassifyOperatorFailure` trait, exposing a user-content-free
classification to shared telemetry while the underlying error keeps its
diagnostic detail internally. The four classes:

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

The class states only how bad a failure is. The orthogonal sanitized cause code
stating *what happened* — carried by the model-call bridge, reusing this page's
`ProviderErrorKind` vocabulary verbatim for definitive provider errors — is
owned by [model-call-execution](model-call-execution.md#operator-diagnostics).

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
  vocabulary-total variants no in-repository adapter constructs today.
- The three-kind consumer allowlist (provider adapters, the
  `model-provider-runtime` bridge, the daemon composition root) is a review-time
  contract only; no manifest allowlist check enforces it.
- [Identity, credentials, and resource governance](../open-questions.md#identity-credentials-and-resource-governance)
  owns controlled provider-proxy and private-root support.
