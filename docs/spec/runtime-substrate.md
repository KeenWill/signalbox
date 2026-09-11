# Model-runtime substrate

The model-runtime substrate is the typed boundary through which the daemon sends
one prepared model operation to a provider and receives typed evidence of what
happened.

## Overview

The substrate is `crates/model-runtime` plus four adapter crates: Anthropic and
OpenAI over HTTPS, and the Codex CLI and Claude Code CLI as supervised
subprocesses. The core crate owns the provider-neutral vocabulary: the
operation, the observations emitted while a call runs, the terminal evidence a
call ends in, SSE framing, structured-output and tool-argument decoding, the
credential access boundary, and the `ScriptedModel` fixture. Authorization and
failure classification belong to
[model-call-execution](model-call-execution.md), credential channels and
rotation to [configuration-and-credentials](configuration-and-credentials.md),
and the transcript commit to
[sessions-and-transcript](sessions-and-transcript.md). The daemon reaches the
runtime through the `RuntimeModelCallProvider` bridge in
`crates/model-provider-runtime`, which implements the application's model-call
port over any `ModelRuntime` and its input-token counting port over an adapter
that counts a prospective operation's rendered input without a generation
request, sending the same prompt- and cache-affecting controls the generation
request would carry.

An input-count failure carries caller correlation and typed stage evidence,
including a closed credential failure or HTTP status where available, without
provider text or credential material.

Anthropic Messages generation and input-count requests do not enable tool-result
context editing; no durable model-call fact represents its applied edits.

Anthropic generation and input-count requests mark nonempty system text and the
last eligible text or tool block in the penultimate message for ephemeral prompt
caching; thinking and opaque compaction blocks receive no explicit breakpoint.

Caller identity crosses the boundary as an opaque correlation parameter carried
by `ModelOperation`, every `Observation` and the final `TerminalReport`; the
runtime imports no domain identifier. An operation names its target as two
caller-supplied facts, the requested selection and the resolved target. The
provider-reported model is a third fact, which the adapter surfaces through an
observation and the terminal evidence.

`ConversationMessage` is the closed typed history of text, tool calls and
results, thinking with an optional signature, redacted thinking, and opaque
provider-compaction blocks and provider-reasoning items. Completed terminal
evidence uses the same ordered response-part vocabulary for text, thinking,
redacted thinking, tool proposals, suppressed tool calls, provider compaction,
and provider reasoning. A provider-compaction part carries one complete raw JSON
content block unchanged across the runtime and bridge; adapters that cannot
replay that provider-qualified part reject the operation before send. A
provider-reasoning part carries a complete raw reasoning item with encrypted
content; its JSON bytes remain unchanged through the runtime and bridge. Replay
carries the producing call's durable effective target and pinned credential
reference. A missing producing-target mapping fails input estimation and request
preparation before provider interaction. Streaming takes those bytes from the
completed output item. Terminal reasoning may omit encrypted content; when
present, it must match the completed item's encrypted content. A credential in
the item rejects the whole evidence with `credential_in_provider_reasoning`.

`ModelRuntime` has two stages. `prepare` does all work that needs no provider
traffic and returns an opaque one-shot capability or a typed failure.
Preparation distinguishes a trustworthy failure, an unsupported operation or an
unusable or unavailable credential, from a construction defect; the bridge maps
only the trustworthy failure to a known failure and fails closed on a defect.
`execute` consumes that capability, performs at most one provider interaction
and returns a terminal report. The unit of irrevocable dispatch is one HTTPS
request for a direct adapter and one process spawn for a subprocess adapter. The
caller side of that boundary is [model-call-execution](model-call-execution.md)
scope.

Observations are transient progress facts emitted during execute: the request
about to reach the transport, the correlated exchange opening, the
provider-reported model, text, thinking and tool-argument deltas, tool
proposals, usage and the finish reason. An adapter puts the provider's request
or session identifier, and the HTTP status where there is one, in the exchange
facts it reports when the exchange opens and retains in terminal evidence. The
send-commenced fact marks the acceptance boundary: from that point the provider
may have accepted the request.

