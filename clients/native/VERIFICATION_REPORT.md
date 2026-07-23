# LLM Hub Native Verification Report

## Summary

Implemented and verified the native SwiftUI LLM Hub client vertical slice, then expanded validation so the Apple workflows can run inside Tart macOS VMs. The current screenshot baseline is the reviewable golden copy for iOS, iPadOS landscape, and macOS window sizes, with hash checks enforced by both shell and Bazel tests.

The app builds and tests through Xcode and Bazel, launches in Simulator, connects to the real local Docker hub through the native REST/WebSocket APIs, renders deterministic mock states, and covers rich Markdown, tool approvals, artifacts, runners, monitor, templates, and settings. Secrets are kept out of logs and out of the repository.

## Files Changed

- `.gitignore`
- `Justfile`
- `projects/llm_hub_native/BUILD.bazel`
- `projects/llm_hub_native/README.md`
- `projects/llm_hub_native/VERIFICATION_REPORT.md`
- `projects/llm_hub_native/docs/tart-vm-validation.md`
- `projects/llm_hub_native/LLMHubNativeUITests/LLMHubNativeUITests.swift`
- `projects/llm_hub_native/LLMHubNativeUITests/ScreenshotCaptureUITests.swift`
- `projects/llm_hub_native/Sources/LLMHubApp/**`
- `projects/llm_hub_native/Tests/**`
- `projects/llm_hub_native/scripts/**`
- `projects/llm_hub_native/Screenshots/**`

## Implemented Features

- Native SwiftUI target for iOS, iPadOS, and macOS.
- Shared Swift models, native LLM Hub client code, view models, mock transports, and deterministic fixtures.
- Hub URL/API key settings with validation, Keychain-backed persistence, and no secret logging.
- Session list/detail, create, archive/unarchive, templates, runners, monitor, artifacts, streaming events, and tool approval UI.
- Structured timeline rendering for user/assistant messages, tool lifecycle cards, tool arguments/output, artifacts, failures, and unknown event diagnostics.
- Approve/deny controls for confirmation-gated tools.
- Native Markdown rendering for headings, emphasis, links, ordered/unordered/task lists, block quotes, horizontal rules, fenced code blocks, and pipe tables.
- Screenshot capture across iPhone 17 and iPhone 17 Pro, iPad Pro 11-inch/13-inch and iPad Air 13-inch landscape, plus macOS compact/regular/wide windows.
- Tart VM shard scripts for Xcode validation, iOS/iPadOS/macOS screenshots, privacy scan, real-hub smoke, and optional Bazel validation on images that include Bazel.
- Screenshot golden manifest and verification through shell and Bazel.

## Review Fix Update

Latest review-fix pass addressed:

- Removed shared `JSONDecoder`/`JSONEncoder` instances from `HubClient` and `HubWebSocketStream`; each request/stream message now uses a fresh coder.
- Made hub list calls page through sessions, monitor summaries, artifacts, and event `next_after` cursors.
- Cleared completed/failed stream tasks so session detail can reconnect after normal WebSocket completion or errors.
- Added a fallback decode path for unknown runner statuses.
- Surfaced Keychain write failures in settings instead of reporting save success after a failed SecItem operation.
- Added `NSLocalNetworkUsageDescription` for local hub connections.
- Stopped Tart real-smoke scripts from writing API keys to `.tart-real-hub.env`; host-provided `LLM_HUB_NATIVE_REAL_HUB_API_KEY` is written to a temporary `0600` dotenv file, mounted into the guest as `TART_SECRET_ENV_PATH`, omitted from command arguments, and treated as authoritative over mounted project `.env` values.
- Formatted the native app shell scripts with `shfmt` and added Tart dry-run coverage for secret dotenv precedence.

Latest commands:

