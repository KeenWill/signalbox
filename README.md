# Signalbox

Signalbox is a personal, self-hosted platform for durable LLM-assisted work —
your own always-on agent and chat hub rather than an account on someone else's
product. One central daemon owns your sessions and keeps them alive across
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

> **Status:** early implementation phase; APIs, protocols, and storage details
> are not yet stable. The initial domain and persistence slices now support a
> local daemon process protocol, terminal client, scheduler, and offline and
> Anthropic model-call paths. Remote runners and graphical clients remain future
> milestones.

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
- [Invariant catalog](docs/invariants.md)
- [Domain spine](docs/domain-spine.md)
- [Testing style](docs/agents/testing-style.md)
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

The workspace contains the dependency chain `apps/signalboxd` →
`crates/application` → `crates/domain`, with `crates/persistence` depending on
both `crates/application` and `crates/domain`, and the dev-only
`crates/expect-table` consumed by the domain crate's tests. Before finishing any
change, run the repository-wide validation sequence in [AGENTS.md](AGENTS.md) —
the canonical list of required commands and their setup notes — from the
repository root.

### Dev instance

`devenv up` starts a dev instance: a PostgreSQL cluster on loopback and one
`signalboxd` built from the working tree. The cluster asks for port 54341 and
devenv allocates upward from there if it is taken, so the port is resolved
rather than fixed — `echo $PGPORT` inside `devenv shell` names the one in use,
and the daemon is given the same resolved value. What `devenv up` launches, in
what order, and why test databases stay deliberately outside its scope are
recorded in the [decision log](docs/decisions.md); everything below is
operational usage.

State lives under the gitignored `.devenv/state/`: the cluster in `postgres/`,
and everything the daemon needs in `dev-instance/` — a locally generated
certificate authority and server certificate under `tls/`, a process-scoped home
under `home/`, and `signalboxd.toml`, seeded on first run from
[`config/signalboxd.example.toml`](config/signalboxd.example.toml) and left
alone afterwards so local edits survive. Wipe the whole instance with
`rm -rf .devenv/state`, or reseed just the catalog by deleting
`.devenv/state/dev-instance/signalboxd.toml`.

Two things are worth knowing before editing the seeded catalog or reaching for
the socket. The seed is a copy of the checked-in example, so it carries that
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
states and creates neither that directory nor those permissions itself; the
daemon process makes it before binding. Point the terminal client there with
`--socket`.

The daemon reads its Anthropic key from `~/.config/signalbox/anthropic-api-key`
and its code-host token from `~/.config/signalbox/github-token`, overridable
with `SIGNALBOX_DEV_ANTHROPIC_API_KEY_FILE` and
`SIGNALBOX_DEV_GITHUB_TOKEN_FILE` respectively. No credential material is
committed or generated. Both paths are passed to the daemon unconditionally
because it requires both variables at startup; neither file has to exist, since
neither is read at startup. When each file is read, and what its absence does,
are stated in the
[credential lifecycle](docs/spec/configuration-and-credentials.md#credential-lifecycle),
which also states that a trailing newline in either file is ignored — so
`gh auth token > ~/.config/signalbox/github-token` works as written.

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
incur cost. It runs only when all three opt-in values are supplied:

```console
SIGNALBOX_E2E_CONFIG_FILE=config/signalboxd.example.toml \
SIGNALBOX_E2E_ANTHROPIC_API_KEY_FILE=/path/to/anthropic-api-key \
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

The same harness can run the production runtime bridge against Anthropic. Copy
and review [`config/signalboxd.example.toml`](config/signalboxd.example.toml),
put only the API-key bytes in a mode-`0600` file, then run:

```console
SIGNALBOX_DEBUG_DATABASE_URL=postgres://signalbox:signalbox@localhost/signalbox \
SIGNALBOX_CONFIG_FILE=config/signalboxd.example.toml \
ANTHROPIC_API_KEY_FILE=/path/to/anthropic-api-key \
  cargo run -p signalboxd --bin signalbox-debug -- \
  --anthropic a5fec003-0edd-4118-96d1-18af31157bd3 \
  "Reply with exactly: signalbox smoke ok"
```

Production process configuration is specified in
[configuration and credentials](docs/spec/configuration-and-credentials.md#process-configuration).
The process boundary is specified in the
[process protocol](docs/spec/process-protocol.md); model configuration and
credential delivery are recorded in the [decision log](docs/decisions.md).

## License

Signalbox is licensed under the [MIT License](LICENSE).
