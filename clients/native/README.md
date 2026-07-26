# Signalbox Native

> Snapshot import (2026-07-23) from the owner's private monorepo, without
> history. This tree still speaks the project's earlier REST/WebSocket server
> protocol and awaits rewiring to the Signalbox process protocol.

Native SwiftUI client for that earlier server protocol.

The app uses the server's native REST and WebSocket APIs. It does not use the
OpenAI-compatible facade.

## Features

- Configure server URL and shared API key.
- Store the API key in Keychain.
- List, create, open, archive, and unarchive sessions.
- Subscribe to session streams and render structured events.
- Render tool invocations as expandable approval/status cards.
- Approve and deny confirmation-gated tool calls.
- Browse runners, templates, monitor summaries, and artifacts.
- Run deterministic mock UI flows with `--mock-server`.

## Build

```bash
scripts/build-xcode.sh
```

## Test

```bash
scripts/test-xcode.sh
```

The scheme runs the app, client, model, and integration unit suites under the
`SignalboxNativeTests` target.

## Launch In Simulator

```bash
scripts/run-simulator.sh
```

The simulator script launches the deterministic mock server flow. To smoke-test
a real server on a Mac, point the smoke test at your server endpoint and API key
(for example via 1Password):

```bash
export SIGNALBOX_NATIVE_REAL_SERVER_URL='http://127.0.0.1:8000'
export SIGNALBOX_NATIVE_REAL_SERVER_API_KEY="$(op read 'op://<vault>/<item>/<field>')"
export SIGNALBOX_NATIVE_REAL_SERVER_RUNNER_ID='<runner-id>'
scripts/test-real-server-xcode.sh
```

`SIGNALBOX_NATIVE_REAL_SERVER_RUNNER_ID` is optional. When it is omitted, the
smoke test accepts any registered runner but still requires at least one online
runner.

## Screenshots

Golden screenshots live under `Screenshots/iOS`, `Screenshots/iPadOS`, and
`Screenshots/macOS`. Regenerate and review them with:

```bash
scripts/capture-screenshots.sh
scripts/capture-macos-screenshots.sh
scripts/check-screenshot-goldens.sh
```

## Tart VM Validation

Apple validation can also run inside macOS Tart VM shards:

```bash
scripts/tart/run-shard.sh --print-plan xcode
scripts/tart/run-shard.sh xcode
scripts/tart/run-matrix.sh
```

See `docs/tart-vm-validation.md` for image setup, shard names, screenshot
parallelism, and real-server smoke configuration.

## Privacy Boundary

The client contains no analytics, ads, tracking, telemetry, remote config,
accounts, or unrelated third-party SDKs. The only network traffic is user
configured Signalbox REST/WebSocket traffic.

## Known issues (deferred to the protocol rewire)

Findings from the import review that live in code the protocol rewire replaces
(client, transport, and view-model layers). They are recorded here instead of
being fixed piecemeal in the snapshot; the rewire milestone takes them up, in
order. This deferral and its ordering are owned by the
[decision log](../../docs/decisions.md) entry "Defer native-snapshot review
findings to the rewire inventory"; the bullets below are descriptive inventory
under that decision, not normative claims.

- Saving settings persists the new server URL/API key without rebuilding the
  installed client, so traffic keeps flowing to the previous server until a
  successful Test Connection or relaunch.
- The session stream does not reconnect after a transient WebSocket drop; the
  session must be closed and reopened.
- A failed message submission clears the composer, so the draft is lost.
- `turn_failed` events render a failure card even when `visible_to_user` is
  false, exposing internal-only failure reasons in the timeline.
- The WebSocket stream carries the API key as a `token` URL query parameter (a
  design of the earlier protocol; the rewire's local-socket protocol eliminates
  it).
- Plain-HTTP server URLs are accepted for non-loopback hosts. App Transport
  Security blocks non-local plaintext requests to public hostnames, mitigating
  credential exposure there, but the local-network exception permits numeric IP
  addresses and those configurations can still send bearer credentials in
  cleartext. Input validation therefore permits both unsafe and nonconnecting
  configurations (same legacy transport; gone with the rewire).
- Templates are missing from compact-width iOS navigation.
- The Create button stays enabled while session creation is pending, so a double
  tap can create duplicate sessions.
- The operations refresh is all-or-nothing: a monitor-endpoint failure blanks
  the independently successful runner and template loads.
- Setup-screenshot capture clears the API key but not a previously saved server
  URL, so the setup golden can capture a private endpoint.
- `SignalboxNativeTests` writes to a persistent `UserDefaults` suite that is
  never cleaned between runs.
- `scripts/run-simulator.sh` discards the `simctl bootstatus` exit code and
  proceeds to install/launch even after a failed boot.
- When `listMonitorSessions()` fails while the session-list requests succeed,
  the sessions view falls back to labeling every session "Idle" (including
  running or approval-blocked ones) instead of an unknown/unavailable
  presentation.
- `scripts/capture-screenshots.sh` silently emits no match for unrecognized
  screenshot state names, so a typo skips captures while stale goldens keep
  manifest checks passing; requested names are not validated.
- The macOS screenshot exporter omits the `completed-tool` scenario that the
  iOS/iPadOS matrix captures, so there is no macOS golden coverage for the
  completed tool-card state.
