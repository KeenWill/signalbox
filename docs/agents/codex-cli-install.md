# Install the pinned Codex CLI

From the Signalbox checkout on a Linux x86_64 host:

```sh
bash tooling/codex-cli/install.sh "$HOME/.local/lib/signalbox-codex"
"$HOME/.local/lib/signalbox-codex/codex" --version
```

The installer downloads the `KeenWill/codex` release asset named in
`tooling/codex-cli/release.json`, verifies its SHA-256, and extracts the
multitool and companion binaries together. Set the dogfood Codex executable path
to `$HOME/.local/lib/signalbox-codex/codex` during deployment. The binary
reports the upstream version; the manifest tag also identifies the fork patch
revision.

Renovate groups the release tag and asset checksum into one Codex CLI update.
The compatibility smoke installs this same manifest before invoking the adapter.
