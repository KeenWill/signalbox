# Signalbox Native

> Snapshot import (2026-07-23) from the maintainer's private monorepo, without
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
  defaults version.
  - Retry the exact prepared command with a finite schedule after
    `commit_ambiguous` receipt loss, or a receive failure.
  - When that schedule is exhausted, retain the prepared command identity while
    its exact UTF-8 composer draft is unchanged, and prepare a new identity
    after an edit.
- Treat unknown wire kinds conservatively without losing an entire page or
  stream.
- Approve or deny pending tool requests, and stop an active turn while sending
  its required successor input.
- Create a session by selecting a model alias read from the running daemon and
  optionally supplying a system prompt.
- Inspect the bounded, read-only entry inventory for an imported conversation
  and create a resume or fork session from a selected frontier and model alias.
- Exercise the real encoder, decoder, request identity, and JSONL framing in
  deterministic mock UI flows.

The process protocol exposes no runner, template, monitor, or artifact catalog;
those views remain explicit capability gates rather than fabricated client
behavior. Imported conversations open as read-only transcript inventories and
can continue into native sessions from an explicitly selected entry.

## Transport gate

`signalboxd` currently serves the protocol only on a local Unix socket, without
an authentication field. On macOS the app defaults to
`$DEVENV_RUNTIME/signalbox/signalboxd.sock` when that environment value is
present. Override it with an absolute socket path in Settings or launch with:

```bash
export SIGNALBOX_SOCKET_PATH='/absolute/path/to/signalbox.sock'
```

There is no maintainer-approved network transport reachable by a remote or
mobile client. iPhone and iPad **Debug** builds run against the in-memory
process-protocol harness. The harness is compiled out of Release builds, so a
Release iPhone or iPad build has no backend at all — that configuration is not
a supported way to run the app, and shipping one is gated on the same design
decision. Real remote/mobile connectivity remains a maintainer design gate
recorded in
[Protocols and persistence](../../docs/open-questions.md#protocols-and-persistence);
the non-authoritative backlog tracks Tailscale as near-local direction and
iOS/iPad follow-on.

## Build and test

```bash
scripts/build-xcode.sh
scripts/test-xcode.sh
scripts/test-real-server-xcode.sh
```

The scheme runs app, client, model, integration, and UI tests. The local mock is
selected with `--mock-server`, which only has an effect in Debug builds — the
scripts above all build Debug. In a Release build the flag is parsed and
ignored. The real-server script builds `signalboxd`, starts it with an isolated
temporary PostgreSQL database and Unix socket, and runs the macOS client
exchanges without making a model call.

`SIGNALBOX_NATIVE_SKIP_TESTING` and `SIGNALBOX_NATIVE_ONLY_TESTING` take
space-separated `xcodebuild` test identifiers and select which suites
`scripts/test-xcode.sh` runs.

## Snapshot tests

`Tests/SignalboxAppTests/LiveScreenSnapshotTests.swift` renders the screens
`RootView` reaches, and its `+LegacyScreens` companion renders the kept screens
that it no longer reaches. The 129 committed goldens under
`Tests/SignalboxAppTests/__Snapshots__` cover four fixed screen canvases —
iPhone and iPad, each in portrait and landscape — plus standalone sheet
content. Rendering is in process, with one screen hosted in one window at a
fixed canvas size, display scale, and safe area, so it sees no scene lifecycle
or window chrome. A sheet presented by the hosted screen does reach its golden;
sheet content is also snapshotted alone on its own canvas.
`ScreenshotScenario` selects the fixtures.

The canonical record and verification entry points are the two scripts below:
`scripts/record-snapshots.sh` and `scripts/test-snapshots.sh` take the suite and
the simulator from `scripts/lib/snapshots.sh`, which is what CI runs, while a
bare `scripts/test-xcode.sh` resolves whichever compatible phone is booted.
The phone-canvas goldens are byte-identical across the iPhone simulator models
on which they were checked. The two iPad canvases and the sheet canvas are wider
than the host phone's screen, so the window's corner mask and glass materials
composite against the destination. Those wide-canvas goldens can legitimately
fail on a destination other than CI's, and re-recording them there would commit
a rendering the pinned simulator then rejects.

Reduce Transparency is refused rather than pinned. Every other appearance input
these goldens depend on is a trait the canvas overrides, but that one is not a
trait at all — UIKit exposes it only as `UIAccessibility`, and SwiftUI derives
its environment value from that as read-only — so a run on a simulator with it
switched on stops and names the setting instead of comparing against references
recorded without it.

The suite runs as a report-only step in CI, which uploads the reference, the
failed rendering, and their difference as an artifact when a comparison fails.
Re-record the goldens after an intended visual change. Reviewing what you are
about to bless is
[rule 11](../../docs/agents/testing-style.md#expect-tests), which owns that
rule for every snapshot in the repository and is the only place it is stated.

```bash
scripts/record-snapshots.sh
SIGNALBOX_NATIVE_SNAPSHOT_RECORD=missing scripts/record-snapshots.sh
scripts/test-snapshots.sh
```

`SIGNALBOX_NATIVE_SNAPSHOT_RECORD` takes `all` (the recording script's default,
rewriting every golden), `missing`, `failed`, or `never`. Only that script
passes it to the suite; every other entry point always compares.

## Privacy boundary

The client contains no analytics, ads, tracking, telemetry, remote config,
accounts, or unrelated third-party SDKs. The real transport is a user-selected
local Unix socket. The process-protocol path accepts no credential and places
none in a URL or log.

## Rewire inventory

The live process path closes the imported transport and synchronization
findings:

- Settings now install only the client for the tested socket path.
- Stale connection or session-list probes cannot publish.
- Every reconnect path is capped.
- Deadlines are typed separately from heartbeat concerns.
- Snapshot/stream ordering is owned by the synchronization machine.
- Fallbacks preserve diagnostics.
- Failed submission preserves the exact composer text.
- One submission is in flight at a time.
- An unresolved ambiguous submission, including a lost receipt or receive
  failure, preserves its prepared command identity while the exact UTF-8 draft
  is unchanged.
- Process results remain neutral unless the wire reports a failure.
- Internal wire details do not become legacy `visible_to_user` failures.
- No credential crosses a plaintext URL.

The following work remains:

- Remote/mobile transport, authentication, authorization, and revocation await a
  maintainer-approved server design.
- Runners, templates, monitor summaries, and artifacts await real
  process-protocol operations.
- Compact-width navigation omits Templates until its information architecture is
  maintainer-approved; regular-width and macOS navigation retain the explicit
  capability gate.
- The app, client, and model test directories are separate targets in the shared
  scheme. Retired REST/WebSocket implementations now compile only as test-bundle
  compatibility support. Product sources retain a transport-free client protocol
  seam for legacy presentation fixtures; production composition installs only
  the process client.