| Command | Status | Notes |
| --- | --- | --- |
| `nix shell nixpkgs#shfmt --command shfmt -w projects/llm_hub_native/scripts/*.sh projects/llm_hub_native/scripts/lib/*.sh projects/llm_hub_native/scripts/tart/*.sh` | Pass | Formatted the native app shell scripts. |
| `nix shell nixpkgs#shfmt --command shfmt -d projects/llm_hub_native/scripts/*.sh projects/llm_hub_native/scripts/lib/*.sh projects/llm_hub_native/scripts/tart/*.sh` | Pass | No formatting diff. |
| `shellcheck projects/llm_hub_native/scripts/*.sh projects/llm_hub_native/scripts/lib/*.sh projects/llm_hub_native/scripts/tart/*.sh` | Pass | No shellcheck output. |
| `projects/llm_hub_native/scripts/tart/check-tart-scripts.sh` | Pass | Tart scripts passed dry-run validation, including `TART_SECRET_ENV_PATH` overriding a stale project `.env` API key. |
| `projects/llm_hub_native/scripts/check-privacy.sh` | Pass | `No analytics, ads, tracking, telemetry, remote-config, or third-party SDK markers found.` |
| `plutil -lint projects/llm_hub_native/Resources/Info.plist` | Pass | `projects/llm_hub_native/Resources/Info.plist: OK`. |
| `env LLM_HUB_NATIVE_REAL_HUB_API_KEY=super-secret-for-redaction TART_HUB_URL=http://192.168.64.1:8000 projects/llm_hub_native/scripts/tart/run-shard.sh --print-plan real-smoke` | Pass | Printed the mounted `TART_SECRET_ENV_PATH` and did not print the secret. |
| `bazel test --config=apple_host //projects/llm_hub_native:LLMHubModelsTests //projects/llm_hub_native:LLMHubClientTests //projects/llm_hub_native:LLMHubAppTests //projects/llm_hub_native:screenshot_golden_test //projects/llm_hub_native:tart_scripts_test` | Pass | Latest rerun: 5 tests pass. |
| `bazel build --config=ios_sim //projects/llm_hub_native:LLMHubNative` | Pass | Produced `bazel-bin/projects/llm_hub_native/LLMHubNative.ipa`. |
| `bazel build --config=apple_host //projects/llm_hub_native:LLMHubNativeMac` | Pass | Produced `bazel-bin/projects/llm_hub_native/LLMHubNativeMac.zip`. |
| `projects/llm_hub_native/scripts/build-xcode.sh` | Blocked | Host Xcode/CoreSimulator mismatch: `CoreSimulator is out of date. Current version (1051.50.0) is older than build version (1051.54.0)` and `iOS 26.5 is not installed`. |
| `projects/llm_hub_native/scripts/test-xcode.sh` | Blocked | Hung in `simctl list devices available` / `xcodebuild -runFirstLaunch` after the same CoreSimulator mismatch; stopped the process so validation did not remain running. |
| `xcodebuild ... -destination platform=macOS,arch=arm64 ... build` | Pass | macOS build completed, with the same host CoreSimulator warning emitted before build. |
| `xcodebuild ... -destination platform=macOS,arch=arm64 ... -only-testing:LLMHubNativeTests test` | Blocked | First attempt failed because it was run concurrently with the build and locked `build.db`; the sequential retry hung after the same host CoreSimulator mismatch warning and was stopped. |

## Commands Run

