# Signalbox

Signalbox is a personal, self-hosted platform for durable LLM-assisted work —
your own always-on agent and chat hub rather than an account on someone else's
product. One central daemon owns your sessions and keeps them alive across
restarts, disconnects, and device switches; terminal, web, macOS, and iOS
clients connect to it from anywhere, and runners you operate execute tools on
your own machines.

The [vision](docs/vision.md) and [target model](docs/target-model.md) describe
the purpose, deployment shape, and destination in full; the target model details
these capabilities directionally — accepted records decide them — and several
(fork selection, delegation, steering consumption) remain
[open decisions](docs/open-questions.md).

> **Status:** early implementation phase; APIs, protocols, and storage details
> are not yet stable.

```text
 Terminal       Web       macOS / iOS
    \            |            /
     +-----------+-----------+
                 |
          [ Central daemon ] ---- [ Postgres ]
            |         |
    provider adapters | scheduler / tool policy
                      |
              outbound connections
                /           \
       [ambient runner]  [restricted runner]
```

The daemon is the source of truth; a client device and an execution machine need
not be the same machine. See [Architecture](docs/architecture.md) for the
boundaries and important qualifications behind this sketch.

## Design documents

- [Vision](docs/vision.md)
- [Target model](docs/target-model.md)
- [Architecture](docs/architecture.md)
- [Glossary](docs/glossary.md)
- [Scenarios](docs/scenarios.md)
- [Invariant test index](docs/invariants.md)
- [Domain spine](docs/domain-spine.md)
- [Testing style](docs/agents/testing-style.md)
- [Open questions](docs/open-questions.md)
- [Living specification](docs/spec/README.md)

Project participation is described in [CONTRIBUTING.md](CONTRIBUTING.md),
security reporting in [SECURITY.md](SECURITY.md), and repository guidance for
coding agents in [AGENTS.md](AGENTS.md).

## Development

