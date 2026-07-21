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
> eligible-turn activation — with model calls next; provider adapters, runners,
> and the clients are milestones ahead, and APIs, protocols, and storage details
> are not yet stable.

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
- [Architecture decision records](docs/decisions/README.md)

Project participation is described in [CONTRIBUTING.md](CONTRIBUTING.md),
security reporting in [SECURITY.md](SECURITY.md), and repository guidance for
coding agents in [AGENTS.md](AGENTS.md).

## Development

Install [rustup](https://rustup.rs/). The repository's `rust-toolchain.toml`
makes rustup select the pinned minimal stable toolchain with rustfmt and Clippy.

Non-cargo tooling (currently the pinned mdformat toolchain from
`tooling/requirements-mdformat.txt`) comes from the [devenv](https://devenv.sh/)
environment defined in `devenv.nix`, so local tool versions match CI exactly.
With Nix installed, `nix profile install nixpkgs#devenv` adds the devenv CLI;
`devenv shell` then enters the environment, and
`devenv shell -- mdformat --check *.md docs/` runs a single command in it
without entering. Alternatively, `nix run nixpkgs#devenv -- shell` works without
installing anything, and direnv users can run a one-time `direnv allow` to
auto-load the environment from the committed `.envrc`. The Postgres integration
suite is separate from this environment: it starts its own ephemeral database
via testcontainers and needs a running Docker daemon.

The workspace contains the dependency chain `apps/hubd` → `crates/application` →
`crates/domain`, with `crates/persistence` depending on both
`crates/application` and `crates/domain`, and the dev-only `crates/expect-table`
consumed by the domain crate's tests. Before finishing any change, run the
repository-wide validation sequence in [AGENTS.md](AGENTS.md) — the canonical
list of required commands and their setup notes — from the repository root.

## License

Signalbox is licensed under the [MIT License](LICENSE).