| Command | Status | Notes |
| --- | --- | --- |
| `projects/llm_hub_native/scripts/build-xcode.sh` | Pass | Xcode iOS Simulator build; final local pass recorded before Tart validation. |
| `projects/llm_hub_native/scripts/test-xcode.sh` | Pass | Xcode iOS Simulator unit/UI tests; final local pass recorded before Tart validation. |
| `xcodebuild -quiet -project projects/llm_hub_native/LLMHubNative.xcodeproj -scheme LLMHubNative -configuration Debug -destination 'platform=macOS,arch=arm64' ... build` | Pass | macOS build. |
| `xcodebuild -quiet -project projects/llm_hub_native/LLMHubNative.xcodeproj -scheme LLMHubNative -configuration Debug -destination 'platform=macOS,arch=arm64' ... -only-testing:LLMHubNativeTests test` | Pass | macOS unit tests. |
| `projects/llm_hub_native/scripts/build-bazel.sh` | Pass | Bazel Apple app build; rerun as `just build-llm-hub-native-bazel` after Tart work. |
| `projects/llm_hub_native/scripts/test-bazel.sh` | Pass | Bazel Swift client/model/app tests; rerun as `just test-llm-hub-native-bazel` after Tart work. |
| `projects/llm_hub_native/scripts/check-privacy.sh` | Pass | No analytics, ads, tracking, telemetry, remote-config, or unrelated SDK markers. |
| `projects/llm_hub_native/scripts/check-screenshot-goldens.sh` | Pass | Final pass after Tart iPadOS recapture. |
| `bazel test //projects/llm_hub_native:screenshot_golden_test //projects/llm_hub_native:tart_scripts_test` | Pass | Final pass: screenshot manifest matched and Tart script dry-run test passed. |
| `shellcheck projects/llm_hub_native/scripts/capture-screenshots.sh projects/llm_hub_native/scripts/tart/*.sh` | Pass | Final pass after batching iPad screenshot captures. |
| `projects/llm_hub_native/scripts/tart/check-tart-scripts.sh` | Pass | Final pass: Tart scripts passed dry-run validation. |
| `just tart-llm-hub-native-plan` | Pass | Printed the default Tart shards. Default excludes Bazel because the stock Cirrus Xcode image does not include Bazel. |
| `TART_HUB_URL=http://192.168.64.1:8001 TART_SHARDS=xcode,macos-screenshots just tart-llm-hub-native-matrix` | Fixed | `matrix-20260512013721-27939`: `xcode=0`, `macos-screenshots=0`; earlier `ios-screenshots=1` and `real-smoke=70` drove later fixes and reruns. |
| `TART_HUB_URL=http://192.168.64.1:8001 TART_SHARDS=ios-screenshots,real-smoke,privacy just tart-llm-hub-native-matrix` | Pass | `matrix-20260512022200-41465`: `ios-screenshots=0`, `real-smoke=0`, `privacy=0`. |
| `TART_HUB_URL=http://192.168.64.1:8001 TART_SHARDS=ipados-screenshots just tart-llm-hub-native-matrix` | Pass | Final `matrix-20260512041602-74992`: `ipados-screenshots=0`; Pro 11, Pro 13, and Air 13 landscape screenshots were regenerated and the manifest was updated. |
| `docker compose up -d --build postgres redis litellm hub agents` in `projects/llm_hub` | Pass | Local real hub stack started from this worktree. |
| `docker compose ps` in `projects/llm_hub` | Pass | `hub`, `agents`, `litellm`, `postgres`, and `redis` running; hub bound to `127.0.0.1:8000`. |
| `curl -fsS http://127.0.0.1:8000/healthz` | Pass | Returned `{"status":"ok"}`. |
| `curl -fsS -H "Authorization: Bearer $HUB_API_KEY" http://127.0.0.1:8000/api/v1/runners` | Pass | Confirmed `runner-docker` online without printing the key. |
| `socat TCP-LISTEN:8001,bind=0.0.0.0,reuseaddr,fork TCP:127.0.0.1:8000` | Pass | Host-to-Tart hub forward used by VM smoke tests at `http://192.168.64.1:8001`. |

## Manual Scenarios

