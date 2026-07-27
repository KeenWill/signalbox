# Model-runtime substrate

This page specifies the Layer-1 typed model-runtime boundary as implemented in
`crates/model-runtime`, `crates/model-runtime-anthropic`,
`crates/model-runtime-openai`, and `crates/model-runtime-codex-cli`, verified
against the implementing stack through PR #183
(`agent/provider-call-security-parser`) plus the Codex CLI adapter stack (PR
#264, `agent/codex-cli-wrap`, and PR #268, `agent/codex-cli-pin-smoke`). The
`signalboxd` names this page states for the composition root, its telemetry, and
the production `FileCredentialAccess` were verified through PR #258
(`agent/signalboxd-rename`); the Anthropic adapter's server-side
`fallback`-block recognition was verified through PR #280
(`agent/provider-identity-normalization`). The five persistence-repository
families in the operator-failure inventory were verified through PR #288
(`agent/audit-fix-docs-coherence`). The streamed-delivery bridge and ephemeral
text-delta projection were verified through PR #300
(`agent/token-level-streaming`). It covers the provider-neutral operation,
observation, and evidence vocabulary; SSE framing; structured-output and tool
decode; `ScriptedModel`; the three provider adapters; and their credential
boundaries. Layer-2 authorization and evidence classification
([model-call-execution](model-call-execution.md)), credential channels,
delivery, and rotation discipline
([configuration-and-credentials](configuration-and-credentials.md)), and the
authoritative transcript commit
([sessions-and-transcript](sessions-and-transcript.md)) are owned by those
companion pages. This page also owns the shared
[operator failure taxonomy](#operator-failure-taxonomy) — defined in
`crates/application` and consumed by signalboxd telemetry.

## Boundary and crate layout

The runtime layer is four library crates, hand-rolled per the 2026-07-20
[decision-ledger entry](../decisions.md) that closed the substrate's
vendor-versus-hand-roll question: one provider-neutral core crate plus
separately named provider adapters, with SerdesAI as a design reference only.
`signalbox-model-runtime` is the shared vocabulary; the Anthropic and OpenAI
adapters additionally own their HTTP, TLS, and serde dependencies, while the
Codex CLI adapter owns only its subprocess, temporary-schema-file, signal, and
serde dependencies. Test helpers ship in no built library artifact.
`crates/domain`, `crates/application`, and `crates/persistence` declare no
dependency on any runtime crate, and no runtime type appears in a domain or
application signature (INV-002, INV-005); the approved runtime consumers are the
adapter crates, the `crates/model-provider-runtime` bridge — whose
`RuntimeModelCallProvider` implements the application's `ModelCallProvider` port
over any `ModelRuntime<ModelCallId>`, depending on both crates so the dependency
arrow points from the bridge into application, never from application into the
runtime — and the daemon composition root (see Open edges). The Cargo manifest
is the enforcement mechanism: an undeclared dependency fails the workspace
build. Why: manifest-visible boundaries make a boundary violation a reviewable
diff instead of a silent import.

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
user configuration and rule files, disables the shell, unified-exec, and
skill-search features — the last so ambient `SKILL.md` discovery cannot add
instructions the caller never rendered — sets the project-instruction byte
budget to zero, and uses the read-only CLI sandbox. Strict configuration turns
an unavailable control into a closed failure instead of silently relaxing this
invocation boundary. Before spawn it clears the parent environment, then copies
only its explicit home/Codex-home, executable and temporary path, XDG,
locale/terminal, certificate, and proxy allowlist; unrelated service variables
do not reach the CLI. It neither resumes nor persists a Codex thread. Why: a
fresh ephemeral invocation keeps provider session state out of memory, and the
caller supplies the complete conversation frontier instead of an in-memory
resume pointer. The read-only sandbox and working root are the adapter's
filesystem boundary; Unix process-group supervision bounds descendant lifetime,
so construction rejects hosts where that supervision is unavailable. Stronger
host isolation is later composition work, not an adapter claim.

`SendCommenced` immediately precedes spawn. Spawn failure is
`ProvenUnsent(ConnectFailed)`; after successful spawn no path respawns the CLI.
The first `thread.started` establishes the exchange and its thread id becomes
the provider request id. Unknown top-level events and unsupported item kinds are
additively tolerated within the byte and JSON-depth bounds. Known item lifecycle
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

An offline test asserts that the pinned version equals the adapter's supported
version. It runs unconditionally in the ordinary Rust check, so a pin bump fails
that check until the supported-version constant moves with it — which is what
forces the fixture corpus to be re-examined against the new release rather than
inherited.

The compatibility smoke is the second gate: one exchange against the cheapest
model the smoke credential can address, run through this adapter with the real
pinned executable. Which models a credential may address is account-scoped, so
the model is a configured value with the cheapest advertised model as default.
Before spending anything it asserts that the executable's reported version
equals the supported version, and an unreadable, unparsable, or mismatched
version fails the smoke rather than skipping it. Why: evidence recorded against
a version that never ran is worse than no evidence. The smoke then asserts only
the protocol surfaces a version bump moves — the thread identifier reaching the
exchange facts, the terminal usage counters, and the response envelope decoding
as a completed or refused terminal outcome — and nothing about answer quality.
It never runs on a pull-request event, so no fork can reach its credentials; it
is dispatched manually, including against a pin-bump branch to verify that bump
before it lands, and runs automatically on `main` only when the pin manifest or
its committed lockfile changes. The model dispatch itself still performs no
version probe: this check lives in the smoke, never in the hot path.

The smoke authenticates the CLI through its own non-interactive API-key login,
piped from an environment-scoped secret into the CLI's credential store, which
the adapter then never reads — the same ownership split as production. Because
that is an API-key login rather than a subscription one, the smoke proves the
process, event-protocol, and envelope compatibility that a version bump breaks;
it does not exercise subscription login itself, which the CLI offers no durable
unattended path for.

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
  and JSON identity/session-token members are included. Envelope-decode errors
  are content-silent rather than embedding a rejected provider value. Why:
  subscription authentication remains wholly inside the intended CLI control
  surface while credential-shaped reflection still fails closed.

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