Terminal evidence divides three ways. Definitive evidence is a completed
response, a provider refusal, a classified provider error with its native facts
retained, or a confirmed cancellation. Proven-unsent evidence says acceptance
was impossible: cancelled before send, a connection that failed before any
request byte, or a provably unacceptable incomplete write. Boundary loss says
the request crossed or may have crossed the acceptance boundary and no
definitive response exists; it carries a typed loss cause, the partial facts
observed before the loss, whether response content was observed, and whether a
tool call had opened in the material the adapter decoded. A provider error may
also carry an adapter-owned proof that the provider never accepted the request.
[Credential availability](credential-availability.md) owns successor policy for
classified failures.

`SseFraming` is the provider-agnostic incremental parser both HTTP adapters
build on, from transport byte chunks to event-stream records.
`StructuredOutputContract` carries a name, description and JSON Schema,
generated from a Rust type or supplied explicitly, that every adapter realizes
as one tool proposal under a reserved name: OpenAI forces it through tool
choice, Anthropic asks for it by instruction, Codex renders it into the prompt
with an outer response schema, and Claude Code adds it to the private MCP
catalog and forces it as the named tool choice. One provider-independent decoder
enforces exactly one proposal.

The Anthropic and OpenAI adapters share one shape: at most one POST per
operation, hand-written wire types with no provider SDK dependency, and typed
evidence out. The two CLI adapters share the process supervision in
`cli_process.rs` and a cleared child environment. `ScriptedModel` replays
caller-declared scripts of observation facts and exact terminal evidence through
the real runtime surface, so fixtures declare their result rather than simulate
one. The page also carries the one cross-page rule for `OperatorFailureClass`,
the closed severity classification defined in `crates/application`.

## Design decisions

The Cargo manifest is the boundary's enforcement: an undeclared dependency fails
the workspace build, so a boundary violation is a reviewable diff instead of a
silent import.

The runtime holds no durable state, makes no lifecycle decisions and performs no
logging. A rejected Codex completion envelope carries a closed rejection stage
as boundary-loss evidence, including duplicate-member rejection.

The `RuntimeModelCallProvider` bridge sets every operation it prepares to
streamed delivery, and buffered delivery remains available to other direct
callers. Why: the bridge is the one composition point that can request live
observations without changing the application port.

Neither HTTP adapter requests server-side model fallback, so a provider marker
announcing that another model continued the turn is evidence the resolved target
did not serve it; the marker crosses the boundary only through the reported
identity, and this layer has no substitution variant of its own.

A subprocess adapter cannot observe or govern the wrapped client's internal HTTP
attempts; they are provider-internal, like server-side attempts behind one
direct request.

Quota exhaustion is a distinct error kind from rate limiting, so a billing
condition is never treated as retry-later backoff.

Claude Code failures classify from the closed native result subtype and reported
HTTP status, which takes precedence. Generic or unknown subtypes without a
classifiable status remain unrecognized; result text and stderr do not classify
failures.

The confirmed-cancellation variant exists to keep the evidence vocabulary total;
no adapter in the repository constructs it.

The HTTP adapters never construct the incomplete-write unsent cause, because an
HTTP server can act before end-of-request framing.

Refusal evidence carries a typed reason: content policy, cyber policy,
misalignment, or unspecified. Policy refusals carry no credential-rejection or
non-acceptance proof. HTTP refusals with reported usage remain refusal evidence.
Without reported usage, an HTTP refusal remains refusal evidence only when it
carries a validated provider-compaction block with non-null replacement content
and retained input and output counts; otherwise execute returns an unrecognized
provider error.

The Claude Code CLI never supplies the non-acceptance proof.

SSE id and retry fields are parsed and dropped, because they exist for stream
resumption and resuming would be a second request.

Decoding never performs a model call; a repair attempt is a new, explicitly
authorized operation owned by the caller.

This layer contains no tool execution: a decoded proposal is data for a
separately authorized tool request, and the Claude Code adapter's MCP bridge
answers every tool call with a fixed acknowledgement so the proposal returns for
external authorization.

