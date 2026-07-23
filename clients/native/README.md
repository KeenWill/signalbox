# LLM Hub Native

> Snapshot import (2026-07-23) from the owner's private monorepo, without
> history. This tree targets the legacy llm-hub protocol and awaits rewiring to
> the Signalbox process protocol.

Native SwiftUI client for the legacy llm-hub server.

The app uses the hub's native REST and WebSocket APIs. It does not use the
OpenAI-compatible facade.

## Features

- Configure hub URL and shared API key.
- Store the API key in Keychain.
- List, create, open, archive, and unarchive sessions.
- Subscribe to session streams and render structured events.
- Render tool invocations as expandable approval/status cards.
- Approve and deny confirmation-gated tool calls.
- Browse runners, templates, monitor summaries, and artifacts.
- Run deterministic mock UI flows with `--mock-hub`.

## Build

```bash
scripts/build-xcode.sh
```

## Test

```bash
scripts/test-xcode.sh
```

## Launch In Simulator

```bash
scripts/run-simulator.sh
```

The simulator script launches the deterministic mock hub flow. To smoke-test a
real hub on a Mac, point the smoke test at your hub endpoint and API key (for
example via 1Password):

```bash
export LLM_HUB_NATIVE_REAL_HUB_URL='http://127.0.0.1:8000'
export LLM_HUB_NATIVE_REAL_HUB_API_KEY="$(op read 'op://<vault>/<item>/<field>')"
export LLM_HUB_NATIVE_REAL_HUB_RUNNER_ID='<runner-id>'
scripts/test-real-hub-xcode.sh
```

`LLM_HUB_NATIVE_REAL_HUB_RUNNER_ID` is optional. When it is omitted, the smoke
test accepts any registered runner but still requires at least one online
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
just tart-llm-hub-native-plan
just tart-llm-hub-native-shard xcode
just tart-llm-hub-native-matrix
```

See `docs/tart-vm-validation.md` for image setup, shard names, screenshot
parallelism, and real-hub smoke configuration.

## Privacy Boundary

The client contains no analytics, ads, tracking, telemetry, remote config,
accounts, or unrelated third-party SDKs. The only network traffic is user
configured LLM Hub REST/WebSocket traffic.

## Known issues (deferred to the protocol rewire)

Findings from the import review that live in code the protocol rewire replaces
(client, transport, and view-model layers). They are recorded here instead of
being fixed piecemeal in the snapshot; the rewire milestone takes them up, in
order.

- The Xcode project wires no test targets for `Tests/LLMHubAppTests`,
  `Tests/LLMHubClientTests`, or `Tests/LLMHubModelsTests`, so those 44 tests
  are unreachable since the Bazel exclusion — restoring them is the rewire's
  first task.
- Saving settings persists the new hub URL/API key without rebuilding the
  installed client, so traffic keeps flowing to the previous hub until a
  successful Test Connection or relaunch.
- The session stream does not reconnect after a transient WebSocket drop; the
  session must be closed and reopened.
- A failed message submission clears the composer, so the draft is lost.
- `EventNormalizer.toolCard` takes the first function-call/response parts
  instead of matching `toolCallID`, showing the wrong arguments/output for
  multi-tool-call messages.
- `turn_failed` events render a failure card even when `visible_to_user` is
  false, exposing internal-only failure reasons in the timeline.
- The WebSocket stream carries the API key as a `token` URL query parameter
  (legacy llm-hub protocol design; the rewire's local-socket protocol
  eliminates it).
- Plain-HTTP hub URLs are accepted for non-loopback hosts, sending the bearer
  key in cleartext (same legacy transport; gone with the rewire).
- Templates are missing from compact-width iOS navigation.
- The Create button stays enabled while session creation is pending, so a
  double tap can create duplicate sessions.
- The operations refresh is all-or-nothing: a monitor-endpoint failure blanks
  the independently successful runner and template loads.
- Setup-screenshot capture clears the API key but not a previously saved hub
  URL, so the setup golden can capture a private endpoint.
- `LLMHubNativeTests` writes to a persistent `UserDefaults` suite that is
  never cleaned between runs.
- `scripts/run-simulator.sh` discards the `simctl bootstatus` exit code and
  proceeds to install/launch even after a failed boot.