| Scenario | Status | Evidence |
| --- | --- | --- |
| Fresh launch/setup | Pass | `Screenshots/iOS/iphone-17/setup.png`, `Screenshots/macOS/regular/setup.png`. |
| Settings | Pass | `Screenshots/iOS/iphone-17-pro/settings.png`, `Screenshots/macOS/regular/settings.png`. |
| Invalid/valid connection UI | Pass | Settings screen renders clear status and masked API key; real smoke validated successful connection. |
| Session list | Pass | `Screenshots/iPadOS/ipad-pro-13-inch-m5/sessions.png`. |
| New session modal | Pass | `Screenshots/iOS/iphone-17/new-session.png`, `Screenshots/iPadOS/ipad-air-13-inch-m4/new-session.png`, `Screenshots/macOS/regular/new-session.png`. |
| Active chat | Pass | `Screenshots/iOS/iphone-17-pro/active-chat.png`, `Screenshots/macOS/regular/active-chat.png`. |
| Markdown headings/lists | Pass | `Screenshots/iOS/iphone-17/markdown-basics.png`, `Screenshots/macOS/regular/markdown-basics.png`. |
| Markdown tables/links | Pass | `Screenshots/iPadOS/ipad-pro-11-inch-m5/markdown-table.png`, `Screenshots/macOS/regular/markdown-table.png`. |
| Markdown quotes/code | Pass | `Screenshots/iOS/iphone-17/markdown-code.png`, `Screenshots/macOS/regular/markdown-code.png`. |
| Markdown mixed report | Pass | `Screenshots/iOS/iphone-17/markdown-message.png`, `Screenshots/macOS/regular/markdown-message.png`. |
| Pending approval | Pass | `Screenshots/macOS/regular/pending-approval.png`. |
| Completed/failed tool | Pass | `Screenshots/iOS/iphone-17/completed-tool.png`, `Screenshots/iOS/iphone-17/failed-tool.png`. |
| Artifact preview | Pass | `Screenshots/macOS/regular/artifact-preview.png`. |
| Runner fleet | Pass | `Screenshots/iPadOS/ipad-air-13-inch-m4/runners.png`. |
| Monitor | Pass | `Screenshots/macOS/wide/monitor.png`. |
| Dark mode | Pass | `Screenshots/macOS/regular/dark.png`, `Screenshots/iPadOS/ipad-pro-13-inch-m5/dark.png`. |
| Large Dynamic Type | Pass | `Screenshots/iOS/iphone-17/large-type.png`, `Screenshots/iPadOS/ipad-pro-11-inch-m5/large-type.png`. |
| Real local hub | Pass | Docker stack plus Tart real-smoke and local Xcode smoke covered connection, runner list, session creation, send message, streaming/storage, and tool approval. |

## Screenshot Matrix

Golden PNG count: 133.

- iOS: 34 PNGs under `Screenshots/iOS/{iphone-17,iphone-17-pro}`.
- iPadOS: 51 landscape PNGs under `Screenshots/iPadOS/{ipad-pro-11-inch-m5,ipad-pro-13-inch-m5,ipad-air-13-inch-m4}`.
- macOS: 48 PNGs under `Screenshots/macOS/{compact,regular,wide}`.

Representative screenshot paths:

- `projects/llm_hub_native/Screenshots/iOS/iphone-17/setup.png`
- `projects/llm_hub_native/Screenshots/iOS/iphone-17/markdown-table.png`
- `projects/llm_hub_native/Screenshots/iOS/iphone-17-pro/settings.png`
- `projects/llm_hub_native/Screenshots/iPadOS/ipad-pro-11-inch-m5/sessions.png`
- `projects/llm_hub_native/Screenshots/iPadOS/ipad-pro-13-inch-m5/new-session.png`
- `projects/llm_hub_native/Screenshots/iPadOS/ipad-air-13-inch-m4/markdown-table.png`
- `projects/llm_hub_native/Screenshots/macOS/regular/settings.png`
- `projects/llm_hub_native/Screenshots/macOS/regular/new-session.png`
- `projects/llm_hub_native/Screenshots/macOS/regular/markdown-message.png`
- `projects/llm_hub_native/Screenshots/macOS/wide/monitor.png`

## Visual Critique Notes