`ScriptedModel` ignores the cancellation signal in both stages, so an
already-fired signal never manufactures cancelled or proven-unsent outcomes from
a fixture; scripted evidence is declared, never inferred from timing.

The HTTP clients disable ambient system and environment proxies and expose no
proxy configuration, so credentials and content never traverse an
operator-unreviewed intermediary.

Each HTTP adapter's base URL must use HTTPS, or plain HTTP to a loopback IP
literal, and carry no userinfo, query or fragment; construction rejects anything
else.

Idle-connection reuse is disabled so every send opens a fresh connection; this
removes stale-connection replay and lets a connect failure claim proven-unsent.

The OpenAI adapter sends `POST /v1/responses` with `store: false` and no
stored-state chaining. It rejects non-empty stop sequences during settings
validation and before send as `UnsupportedOperation`. Configuration validation
and preparation require `max_output_tokens` to be at least 16. A completed
response maps to end of turn or tool use according to its output;
`incomplete_details.reason` maps `max_output_tokens` to the output ceiling and
`content_filter` to refusal, while other reasons remain unrecognized.

The Codex CLI adapter neither resumes nor persists a Codex thread; each call is
a fresh invocation given the complete conversation frontier, so provider session
state stays out of memory.

Claude Code calls with conversation history fork a disposable native transcript
of the complete canonical frontier, preserving distinct assistant message groups
for native compaction. Historical tool calls and results remain canonical JSON
text; images retain their message and part order. Request and growth
measurements include the native framing, and the private transcript is removed
with the request’s support directory.

Unix supervision contains the process group the adapter creates, so construction
rejects hosts without process-group control; containment beyond that group
belongs to host isolation, not to the adapter.

The pinned Codex CLI injects its own agent-identity instructions around the
stdin prompt, so the adapter's preamble is the first tool statement the adapter
controls, not the first text the model sees; that preamble names the serialized
tools array as the single authority on which tools exist, narrowed or extended
only by tool choice and structured output.

The Codex CLI's reasoning-output counter and its additive total-token siblings
have no Signalbox usage axis and are folded into no other field.

Exact-value redaction covers the exact credential value, its JSON-string-escaped
form and chunk-split prefixes of it; a reflection the provider re-encodes in any
other form passes through unscrubbed.

Each CLI adapter's supported-version constant is only a claim until three
statements agree: the version pinned for installation, the version the adapter
covers, and the version actually invoked.

The Codex smoke compares the pin's checked-in schemas for every decoded
app-server notification, response, and JSON-RPC envelope with the adapter's
consumed fields, enum members, and required fields. Consumed fields must remain
decoder-compatible, adapter-required fields must remain required, and turn
statuses must match. Consumed turn-item discriminators remain required strings;
agent-message items preserve their consumed fields. Tagged error objects contain
only their tag. Compatible additions are reported; consumed fields and error
members must remain present. Committed schema fixtures must match the pinned
files byte for byte.

The compatibility smokes assert nothing about answer quality.

A smoke's required aggregate gates merge for a pull request that changes the
paths its gate names, an exception to the repository default that a credentialed
live smoke never gates merge. The Anthropic, OpenAI and Claude Code gates name
their adapter crate and workflow file, the Claude Code gate also the shared CLI
supervision module; the Codex gate names its adapter crate, the shared CLI
supervision module, the `tooling/codex-cli` pin directory and its workflow file.

A twice-daily schedule runs each smoke as a provider-drift canary between
adapter-touching changes, spending one paid exchange per run.

No job-level concurrency group serializes the live exchange, because a fixed
shared group would let an unrelated run evict a required check's queued slot;
required-check integrity outweighs the fraction of a cent serialization would
save.

## Boundary contracts

The daemon refers to a credential by its non-secret name everywhere except at
the point of use. OAuth authorization tables hold the refresh and identity
tokens the delivery needs; no credential value appears in a log, an error, or
any other durable record. No credential file path or database URL appears in a
log, an error, or a durable record. Provider output follows the adapter
credential boundary. A credential for one repository never authorizes a request
to another.

