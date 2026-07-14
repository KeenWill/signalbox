# Signalbox

Signalbox is an independently designed, personal LLM session platform for durable, resumable work across machines and interfaces. An always-on central hub will coordinate conversation state, model calls, approvals, tools, and outbound execution runners while preserving enough provenance to explain and recover accepted work.

> **Status:** design and foundation phase. This repository currently contains architectural framing, development policy, and mechanical Rust workspace scaffolding, not a usable product. The first domain terminology and lifecycle foundation is accepted; APIs, protocols, storage models, and implementation details are not yet stable.

Intended product surfaces include a central hub, remote runners, shared protocols and tool infrastructure, a terminal client, a web client, and native macOS and iOS clients.

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

The hub is the source of truth; a client device and an execution machine need not be the same machine. See [Architecture](docs/architecture.md) for the boundaries and important qualifications behind this sketch.

## Design documents

- [Vision](docs/vision.md)
- [Architecture](docs/architecture.md)
- [Glossary](docs/glossary.md)
- [Scenarios](docs/scenarios.md)
- [Invariant catalog](docs/invariants.md)
- [Decision ledger](docs/decision-ledger.md)
- [Testing strategy](docs/testing-strategy.md)
- [Architecture decision records](docs/decisions/README.md)

Project participation is described in [CONTRIBUTING.md](CONTRIBUTING.md), security reporting in [SECURITY.md](SECURITY.md), and repository guidance for coding agents in [AGENTS.md](AGENTS.md).

## Development

Install [rustup](https://rustup.rs/). The repository's `rust-toolchain.toml` makes rustup select the pinned minimal stable toolchain with rustfmt and Clippy.

The workspace contains the dependency chain `apps/hubd` → `crates/application` → `crates/domain`. Run the full local validation sequence from the repository root:

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
cargo metadata --no-deps --format-version 1
git diff --check
```

## License

Signalbox is licensed under the [MIT License](LICENSE).
