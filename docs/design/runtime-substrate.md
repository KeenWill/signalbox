# Model-runtime substrate design

This document holds committed design that is not built; it extends
[runtime-substrate.md](../spec/runtime-substrate.md).

## Goal

Four capabilities extend the runtime boundary. The operation gains a typed
workspace-instruction region so daemon-authored instructions reach a provider's
instruction transport without being mixed into ordinary system text. The Codex
CLI adapter gains file credential delivery, so a deployment can run it with a
daemon-held API key instead of the CLI's ambient login. The Codex CLI adapter
also gains OAuth delivery, with exact-value redaction of every delivered token
installed before the child starts. The direct HTTP adapters gain a way to
surface refusal evidence once a transport can prove the response followed the
complete request upload.

## Design

Workspace-instruction region. `ModelOperation` carries
`workspace_instructions: Option<WorkspaceInstructionRegion>` beside the system
text and the conversation history. Validation rejects a present region unless
the resolved target and the adapter mapping both declare `typed_system` support
and byte capacity. Each adapter maps the region only to its provider's
instruction transport, after the system prompt and before conversation messages,
and fails before send when that mapping cannot preserve the boundary. The
region's bytes, preamble and wrappers are owned by
[workspace-instructions.md](../spec/workspace-instructions.md); the runtime
neither parses nor rewrites them.

Codex file delivery. The configuration grammar admits a file delivery for the
Codex adapter, and composition rejects it as undelivered. Delivery resolves the
selected profile during preparation and admits only the exact `OPENAI_API_KEY`
env_key; every forwarded or process-control name is invalid configuration. The
selected value is an operation-scoped child override added after the parent
environment is cleared. It is absent from argv, logs, debug output, retained
evidence and every later spawn, and it seeds the adapter's exact-value redaction
before any provider-controlled output leaves the crate. The override does not
weaken ambient mode's credential exclusion.

Codex OAuth delivery. OAuth delivery gives the adapter a daemon-minted access
token, the identity token issued with it, and account metadata in a scratch
credential home rather than through the child environment. Which values the
redactor is seeded with is
[configuration-and-credentials.md](../spec/configuration-and-credentials.md)'s
design; this document owns how the adapter installs and applies the scrub.
Before anything is written or the child starts, the adapter seeds exact-value
redaction with every such value, as the raw token and as the JSON string forms
whose escapes decode to it. Possible token prefixes are retained across stdout
and stderr chunks, and all child-controlled text passes through the scrub before
JSON decoding, truncation, debug rendering, observations or durable evidence.
Ambient-mode shape redaction remains defense in depth and cannot replace
exact-value redaction when preparation knows the token. Failure to install the
scrub is a typed pre-spawn delivery failure.

Refusal evidence over HTTP. Both HTTP decoders construct refusal evidence and
execute downgrades it to an unrecognized provider error. Surfacing it requires a
transport or evidence source that proves the response arrived only after the
complete request upload; the downgrade is removed only for an exchange that
carries that proof.

## Compatibility constraints

No present runtime operation carries the workspace-instruction field and no
present adapter mapping declares typed-system capacity. An adapter may not
concatenate such a region into ordinary system text, emit it as a user or tool
message, or enable a native project-file loader.

In ambient delivery the Codex adapter's child environment stays cleared, with
`OPENAI_API_KEY` and every other direct credential value excluded. The file
env_key grammar admits only `OPENAI_API_KEY`, and composition keeps rejecting
Codex file delivery as undelivered until the delivery exists.

The exact-value redactor and the CLI shape redactor stay separate layers, and
the CLI adapters keep a seam where exact values can be seeded before spawn.

The refusal downgrade in both HTTP adapters holds for every exchange without
complete-upload proof, and no caller treats an unrecognized provider error from
an HTTP adapter as a refusal.

## Acceptance criteria

An operation with a present region against a target or adapter mapping without
declared typed-system capacity fails preparation before any send. With capacity,
each adapter delivers the region only through its provider's instruction
transport, after the system prompt and before conversation messages, never as
system-text concatenation, a user or tool message, or a project-file loader.

A Codex file profile whose env_key is `OPENAI_API_KEY` resolves during
preparation; any other env_key is rejected as invalid configuration. The value
reaches the child's environment only, never argv, logs, evidence or a later
spawn, and provider-controlled output reflecting it is redacted by exact value.

Every token OAuth delivery hands the Codex adapter is redacted in raw and
JSON-escaped forms across chunk boundaries on stdout and stderr, before
decoding, truncation, observations and evidence. A failure to install the scrub
fails before spawn as a typed delivery failure.

Refusal evidence leaves an HTTP adapter only for an exchange carrying upload
proof; the Codex CLI path is unchanged.