OAuth access and identity tokens seed exact-value redaction before scratch-home
writes. Raw and JSON-escaped token forms are scrubbed across child-output chunks
before adapter decoding, truncation, observations, or terminal evidence. A
failure to install this delivery fails preparation before spawn.

`crates/domain`, `crates/application` and `crates/persistence` declare no
dependency on any runtime crate, and no runtime type appears in a domain or
application signature.

`prepare` performs all validation, translation, serialization, credential access
and request construction with no provider traffic except OAuth delivery
exchanges with the authorization server, rejecting duplicate ordinary tool
names, an ordinary tool whose name equals the structured-output contract name,
and a named choice of an undeclared tool before any send. `execute` consumes the
capability, performs at most one provider interaction, emits observations
synchronously and in order, and always returns a terminal report. Nothing in
this layer retries, falls back, or repeats its unit of dispatch after the
provider could have accepted it; the attempt-level retry rule is
[model-call-execution](model-call-execution.md)'s. In a subprocess adapter the
send-commenced fact immediately precedes spawn, a spawn failure is proven
unsent, and after a successful spawn no path respawns the CLI.

In both stages the pending work is polled before the cancellation signal, so a
result already available in the same poll wins over cancellation. Once a
provider terminal marker is observed, a later cancellation cannot replace that
definitive evidence. During execute cancellation is best-effort: the adapter
stops local work and reports how far the request provably progressed, never
claiming provider-side work stopped. In a subprocess adapter cancellation before
spawn is proven unsent; after spawn the adapter interrupts the process group,
holds the unreaped leader through a grace period, kills the group and reports
boundary loss; dropping the execution after spawn kills the still-owned process
group before the child handle drops. On an ordinary exit the executor kills the
remaining process group before it reaps the leader. After a stdin write failure
a provider failure remains definitive, but a nominal completion becomes boundary
loss because the adapter cannot prove the full frontier reached the CLI.

Settings are provider-enforced request controls unless an adapter records a
capability-limited advisory exception; an adapter never presents prompt
instructions as hard transport controls. Under the Anthropic adapter, any-tool
and named tool choice and the output contract are advisory, and a temperature or
top-p setting fails preparation rather than being dropped. Under the Codex CLI
adapter, the output-token ceiling, temperature, top-p and stop sequences are
rendered into the prompt as advisory context. Under the Claude Code CLI adapter,
temperature, top-p and stop sequences are advisory, the output-token ceiling is
enforced through the child environment, and a service tier is always rejected. A
caller that requires provider-enforced settings must not select an adapter whose
exception covers them. Reasoning level, fast mode and service tier follow the
mappings [model-session-settings](model-session-settings.md) owns. Preparation
fails before any send when the wire representation cannot preserve the order of
typed conversation parts. Adapters send exactly the resolved target as the
provider model parameter, never the requested selection, and surface a
provider-reported identity as soon as it is observed without fabricating a match
or mismatch. The Claude Code CLI adapter reports the model its init event
announces, and no observation or record carries the provider-resolved model a
later assistant event names.

Observations are transient progress facts, never canonical transcript history.
For a correctly correlated text delta the bridge copies the adapter-sanitized
text unchanged to its best-effort presentation sink and still retains the exact
observation on the evidence path. Presentation delivery neither alters nor
replaces the terminal report, and sink loss cannot change terminal
classification. The HTTP adapters redact credentials before emitting a delta;
the bridge and daemon attempt no second redaction.

