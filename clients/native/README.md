# Signalbox Native

> Snapshot import (2026-07-23) from the owner's private monorepo, without
> history.

Native SwiftUI client for the Signalbox process protocol.

The production path encodes and decodes the version 5 session and transcript
vocabulary as newline-delimited JSON. It does not use the earlier REST,
WebSocket, or OpenAI-compatible surfaces.

## Phase A surface

- List, search, open, archive, and unarchive sessions from metadata operations.
- Follow a session through explicit connect, hello, history, replay, steady, and
  bounded-recovery states.
- Project transcript snapshots into the existing timeline normalizer.
- Preserve queued input separately until matching transcript content appears.
- Submit one exact, nonblank composer draft at a time with the session's
  defaults version and retry the exact prepared command after `commit_ambiguous`
  receipt loss, or a receive failure, with a finite schedule; if that schedule
  is exhausted, retain the prepared command identity while its exact UTF-8
  composer draft is unchanged and prepare a new identity after an edit.
- Treat unknown wire kinds conservatively without losing an entire page or
  stream.
- Exercise the real v5 encoder, decoder, request identity, and JSONL framing in
  deterministic mock UI flows.

The process protocol exposes no tool-decision operation or successful-result
discriminator. Tool cards therefore show observed results as neutral completed
state and never offer approve or deny controls. It also exposes no runner,
template, monitor, artifact, or model-discovery catalog; those views and
new-session creation are explicit capability gates rather than fabricated client
behavior.

## Transport gate

`signalboxd` currently serves the protocol only on a local Unix socket, without
an authentication field. On macOS, set an absolute socket path in Settings or
launch with:

```bash
export SIGNALBOX_SOCKET_PATH='/absolute/path/to/signalbox.sock'
```

There is no owner-approved network transport reachable by a remote or mobile
client. Phase A does not invent one. iPhone and iPad builds run against the
in-memory v5 harness; real remote/mobile connectivity remains an owner design
gate tracked by
[Authenticated transports and remote clients](../../docs/open-questions.md#protocols-and-persistence).

## Build and test

```bash
scripts/build-xcode.sh
scripts/test-xcode.sh
```

The scheme runs app, client, model, integration, and UI tests. The local mock is
selected with `--mock-server`.

## Screenshots

Golden screenshots live under `Screenshots/iOS`, `Screenshots/iPadOS`, and
`Screenshots/macOS`. The 136-image matrix includes pending, completed, and
failed tool states. Regenerate and verify it with:

```bash
scripts/capture-screenshots.sh
scripts/capture-macos-screenshots.sh
scripts/check-screenshot-goldens.sh
```

The `new-session`, operations, and remote setup captures intentionally present
capability gates. They are not previews of unimplemented server behavior.

## Tart VM validation

Apple validation can also run inside macOS Tart VM shards:

```bash
scripts/tart/run-shard.sh --print-plan xcode
scripts/tart/run-shard.sh xcode
scripts/tart/run-matrix.sh
```

See [Tart VM validation](docs/tart-vm-validation.md).

## Privacy boundary

The client contains no analytics, ads, tracking, telemetry, remote config,
accounts, or unrelated third-party SDKs. The real transport is a user-selected
local Unix socket. The process-protocol path accepts no credential and places
none in a URL or log.

## Rewire inventory

Phase A closes the imported transport and synchronization findings: settings now
install only the client for the tested socket path, and stale connection or
session-list probes cannot publish; every reconnect path is capped; deadlines
are typed separately from heartbeat concerns; snapshot/stream ordering is owned
by the synchronization machine; fallbacks preserve diagnostics; failed
submission preserves the exact composer text; one submission is in flight at a
time; an unresolved ambiguous submission, including a lost receipt or receive
failure, preserves its prepared command identity while the exact UTF-8 draft is
unchanged; process results remain neutral unless the wire reports a failure;
internal wire details do not become legacy `visible_to_user` failures; and no
credential crosses a plaintext URL.

The following work remains:

- Remote/mobile transport, authentication, authorization, and revocation await
  an owner-approved server design.
- Session creation awaits model discovery or another owner-approved way to
  select the protocol's required model UUID.
- Tool decisions, runners, templates, monitor summaries, and artifacts await
  real process-protocol operations.
- The older REST/WebSocket implementation remains compiled temporarily for
  import-era test and presentation compatibility, but production composition no
  longer installs it.