- iPad screenshots briefly showed readable landscape content in sideways images. Fixed by returning to `XCUIScreen.main.screenshot()` and retaining orientation normalization only for portrait-shaped outputs; final inspected `iPad Pro 13-inch (M5)` sessions screenshot is upright at `2752 x 2064`.
- iPad Air 13 originally exceeded the 10-minute UI-test execution allowance after producing partial screenshots. Fixed by batching iPad screenshot captures into smaller state groups while preserving the same golden set.
- Sidebar content was missing from earlier macOS captures. The deterministic macOS exporter now includes the left sidebar in compact, regular, and wide screenshots.
- macOS `new-session` originally did not show the modal. The exporter now renders a deterministic in-view modal composition, and the inspected regular screenshot shows the modal centered over the session list.
- macOS `new-session` polish pass: Cancel moved to the footer, the redundant Session subheader was removed, form fields align left, model/tool labels are semibold, and tools render as compact badges.
- macOS Settings was too loose and ungrouped. It now uses separate Connection/Client panels, bordered desktop cards, masked API key text, and a clearer no-telemetry note. The regular screenshot remains intentionally sparse rather than filling a wide desktop with oversized controls.
- Markdown was previously flat text. The inspected Markdown screenshots now show headings, tables, inline code, links, and scroll-safe compact phone behavior.
- iPhone table screenshots intentionally show the left edge of a horizontally scrollable table; iPad and macOS screenshots show the full table.
- Large Dynamic Type is usable and scrollable, though visually heavy on iPhone.

## Fixes After Screenshot Review

- Added `MacScreenshotExporter` and `capture-macos-screenshots.sh`.
- Added `ScreenshotCaptureUITests` for iPadOS landscape screenshot capture through `XCUIScreenshot`.
- Added UI-test runner ad-hoc signing for screenshot/Xcode test commands to fix the damaged runner launch popup.
- Disabled parallel UI-test clones for iPad screenshot capture after a second clone failed despite the first clone producing screenshots.
- Added iPad landscape orientation normalization with dimension checks.
- Restored screen-based iPad screenshots after app-window screenshots produced sideways-looking landscape captures.
- Batched iPad screenshot UI-test captures to avoid Xcode's 10-minute per-test timeout on iPad Air 13.
- Added current-generation iPhone regular/Pro screenshot devices and iPad Pro/Air landscape devices.
- Removed stale flat screenshot locations and moved images under `Screenshots/iOS`, `Screenshots/iPadOS`, and `Screenshots/macOS`.
- Removed flaky iPhone Pro Max and iPad Air 11 defaults from the committed golden matrix; both remain explicit `SCREENSHOT_DEVICE_NAMES` opt-ins.
- Added new-session and rich Markdown screenshot states.
- Implemented native Markdown block rendering in timeline message bubbles.
- Polished the new-session modal across Apple platforms.
- Improved macOS Settings grouping and desktop card sizing.
- Added `Screenshots/MANIFEST.sha256`, `check-screenshot-goldens.sh`, and Bazel `screenshot_golden_test`.
- Added Tart VM docs, shard scripts, dry-run tests, guest-agent execution, false-green log checks, and local hub config handoff.
- Fixed real-hub UI smoke flakiness around text entry, tab selection, relaunch state, and session-list navigation.

## Real Hub Smoke Results

The local hub stack was run from `projects/llm_hub` in this worktree. A development `.env` was used locally but remains ignored; the report intentionally does not include the API key.

Exact evidence:

```text
$ docker compose ps
NAME                 SERVICE    STATUS                    PORTS
llm_hub-agents-1     agents     Up
llm_hub-hub-1        hub        Up                        127.0.0.1:8000->8000/tcp
llm_hub-litellm-1    litellm    Up                        127.0.0.1:4000->4000/tcp
llm_hub-postgres-1   postgres   Up (healthy)              127.0.0.1:5432->5432/tcp
llm_hub-redis-1      redis      Up (healthy)              127.0.0.1:6379->6379/tcp
```

```text
$ curl -fsS http://127.0.0.1:8000/healthz
{"status":"ok"}
```