Terminal evidence is typed so the caller classifies without string matching;
strings appear only as retained detail inside already-classified variants. Each
adapter owns an exhaustive native mapping into the shared provider-error kind,
which lives in the core crate, and unknown material classifies as unrecognized
with its native facts retained rather than guessed at. In both HTTP adapters an
unauthorized status is credential rejection before the body is consulted, and
otherwise a recognized native code outranks a recognized type, which outranks
the status. An error record that follows the provider's finish marker and names
no classifiable failure is stream-protocol loss, while a classified one stays
definitive and outranks the finish. A provider-directed retry delay, decoded
from the HTTP `Retry-After` header or the Codex app-server's typed rate-limit
snapshot, rides the provider-error evidence into durable availability backoff
except for quota failures, which carry the evidence without affecting backoff.
The header admits delay-seconds and HTTP-date forms; malformed values carry no
evidence. For rate-limit and quota failures, Codex uses the latest reported
reset among fully consumed primary and secondary windows; null windows in sparse
updates preserve prior observations. Past HTTP dates and Codex reset instants
saturate to zero delay.

Classified availability failures admit retry and configured rotation without
non-acceptance proof, including status-derived fallback classifications and
mid-stream error records. A received HTTP error status remains classified when
its body is lost. A definitive classified error after response content is still
a known failure. A timeout or transport break before observed response content
follows transient retry policy; after content it retains ambiguous-loss
handling. Content progress is retained in buffered and streamed delivery. A
buffered body loss records whether any body bytes were received, including bytes
that exceed the response limit. The provider error's `non_acceptance_proven`
field retains the adapter's protocol evidence and does not gate availability.
Codex `willRetry` events do not prevent a subsequent classified failure from
admitting a successor. OAuth delivery refreshes a rejected access token before
returning recovery evidence; the daemon records the rejection and admits a new
call on the same profile after refresh, or rotates after failed refresh or
rejection of the refreshed token. HTTP statuses carried by Codex connection and
stream errors classify 401 as credential rejection, 429 as rate limit, 500 as
provider internal failure, and 503 or 529 as overload.

A failed Codex turn whose final error is `other` retains the latest correlated
retry error's typed classification. A specific terminal error takes precedence;
retry telemetry alone proves neither terminal failure nor non-acceptance, and a
successful turn discards it. Codex connection and stream-disconnection errors
without an HTTP status retain transport loss and whether response content was
observed.

A success-status response whose body is not valid completion material is
boundary loss, never completion, and an unrecognized finish token is boundary
loss in both HTTP adapters. An Anthropic stop-sequence finish naming a sequence
the request did not declare is stream-protocol loss. A stream that ends in any
way other than its protocol's terminal marker is incomplete-stream evidence,
never silent success: a Codex CLI stream without `turn/completed` is boundary
loss, and under the Claude Code CLI only a terminal result event establishes
success or refusal, never prose; in the Claude Code adapter that loss follows a
zero exit while a nonzero exit is provider-error evidence classified from the
terminal subtype and HTTP status. Stderr remains native detail. The Codex
process exit carries no failure classification. A Codex `interrupted` turn is
boundary loss whose transport detail names that status, without cancellation or
non-acceptance proof. A Codex turn that completes without a streamed agent
message takes its response from the agent-message item in its `turn/completed`
summary under the same size and redaction checks, and a streamed message
outranks it. A finish reason observed before a stream loss is retained as a
reported finish but is not completion or refusal evidence; an unrecognized
finish reported before the envelope is validated is an envelope violation
instead, and no finish is retained. Within one adapter the buffered and streamed
decoders never disagree about an output-ceiling finish inside accumulated tool
content, which is an observed fact in both and not an envelope defect; an
unrequested Anthropic fallback block is the exception, unintelligible-response
loss in the buffered decoder and a stream protocol violation in the streamed
one.

The tool-calls-at-loss fact reports the decoded prefix and nothing beyond it:
none-opened says no tool call opened in what the adapter decoded, never that the
provider sent none. A negative is stated rather than withheld when the adapter
examined the material that could open a tool call, whether or not it accepted
that material. A tool call an earlier record already established outranks the
withholding in every adapter.