Install [rustup](https://rustup.rs/). The repository's `rust-toolchain.toml`
makes rustup select the pinned minimal stable toolchain with rustfmt and Clippy.

Non-cargo tooling comes from the [devenv](https://devenv.sh/) environment. With
Nix and the devenv CLI installed, use `devenv shell` to enter it; direnv users
can instead allow the committed `.envrc`. The Postgres integration suite still
needs a running Docker daemon. See [AGENTS.md](AGENTS.md) for the authoritative
tooling, formatting, and validation workflow.

The workspace contains the dependency chain `apps/signalboxd` →
`crates/application` → `crates/domain`, with `crates/persistence` depending on
both `crates/application` and `crates/domain`, and the dev-only
`crates/expect-table` consumed by the domain crate's tests. Before finishing any
change, run the repository-wide validation sequence in [AGENTS.md](AGENTS.md) —
the canonical list of required commands and their setup notes — from the
repository root.

### Dev instance

`devenv up` starts a dev instance: a PostgreSQL cluster on loopback, one
`signalboxd` built from the working tree, and on Linux one registration-only
`signalbox-runner`. The cluster asks for port 54341 and devenv allocates upward
from there if it is taken, so the port is resolved rather than fixed —
`echo $PGPORT` inside `devenv shell` names the one in use, and the daemon is
given the same resolved value. The devenv configuration owns what `devenv up`
launches, in what order, and why test databases stay deliberately outside its
scope; everything below is operational usage.

State lives under the gitignored `.devenv/state/`: the cluster in `postgres/`,
and everything the daemon needs in `dev-instance/` — a locally generated
certificate authority and server certificate under `tls/`, a process-scoped home
under `home/`, and `signalboxd.toml`, `session-templates.toml`, and
`signalbox-runner.toml`, seeded on first run from the corresponding files under
[`config/`](config/). The runner copy receives only deployment-specific paths
for its socket, state root, bubblewrap executable, and absent placeholder
credential file. All three copies are left alone afterwards so local edits
survive. Wipe the whole instance with `rm -rf .devenv/state`, or reseed one
configuration by deleting its file under `.devenv/state/dev-instance/`. The
daemon's runtime-only copy resolves the example workspace placeholder to a
persistent, initially unborn repository under `dev-instance/workspace/` and the
exec-supervisor placeholder to Cargo's built artifact; the user-editable seeded
file retains both installation examples. This separate repository is deliberate:
the local Git tools reject repository discovery and linked worktrees, while a
source checkout may use either.

Two things are worth knowing before editing the seeded model catalog or reaching
for the socket. Its seed is a copy of the checked-in example, so it carries that
file's undated family names such as `claude-haiku-4-5`; the spelling a
`provider_model` must take is stated in
[configuration and credentials](docs/spec/configuration-and-credentials.md#the-static-model-and-alias-catalog),
and how a reported identity is related back to it — including the dated snapshot
a family name resolves to — in
[provider-target identity](docs/spec/model-call-execution.md#provider-target-identity).
And the process socket lives at `$DEVENV_RUNTIME/signalbox/signalboxd.sock`
rather than directly in the runtime directory, because the daemon accepts only a
socket parent meeting the ownership and permission rules the
[process protocol](docs/spec/process-protocol.md#transport-and-trust-boundary)
states. The devenv dev-instance launcher creates that directory and sets mode
`0700` before executing the daemon; the daemon then binds the socket there. The
devenv shell exports that path as `SIGNALBOX_SOCKET_PATH` and provides a
`signalbox <verb>` convenience. The dedicated runner listener is the sibling
`$DEVENV_RUNTIME/signalbox/signalbox-runner.sock`; the seeded runner copy names
that path directly, while `SIGNALBOX_RUNNER_SOCKET_PATH` is set only on the
daemon that owns the listener. That convenience execs Cargo's resolved binary
directly rather than through `cargo run`, so a shell carrying an ambient
`-C prefer-dynamic` (in `RUSTFLAGS` or inherited Cargo configuration) produces
an executable that needs Cargo's runtime library search path, which the direct
`exec` does not set; the command then exits `127` naming the missing shared
object. This is a recorded, loud failure under an unusual global setting, not a
silent one, and is left as a known limitation rather than reproducing
`cargo run`'s environment here.

The daemon's default Anthropic key path is
`$HOME/.config/signalbox/anthropic-api-key`, written into the seeded model
catalog's Anthropic credential profile rather than passed in the environment,
and overridable with `SIGNALBOX_DEV_ANTHROPIC_API_KEY_FILE` at the moment that
copy is seeded; edit the seeded catalog to change it afterwards. The default
code-host token path is `$HOME/.config/signalbox/github-token`, overridable with
`SIGNALBOX_DEV_GITHUB_TOKEN_FILE`. The devenv Brave key path defaults to
`$DEVENV_STATE/dev-instance/brave-api-key` and is overridable with
`SIGNALBOX_DEV_BRAVE_API_KEY_FILE`. No credential material is committed or
generated. The
[credential lifecycle](docs/spec/configuration-and-credentials.md#credential-lifecycle)
owns when those files are read, what their bytes mean, and how absence is
handled. Provision the default code-host path from the GitHub CLI, and create
the Anthropic and Brave paths for editing, with these one-line commands:

```console
install -d -m 700 "$HOME/.config/signalbox" && (umask 077; destination="$HOME/.config/signalbox/github-token"; temporary="$(mktemp "$destination.XXXXXX")" || exit; trap 'rm -f "$temporary"' EXIT; gh auth token >"$temporary" && mv "$temporary" "$destination" && trap - EXIT)
install -d -m 700 "$HOME/.config/signalbox" && (umask 077; destination="$HOME/.config/signalbox/anthropic-api-key"; temporary="$(mktemp "$destination.XXXXXX")" || exit; trap 'rm -f "$temporary"' EXIT; if [ -e "$destination" ]; then cp "$destination" "$temporary" || exit; fi; editor="${EDITOR:-vi}"; EDITOR="$editor" sh -c 'set -f; $EDITOR "$1"' sh "$temporary" && mv "$temporary" "$destination" && trap - EXIT)
install -d -m 700 "$DEVENV_STATE/dev-instance" && (umask 077; destination="$DEVENV_STATE/dev-instance/brave-api-key"; temporary="$(mktemp "$destination.XXXXXX")" || exit; trap 'rm -f "$temporary"' EXIT; if [ -e "$destination" ]; then cp "$destination" "$temporary" || exit; fi; editor="${EDITOR:-vi}"; EDITOR="$editor" sh -c 'set -f; $EDITOR "$1"' sh "$temporary" && mv "$temporary" "$destination" && trap - EXIT)
```

Most of `devenv.nix` exists to satisfy the ambient-configuration refusals that
[configuration and credentials](docs/spec/configuration-and-credentials.md#process-configuration)
specifies — the `PG*` and `SSL_CERT_*` scrub, the process-scoped home the
passfile check reads, the generated authority that lets a loopback cluster pass
full verification, and a fully stated `DATABASE_URL` exported to the daemon
process alone — so that experiments stop re-deriving them. Each is commented in
`devenv.nix` at the point it is handled.

### Terminal client

The `signalbox` binary is the supported local terminal surface for the
[process protocol](docs/spec/process-protocol.md). Point it at the daemon socket
with `--socket` or `SIGNALBOX_SOCKET_PATH`; `signalbox --help` lists the closed
command surface. For example:

```console
cargo run -p signalbox-client -- --socket /path/to/signalbox.sock list
printf '%s' 'hello' |
  cargo run -p signalbox-client -- --socket /path/to/signalbox.sock \
    send 00000000-0000-4000-8000-000000000001
```

The Docker-backed offline terminal-to-model smoke test is explicitly ignored:

```console
cargo test -p signalbox-client --test end_to_end \
  terminal_client_completes_an_offline_scripted_conversation \
  -- --ignored --nocapture
```

The companion ignored real-Anthropic path makes a live provider request and may
incur cost. The checked-in catalog names the production installation placeholder
for the exec supervisor, so first materialize a runtime copy with the host
executable that Cargo actually builds (inside `devenv shell`):

```console
supervisor="$(tooling/resolve-cargo-bin.sh "$PWD/Cargo.toml" "$PWD/target" \
  signalbox-tools-exec signalbox-exec-supervisor)"
signalbox-materialize-config config/signalboxd.example.toml \
  target/signalboxd.live.toml "$supervisor"
```

Review that runtime copy and point its Anthropic credential profile's `file` at
a mode-`0600` file containing only the API-key bytes. The live path then runs
only when both opt-in values are supplied:

```console
SIGNALBOX_E2E_CONFIG_FILE=target/signalboxd.live.toml \
SIGNALBOX_E2E_SELECTION_ID=a5fec003-0edd-4118-96d1-18af31157bd3 \
  cargo test -p signalbox-client --test end_to_end \
    terminal_client_completes_the_real_anthropic_path \
    -- --ignored --nocapture
```

### Scripted debug harness

The `signalbox-debug` binary is a local development harness, not the supported
terminal client defined by the
[process protocol](docs/spec/process-protocol.md). Against a disposable local
PostgreSQL database it runs migrations, creates one session, submits one input,
lets the real scheduler execute a deterministic reply, and prints the terminal
semantic transcript:

```console
SIGNALBOX_DEBUG_DATABASE_URL=postgres://signalbox:signalbox@localhost/signalbox \
  cargo run -p signalboxd --bin signalbox-debug -- \
  "hello" "scripted assistant reply"
```

The debug database connection explicitly disables TLS and must not be used as
production connection configuration.

The same harness can run the production runtime bridge against Anthropic. Review
the checked-in example, materialize `target/signalboxd.live.toml` with the setup
command above, and point its Anthropic credential profile at the mode-`0600` key
file before running:

```console
SIGNALBOX_DEBUG_DATABASE_URL=postgres://signalbox:signalbox@localhost/signalbox \
SIGNALBOX_CONFIG_FILE=target/signalboxd.live.toml \
  cargo run -p signalboxd --bin signalbox-debug -- \
  --anthropic a5fec003-0edd-4118-96d1-18af31157bd3 \
  "Reply with exactly: signalbox smoke ok"
```

Production process configuration is specified in
[configuration and credentials](docs/spec/configuration-and-credentials.md#process-configuration).
The process boundary is specified in the
[process protocol](docs/spec/process-protocol.md); model configuration and
credential delivery are specified in
[configuration and credentials](docs/spec/configuration-and-credentials.md).

## License

Signalbox is licensed under the [MIT License](LICENSE).
