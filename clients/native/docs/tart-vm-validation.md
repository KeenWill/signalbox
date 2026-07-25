# Tart VM Validation

This project can run its Apple validation and screenshot flows inside macOS Tart
VMs. The Tart scripts wrap the existing project commands rather than replacing
them, so local non-VM validation remains the source of truth for the actual
build, test, and screenshot behavior.

## Requirements

- Apple Silicon host running macOS 13 or newer.

- Tart installed on the host:

  ```bash
  brew install cirruslabs/cli/tart
  ```

- `sshpass` only when forcing `TART_EXECUTOR=ssh` with the default
  `admin`/`admin` Tart image credentials:

  ```bash
  brew install cirruslabs/cli/sshpass
  ```

- A macOS Xcode Tart image with the same Xcode and simulator runtime family used
  for the committed screenshot goldens. The default image is:

  ```text
  ghcr.io/cirruslabs/macos-tahoe-xcode:latest
  ```

Tart's official docs describe the image names, guest-agent execution through
`tart exec`, SSH access through `tart ip`, and shared directory mounts through
`tart run --dir`. The scripts use `tart exec` by default because it avoids
password prompts in headless validation runs.

## Shards

All commands below run from the repository root.

Run a dry-run plan without starting a VM:

```bash
clients/native/scripts/tart/run-shard.sh --print-plan xcode
```

Run one shard:

```bash
clients/native/scripts/tart/run-shard.sh xcode
clients/native/scripts/tart/run-shard.sh ios-screenshots
clients/native/scripts/tart/run-shard.sh ipados-screenshots
clients/native/scripts/tart/run-shard.sh macos-screenshots
clients/native/scripts/tart/run-shard.sh bazel
clients/native/scripts/tart/run-shard.sh real-smoke
```

Run the default matrix:

```bash
clients/native/scripts/tart/run-matrix.sh
```

The default matrix runs:

- `xcode`
- `macos-screenshots`
- `ios-screenshots`
- `ipados-screenshots`
- `privacy`

Add the real server smoke shard with:

```bash
TART_INCLUDE_REAL_SMOKE=1 clients/native/scripts/tart/run-matrix.sh
```

The `bazel` shard is inert in this repository: the snapshot import deliberately
left the Bazel build files behind, so the shard's `scripts/build-bazel.sh` and
`//clients/native:screenshot_golden_test` targets do not exist here. It is kept
only for hosts that carry their own Bazel-enabled Tart image and build files.

## Parallelism

The matrix runner defaults to two concurrent Tart VMs:

```bash
TART_PARALLELISM=2 clients/native/scripts/tart/run-matrix.sh
```

That matches the practical Apple licensing and host resource constraints for a
single Apple Silicon machine. Use a higher value only on infrastructure where
that is explicitly licensed and provisioned.

For larger farms, run the same shard commands on multiple Apple Silicon hosts or
put these scripts behind Orchard/Cirrus-style orchestration. The shard contract
is deliberately plain shell so a remote runner only needs the repo, Tart, Xcode,
and a reachable server URL.

## Screenshots

The screenshot shards write directly into the mounted worktree:

- `Screenshots/iOS`
- `Screenshots/iPadOS`
- `Screenshots/macOS`

The matrix updates and checks `Screenshots/MANIFEST.sha256` after screenshot
shards by default. Disable that if the job should only capture raw artifacts:

```bash
TART_UPDATE_SCREENSHOT_MANIFEST=0 clients/native/scripts/tart/run-matrix.sh
```

Limit screenshot states or devices with the same environment variables used by
the non-VM scripts:

```bash
SCREENSHOT_STATE_NAMES=new-session \
SCREENSHOT_DEVICE_NAMES='iPad Pro 13-inch (M5)' \
clients/native/scripts/tart/run-shard.sh ipados-screenshots
```

The default iPhone shard captures the current-generation regular and Pro phone
sizes (`iPhone 17`, `iPhone 17 Pro`). Add larger phone classes explicitly with
`SCREENSHOT_DEVICE_NAMES` when needed.

The default iPadOS shard captures `iPad Pro 11-inch (M5)`,
`iPad Pro 13-inch (M5)`, and `iPad Air 13-inch (M4)`. The Air 11-inch simulator
is intentionally left as an explicit `SCREENSHOT_DEVICE_NAMES` opt-in because it
has shown repeated CoreSimulator lockdown timeouts in the stock Tahoe/Xcode Tart
image.

## Real Server Smoke

The `real-smoke` shard uses the native app UI test
`testRealServerConnectionListsRunnerAndCreatesSessionWhenConfigured`.

The VM can read `clients/native/.env` through the shared worktree mount for
`SIGNALBOX_API_KEY`, but `http://127.0.0.1:8000` inside the VM points at the
guest, not the host. Prefer an explicit URL reachable from the guest:

```bash
TART_SERVER_URL='http://192.168.64.1:8000' \
clients/native/scripts/tart/run-shard.sh real-smoke
```

If `TART_SERVER_URL` is not set, the guest tries to infer the Tart NAT router IP
and uses `http://<router>:8000`. For that to work, the server must listen on an
address reachable from the VM, not only `127.0.0.1`.

Do not pass API keys on a command line. The scripts either read the ignored
`clients/native/.env` file from the mounted worktree or mount
`SIGNALBOX_NATIVE_REAL_SERVER_API_KEY` from the host environment through a
temporary 0600 env file. The key value is never embedded in the Tart/SSH command
line and is never printed by the scripts.

## Custom Images And Existing VMs

Use a different image:

```bash
TART_BASE_IMAGE='ghcr.io/cirruslabs/macos-sequoia-xcode:latest' \
clients/native/scripts/tart/run-shard.sh xcode
```

Reuse an existing local VM:

```bash
clients/native/scripts/tart/run-shard.sh \
  --vm signalbox-native-dev \
  --reuse-vm \
  --keep-vm \
  xcode
```

Tune the ephemeral VM resources:

```bash
TART_VM_CPUS=6 \
TART_VM_MEMORY_MB=12288 \
TART_VM_DISPLAY=2560x1600 \
clients/native/scripts/tart/run-shard.sh ipados-screenshots
```

Force SSH execution instead of the Tart guest agent:

```bash
TART_EXECUTOR=ssh clients/native/scripts/tart/run-shard.sh xcode
```

## Logs

Matrix logs are written under:

```text
clients/native/.tart-results/
```

That directory is ignored by git. A failing shard prints the exact per-shard log
path.

## Known Constraints

- Xcode, simulator runtimes, and screenshot goldens must be pinned by image
  choice. Changing the Tart image can legitimately change screenshots.
- On macOS 15 and newer headless hosts, Tart may require an unlocked
  `login.keychain` before VMs can start.
- The real server stack is best run on the host, a Linux VM, or another
  reachable machine. Running Docker inside the macOS screenshot VM adds nested
  virtualization complexity and is not part of this first automation path.
- iPhone and iPadOS screenshot capture are separate shards because CoreSimulator
  is more predictable when each VM owns a smaller device matrix.
