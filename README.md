# Signalbox

Signalbox is a personal, self-hosted platform for durable LLM-assisted work —
your own always-on agent and chat hub rather than an account on someone else's
product. One central hub owns your sessions and keeps them alive across
restarts, disconnects, and device switches; terminal, web, macOS, and iOS
clients connect to it from anywhere, and runners you operate execute tools on
your own machines.

What it is being built to do:

- **Sessions you can shape.** Steer a running turn mid-flight, fork a
  conversation from any earlier point, and delegate work into sub-sessions.
- **Tools where the work lives.** Outbound-connected runners execute tools on
  your workstations, servers, and sandboxes — the machine that holds the files,
  not necessarily the one you are typing on.
- **Approvals you can inspect.** Risky tool use waits for an explicit decision
  bound to exactly the action requested.
- **Honest reliability.** Reconnecting never presents a draft as final;
  interrupted work is recorded as what actually happened, ambiguity included,
  with provenance for who or what caused each change.

The [vision](docs/vision.md) and [target model](docs/target-model.md) describe
the purpose, deployment shape, and destination in full; the target model details
these capabilities directionally — accepted records decide them — and several
(fork selection, delegation, steering consumption) remain
[open decisions](docs/open-questions.md).

> **Status:** design and foundation phase, not yet a usable product. The initial
> domain and persistence slices exist behind accepted decisions — session
> creation and loading, defaults replacement, durable input acceptance, and
> eligible-turn activation — plus the first offline and Anthropic model-call
> paths; runners and clients are milestones ahead, and APIs, protocols, and
> storage details are not yet stable.

```text
 Terminal       Web       macOS / iOS
    \            |            /
     +-----------+-----------+
                 |
          [ Central hub ] ---- [ Postgres ]
            |         |
    provider adapters | scheduler / tool policy
                      |
              outbound connections
                /           \
       [ambient runner]  [restricted runner]
```

The hub is the source of truth; a client device and an execution machine need
not be the same machine. See [Architecture](docs/architecture.md) for the
boundaries and important qualifications behind this sketch.

## Design documents

- [Vision](docs/vision.md)
- [Target model](docs/target-model.md)
- [Architecture](docs/architecture.md)
- [Glossary](docs/glossary.md)
- [Scenarios](docs/scenarios.md)
- [Invariant catalog](docs/invariants.md)
- [Domain spine](docs/domain-spine.md)
- [Testing style](docs/testing-style.md)
- [Decision log](docs/decisions.md)
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

The workspace contains the dependency chain `apps/hubd` → `crates/application` →
`crates/domain`, with `crates/persistence` depending on both
`crates/application` and `crates/domain`, and the dev-only `crates/expect-table`
consumed by the domain crate's tests. Before finishing any change, run the
repository-wide validation sequence in [AGENTS.md](AGENTS.md) — the canonical
list of required commands and their setup notes — from the repository root.

### Terminal client

The `signalbox` binary is the supported local terminal surface for the
[process protocol](docs/spec/process-protocol.md). Point it at the hub socket
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
incur cost. It runs only when all three opt-in values are supplied:

```console
SIGNALBOX_E2E_CONFIG_FILE=config/hubd.example.toml \
SIGNALBOX_E2E_ANTHROPIC_API_KEY_FILE=/path/to/anthropic-api-key \
SIGNALBOX_E2E_SELECTION_ID=10000000-0000-4000-8000-000000000001 \
  cargo test -p signalbox-client --test end_to_end \
    terminal_client_completes_the_real_anthropic_path \
    -- --ignored --nocapture
```

### Scripted debug harness

The `signalbox-debug` binary is a local development harness, not the supported
process client. Against a disposable local PostgreSQL database it runs
migrations, creates one session, submits one input, lets the real scheduler
execute a deterministic reply, and prints the terminal semantic transcript:

```console
SIGNALBOX_DEBUG_DATABASE_URL=postgres://signalbox:signalbox@localhost/signalbox \
  cargo run -p signalbox-hubd --bin signalbox-debug -- \
  "hello" "scripted assistant reply"
```

The debug database connection explicitly disables TLS and must not be used as
production connection configuration.

The same harness can run the production runtime bridge against Anthropic. Copy
and review [`config/hubd.example.toml`](config/hubd.example.toml), put only the
API-key bytes in a mode-`0600` file, then run:

```console
SIGNALBOX_DEBUG_DATABASE_URL=postgres://signalbox:signalbox@localhost/signalbox \
SIGNALBOX_CONFIG_FILE=config/hubd.example.toml \
ANTHROPIC_API_KEY_FILE=/path/to/anthropic-api-key \
  cargo run -p signalbox-hubd --bin signalbox-debug -- \
  --anthropic 10000000-0000-4000-8000-000000000001 \
  "Reply with exactly: signalbox smoke ok"
```

Production process configuration is specified in
[configuration and credentials](docs/spec/configuration-and-credentials.md#process-configuration).
The process boundary is specified in the
[process protocol](docs/spec/process-protocol.md); model configuration and
credential delivery are recorded in the [decision log](docs/decisions.md).

## License

Signalbox is licensed under the [MIT License](LICENSE).