OpenAI response events other than failed terminals carry a consistent response
id and reported model. Each output index binds one item id and kind to an open
or done item state. Open content accumulates deltas and snapshot prefixes;
content done freezes its type and bytes, and item done freezes its layout.
Repeated snapshots preserve the frozen state; done items admit no further
deltas. Every observed item must fit the terminal output. An incompatible event
ends the stream with incomplete-stream evidence, retaining observed facts
without announcing completion or refusal. Output-ceiling terminals retain
completed and incomplete item content, including incomplete function arguments.
Reasoning replay retains exact item-done bytes even when terminal ciphertext
differs. Reported model and recognized terminal finish are retained before usage
decoding. A terminal response supplies completion content and its response id
becomes the provider message id. A bare `error` event or an HTTP-200 response
with `status: failed` supplies definitive provider-error evidence without
non-acceptance proof. Failed status and error are classified before ancillary
response fields. Claude Code CLI events stay bound to the initialized exchange:
the first non-diagnostic assistant event may name the provider-resolved model
and every later non-diagnostic assistant event must repeat that value, and a
result carrying a different session id is a protocol violation. Assistant
content retains its first message id. After every proposed tool has its bridge
acknowledgement, one message with a distinct id, text and optional thinking
blocks, and at least one text block may acknowledge the batch. Its content is
discarded; the reported finish must be `end_turn`, while the effective
completion of the original batch is `ToolUse`.

Claude native refusal notifications and text-only API-error diagnostics stay
bound to the initialized session and are nonterminal. Diagnostics report the
native `<synthetic>` label, which is not a provider model identity; their text
does not become assistant content. The result event supplies the outcome.

Claude CLI synthetic text-only user-role events must name the initialized
session. Their native context summaries do not become canonical assistant
content, tool acknowledgements, or terminal evidence.

Claude CLI native compaction boundaries must name the initialized session. The
adapter logs the reported trigger and pre-compaction token count; the boundary
does not replace the daemon's canonical transcript.

Usage is provider-stated only, never estimated. Each decoded usage field is
independently optional: an omitted field stays unreported rather than becoming
zero, a total-only report records nothing because no adapter distributes a
total, and no cache-creation count is fabricated. The Codex CLI's cache-write
and cached-input counts map to cache-creation and cache-read usage. OpenAI's
`input_tokens_details.cache_write_tokens` and `cached_tokens` map to
cache-creation and cache-read usage. A later usage report replaces the fields it
carries and preserves the fields it omits. OpenAI streamed success requires
reported usage and a terminal response event with matching terminal status;
message output must carry the assistant role. Anthropic requires input usage at
message start and final output usage before message stop.

The shared framer bounds every line and each record's retained content, makes a
framing failure terminal for the stream, and distinguishes a truncated final
record from a clean end. Framing results never depend on how the transport
fragments bytes into chunks, and records completed before a failure in the same
chunk are delivered alongside the failure.

The structured-output contract is a request constraint, not a response
guarantee: a nonconforming response can carry zero or several proposals, and the
provider-independent decoder enforces exactly one and distinguishes malformed
JSON, a mismatched schema and a value the caller's domain validator rejects. In
both CLI adapters a terminal response with no proposal for an any-tool
requirement, or an empty or mixed proposal set for a named one, is
unintelligible-response boundary loss before that decoder runs. A proposal's raw
argument JSON is kept verbatim and never re-serialized, and the Codex renderer
carries caller tool schemas and replayed tool arguments into the prompt as raw
JSON. The decoders impose no argument-size ceiling; the normalized-argument
ceiling [tool-loop](tool-loop.md) states belongs to the bridge, which fails the
model call as unrepresentable tool material before any tool round rather than
reaching one as invalid arguments.

Both HTTP clients force the rustls backend, select the same `ring` crypto
provider the database stack uses, verify certificate and hostname against
platform trust roots, require TLS 1.2 or newer, and carry no custom-root or
verification-bypass surface. Redirect following is disabled, so a redirect
surfaces as unexpected-status boundary loss rather than a hidden second POST.
The whole-exchange timeout is a required deployment bound supplied to both HTTP
adapters and to both subprocess adapters; it covers connection establishment
through the complete buffered body or streamed terminal record, and a `none`
setting leaves the exchange unbounded. Callers may configure a shorter connect
timeout; a connect timeout is proven unsent, while a whole-exchange timeout
after send is boundary loss. Success is HTTP 200 only; another 2xx is not
terminal success.

