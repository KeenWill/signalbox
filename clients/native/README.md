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