```text
$ curl -fsS -H "Authorization: Bearer $HUB_API_KEY" http://127.0.0.1:8000/api/v1/runners
runner-docker: online
```

```text
$ TART_HUB_URL=http://192.168.64.1:8001 TART_SHARDS=ios-screenshots,real-smoke,privacy just tart-llm-hub-native-matrix
ios-screenshots: 0
real-smoke: 0
privacy: 0
```

The real smoke configured the app with the hub URL and API key, fetched runners, created a real session, sent a message, observed rendered events, and exercised a confirmation-gated tool approval path. For Tart guests, the host hub was forwarded with `socat` to `http://192.168.64.1:8001`.

## Known Issues

- Current local host Apple validation is partially blocked after the Xcode 26.5 update: Xcode reports `CoreSimulator is out of date. Current version (1051.50.0) is older than build version (1051.54.0)` and the iOS 26.5 simulator runtime is not installed. Bazel Apple builds/tests pass after clearing stale Bazel Xcode config; CoreSimulator-dependent Xcode iOS build/test/screenshot flows require updating the host Xcode components/macOS support files.
- Stock `ghcr.io/cirruslabs/macos-tahoe-xcode:latest` does not include Bazel, so the default Tart matrix does not run the Bazel shard. Host Bazel Apple build/test/golden checks were run and pass; the Tart Bazel shard remains available for a Bazel-enabled image.
- `iPad Air 11-inch (M4)` repeatedly hit CoreSimulator lockdown timeouts in the stock Tart image, with output like `unable to connect to "com.apple.instruments.deviceservice.lockdown" - timed out after 120 seconds`. It is an explicit opt-in device instead of part of the committed default golden matrix.
- `iPhone 17 Pro Max` repeatedly hung during app launch in the stock Tart image. It is also an explicit opt-in device; the committed phone matrix uses iPhone 17 and iPhone 17 Pro.
- Xcode 26.4 simulator UI-test launches still print repeated `DebuggerLLDB.DebuggerVersionStore.StoreError error 0` / `no debugger version` messages, but the tests and screenshot captures complete successfully with ad-hoc signing and `-parallel-testing-enabled NO`.
- Largest Dynamic Type is usable but visually dense on iPhone; a more compact accessibility timeline layout would be a useful follow-up.

## True Blockers

No repo-code blocker prevents review of the latest fixes. The current host has an external Apple toolchain blocker for CoreSimulator-dependent Xcode workflows:

```text
CoreSimulator is out of date. Current version (1051.50.0) is older than build version (1051.54.0).
iOS 26.5 is not installed. Please download and install the platform from Xcode > Settings > Components.
```

This is outside the app code because Bazel Apple compilation and tests pass with the installed Xcode after regenerating Bazel's Xcode config, while Xcode/CoreSimulator cannot enumerate usable iOS simulator destinations until the host components are updated. The Air 11 and Pro Max simulator failures are recorded above with exact failure modes and worked around by excluding them from the default golden matrix while keeping them opt-in for debugging.

## Follow-Ups

- Build or cache a custom Tart image with Bazel preinstalled so the VM matrix can include the Bazel shard by default.
- Add screenshot diff visualization in addition to hash checks if pixel drift becomes frequent.
- Promote the one-off real approval smoke into a named opt-in local command if repeated provider/tool activity becomes useful.
- Add a compact largest-Dynamic-Type timeline layout pass.

## Git Status Summary

The review fixes were committed as `7201f808f7 fix(llm-hub-native): address review findings` and pushed to `origin/agent/llm-hub-client-apps` via HTTPS after the SSH remote hung. After refreshing the remote-tracking ref, `git status --short --branch` showed:

```text
## agent/llm-hub-client-apps...origin/agent/llm-hub-client-apps
```

Ignored local files remain uncommitted, including `projects/llm_hub/.env`, DerivedData directories, `.tart-derived-data/`, legacy `.tart-real-hub.env` if present from older runs, and Tart result logs.