The HTTP adapters bound all provider-controlled response input before it can
accumulate into parsed or retained output, and complete records inside the byte
budget are processed before a coalesced over-budget suffix, so transport
batching cannot erase earlier evidence or a terminal marker. A buffered body
past that bound is response-body loss, and a streamed response past it is a
stream protocol violation. The CLI adapters bound each event and the retained
stderr evidence, not the decoded total across an exchange. Before serde sees a
buffered success body or a JSON stream record, a shared allocation-free scanner
rejects JSON nested beyond a fixed depth, unknown fields and raw material
included; unknown fields stay tolerated for additive provider evolution under
the same byte and nesting limits as known ones. The Anthropic and Codex CLI
decoders also tolerate an unknown event name or notification method,
respectively, and discard its bounded payload without typed parsing, while the
OpenAI decoder parses every record's payload whatever its event name and
dispatches on the payload's `type`, rejecting an unrecognized type; the Claude
Code CLI decoder rejects an unrecognized top-level event type as a stream
protocol violation. Malformed or over-depth JSON in a success body is
unintelligible-response boundary loss. In all adapters, over-depth streamed
material and malformed known-event JSON are stream protocol violations. Both CLI
decoders reject a syntactically valid record that repeats an object member, at
any nesting depth, as a stream protocol violation. A malformed or over-depth
body attached to a definitive error status cannot erase that exchange: the
adapter falls back to status classification with bounded sanitized native
material. An Anthropic thinking block must close with exactly one nonempty
integrity signature, and an empty signature on the opening block is a
placeholder rather than a delivered one.

Each CLI adapter mechanically disables every native facility of the pinned CLI
that could add a model-visible tool, an instruction source, an external
interaction or delegated execution, or that could replace the pinned executable;
prompt text is never a capability boundary. Codex uses root-level validating
`--disable` flags and
`app-server --stdio --strict-config --ignore-user-config --ignore-rules`, with
one ephemeral thread and one turn per process. Before spawn the adapter clears
the parent environment and copies only its allowlist of home, executable and
temporary paths, XDG, locale, terminal, certificate and proxy variables. An
allowlisted proxy value that embeds URL userinfo or is not UTF-8 refuses the
exchange as proven unsent before spawn, because the child would receive that
credential verbatim. A copied credential home is absolutized against the
parent's working directory first, and one that is empty or resolves to no
absolute path refuses the exchange the same way, because the child would
otherwise select an ambient login store under its own working directory. Under
the Claude Code CLI the invocation excludes ambient settings, sessions, slash
commands, browser integration, plugins and built-in tools and allows only the
declared MCP tool names; the initial event must report a tool inventory equal to
the declared MCP surface with the private tools server connected, no slash
commands, skills or plugins, and must identify the pinned version, and any
mismatch is stream-protocol boundary loss, not a relaxed invocation. The one
native hook the adapter installs is a session-start command that runs the
private MCP bridge and holds the CLI until that bridge has served its tool list.
A Claude Code tool proposal must name the private MCP namespace, match a
declared schema name, carry a unique nonempty id and object arguments, and
receive exactly one matching acknowledgement result. When an operation carries
an explicit catalog-governed control, a CLI adapter validates the exact target
capability record before checking its ambient-login reference and never
delegates validation to the CLI.

`expose_bytes` is the sole read path on a credential value, and the HTTP
adapters call it for exactly two purposes: building request authentication and
seeding credential redaction. The HTTP adapters and Claude Code file delivery
resolve the credential through `CredentialAccess` during preparation of each
physical request and cache nothing, so rotation is visible without a daemon
restart; resolution races the cancellation signal so a blocked read cannot hold
a cancelled operation. Composition supplies each adapter with the complete map
of file profile references declared for that adapter, and a profile declared for
another adapter stays unmapped. An HTTP adapter scopes the resolved value to the
one prepared request as a sensitivity-marked header and performs no second
lookup; Claude Code file delivery retains it in the one-shot capability instead.

