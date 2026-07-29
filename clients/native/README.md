# Signalbox Native

> Snapshot import (2026-07-23) from the owner's private monorepo, without
> history.

Native SwiftUI client for the Signalbox process protocol.

The production path encodes and decodes the single-version session and
transcript vocabulary as newline-delimited JSON. It does not use the earlier
REST, WebSocket, or OpenAI-compatible surfaces.

## Live macOS surface

- List native and imported conversations through the unified conversation read;
  open, archive, and unarchive native sessions.
- Follow a session through explicit connect, hello, history, replay, steady, and
  bounded-recovery states.
- Project transcript snapshots into the existing timeline normalizer.
- Layer ephemeral provider text above the durable transcript until a snapshot
  supersedes it.
- Preserve queued input separately until matching transcript content appears.
- Submit one exact, nonblank composer draft at a time with the session's
  defaults version and retry the exact prepared command after `commit_ambiguous`
  receipt loss, or a receive failure, with a finite schedule; if that schedule
  is exhausted, retain the prepared command identity while its exact UTF-8
  composer draft is unchanged and prepare a new identity after an edit.
- Treat unknown wire kinds conservatively without losing an entire page or
  stream.
- Approve or deny pending tool requests, and stop an active turn while sending
  its required successor input.
- Create a session by selecting a model alias read from the running daemon and
  optionally supplying a system prompt.
- Exercise the real encoder, decoder, request identity, and JSONL framing in
  deterministic mock UI flows.

The process protocol exposes no runner, template, monitor, or artifact catalog;
those views remain explicit capability gates rather than fabricated client
behavior. Imported conversations appear in the unified list, while transcript
inspection and continuation remain deferred to a separate native UI slice over
the landed imported-conversation read.

## Transport gate

`signalboxd` currently serves the protocol only on a local Unix socket, without
an authentication field. On macOS the app defaults to
`$DEVENV_RUNTIME/signalbox/signalboxd.sock` when that environment value is
present. Override it with an absolute socket path in Settings or launch with:

```bash
export SIGNALBOX_SOCKET_PATH='/absolute/path/to/signalbox.sock'
```

There is no owner-approved network transport reachable by a remote or mobile
client. iPhone and iPad builds run against the in-memory process-protocol
harness; real remote/mobile connectivity remains an owner design gate recorded
in
[Protocols and persistence](../../docs/open-questions.md#protocols-and-persistence);
the non-authoritative backlog tracks Tailscale as near-local direction and
iOS/iPad follow-on.

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

The operations and remote setup captures intentionally present capability gates.
They are not previews of unimplemented server behavior. Selective capture fails
before building when a requested screenshot name is not in the checked-in matrix
or the selection normalizes to no names, so a typo or blank selector cannot
silently validate an empty selection. The iPad capture uses UI automation only
to establish landscape orientation, then launches each state independently with
a bounded settle so one state cannot carry transient lifecycle diagnostics into
the next golden.

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

The live process path closes the imported transport and synchronization
findings: settings now install only the client for the tested socket path, and
stale connection or session-list probes cannot publish; every reconnect path is
capped; deadlines are typed separately from heartbeat concerns; snapshot/stream
ordering is owned by the synchronization machine; fallbacks preserve
diagnostics; failed submission preserves the exact composer text; one submission
is in flight at a time; an unresolved ambiguous submission, including a lost
receipt or receive failure, preserves its prepared command identity while the
exact UTF-8 draft is unchanged; process results remain neutral unless the wire
reports a failure; internal wire details do not become legacy `visible_to_user`
failures; and no credential crosses a plaintext URL.

The following work remains:

- Remote/mobile transport, authentication, authorization, and revocation await
  an owner-approved server design.
- Imported transcript inspection and continuation await their native UI slice
  over the imported-conversation read.
- Runners, templates, monitor summaries, and artifacts await real
  process-protocol operations.
- Compact-width navigation omits Templates until its information architecture is
  owner-approved; regular-width and macOS navigation retain the explicit
  capability gate.
- The older REST/WebSocket implementation remains compiled temporarily for
  import-era test and presentation compatibility, but production composition no
  longer installs it.