The Codex CLI adapter accepts the configured non-secret credential reference,
keeps every direct credential value outside the cleared child environment, and
does not read, copy, log or transport the CLI credential store. Each operation
receives a private `CODEX_HOME` with an empty `config.toml` and an `auth.json`
symlink to the selected profile's home or the ambient Codex home; only the CLI
opens the login store, and auxiliary state stays in the private home. An ambient
Claude Code adapter likewise accepts only its one configured reference. Under
file delivery the Claude Code adapter replaces only the already allowlisted
configuration-directory variable with a private request-scoped directory, never
adds the API-key variable to the child environment, and removes the directory
when the capability drops. It creates the credential, settings and helper files
owner-only, and the helper that delivers the key runs under a fixed shell
interpreter using only builtins, never an executable resolved through the search
path. A file credential value that is empty, not UTF-8, or carries a NUL is
unusable and fails preparation before spawn.

An adapter that reads a credential value redacts that exact value from evidence
text before truncation, tool-argument JSON JSON-aware, and streamed deltas with
a held-back trailing prefix. When ordering forces a held prefix out, it is
replaced with a redaction marker. Under an ambient CLI login no credential value
crosses the adapter boundary, so CLI-controlled text and JSON pass through
unmodified.

The exact-value observation sink also checks each forwarded fact with a shadow
predicate over its canonical JSON bytes and decoded content, using the same
credential-length-minus-one byte lookbehind. The content projection joins string
leaves across fields and facts, joins decoded complete embedded JSON leaves with
ordinary content in order, decodes malformed proposed arguments, and
reconstructs text, thinking, and JSON-escaped argument streams by kind and part
index within one request. A match increments
`signalbox_credential_redaction_disagreements_total` on the daemon's Prometheus
surface and fails runtime unit tests; production forwarding retains the existing
redaction behavior.

Each CLI adapter's build derives its supported-version constant from the exact
version in its pin manifest, so the manifest is the sole source. The daemon
composition probes only the Codex CLI executable and logs its installed version
and SHA-256. A failed probe or pin mismatch makes the Codex adapter unavailable
with a typed cause while the daemon and other adapters remain available; nothing
probes the Claude Code executable before an exchange begins. Before spending
anything, the Codex smoke asserts that the reported version equals the supported
version and compares the CLI's complete feature list, including stage and
default, with an exact classified inventory. In every smoke workflow, forks are
excluded by GitHub secret withholding and by three explicit repository-name
comparisons, no credential is echoed or passed in argv, and the test binary is
compiled before any step carries the credential. The direct-HTTP smokes
reference their secret only in the step that spends the exchange, and that step
runs the compiled binary directly, so no build script or procedural macro runs
while the key is readable. Each CLI smoke references its secret in a setup step
before the exchange: the Claude smoke writes it to a file and gives the exchange
step only that path, and the Codex smoke pipes it on stdin into the CLI's own
login, which writes the credential store the CLI then reads. The exchange step
then invokes the compiled test through Cargo, so its freshness check runs after
credential setup. Each CLI smoke removes what it materialized when the job ends.

`OperatorFailureClass` states only a failure's severity and carries no user
content, so shared telemetry may emit it while the underlying error keeps its
diagnostic detail internally. The class is one of infrastructure, which states
whether the commit is ambiguous, fail-closed corruption, identity collision, or
a caller or hub defect. A failure stops the daemon only when it has no session
scope. Session-scoped recovery suspends only the affected session. The daemon
distinguishes ambiguous infrastructure failures from nonambiguous ones. The
sanitized cause code stating what happened is owned by the page that owns the
behavior raising it: [model-call-execution](model-call-execution.md) for
provider and model-call causes,
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md) for
turn-liveness causes.

## Planned

- Workspace-instruction transport: a typed instruction region on the operation,
  admitted only where the resolved target and adapter mapping declare
  typed-system capacity and mapped only to the provider's instruction transport
  ([design](../design/runtime-substrate.md)).
- Codex CLI file credential delivery: the configuration grammar admits it and
  composition rejects it as undelivered
  ([design](../design/runtime-substrate.md)).
