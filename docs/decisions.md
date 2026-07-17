# Decision log

An append-only, dated record of decisions below foundation weight, newest first. Each entry states context, the decision, rejected alternatives, and what it affects, in roughly ten to twenty lines. Foundation-weight changes — altering accepted ADR semantics, moving a boundary between domain, storage, wire, or framework representations, weakening an invariant, or introducing a technology that constrains several components — require a full record under [decisions/](decisions/README.md) instead. Unresolved questions live in [open-questions.md](open-questions.md).

## 2026-07-16 — Delivery-request caller payload representation

**Context.** ADR-0027 defines four discriminated delivery requests. Three create origin work and carry a model-selection override bound to the caller's expected session-defaults version; safe-point steering must carry no independent configuration. The first caller-payload slice needs a Rust representation without implementing command handling or authoritative-state validation.

**Decision.** Represent `DeliveryRequest` as a domain enum with named fields for its exact caller-supplied payload. Group the expected defaults version and `ModelSelectionOverride` in a `PerInputConfigurationChoices` value with private fields and read-only accessors. Give `NextSafePoint` only its expected active-turn field, making an independent configuration choice unconstructible.

**Rejected alternatives.** Optional configuration on every variant: it would admit both missing origin configuration and forbidden steering configuration. Separate version and override fields on each origin-producing variant: it would repeat one semantic unit and make partial refactors easier to cross-wire. A wire-oriented request struct with nullable fields: domain construction would no longer establish the discriminated payload.

**Affects.** `crates/domain/src/delivery_request.rs` and its re-exports from `crates/domain/src/lib.rs`; acceptance validation, command identity, content, storage, and wire mappings remain later slices.

## 2026-07-15 — Ordinal session-defaults versions

**Context.** ADR-0027 versions session model-selection defaults — creation establishes version one and each explicit update installs a complete later immutable version — without fixing a version representation. The caller's expected version participates in equality comparison at acceptance.

**Decision.** `SessionConfigurationDefaultsVersion` is a private ordinal counter starting at one with a successor operation; equality is the acceptance-time comparison. Storage and wire encodings remain open.

**Rejected alternatives.** UUID version identities: they lose the accepted "version one" and succession semantics. Timestamps: wall-clock coupling and collision risk without adding meaning.

**Affects.** `crates/domain/src/configuration.rs`.

## 2026-07-15 — UUID-backed model-selection keys

**Context.** ADR-0027 defines `DirectModelSelection` as a canonical domain-owned key with immutable semantic meaning and `ModelAlias` as an owner-configured alias name, and represents `FrozenAliasDefinition` as "an immutable definition version or value selecting exactly one `DirectModelSelection`", leaving concrete encodings open. The first configuration slice needs backing values.

**Decision.** `DirectModelSelection` and `ModelAlias` are private UUID-backed newtypes with deliberately named UUID conversions, following the representation convention the amended ADR-0001 accepted for identity newtypes. `FrozenAliasDefinition` takes the value form: it stores exactly the selected `DirectModelSelection`. Deployment-key mapping, storage, wire, display, and serialization encodings remain open, as does adding a definition-version identity if a later slice needs one.

**Rejected alternatives.** String-backed keys: they invite provider-native unnormalized identifiers into domain equality, which ADR-0027 forbids. A definition-version identity inside `FrozenAliasDefinition` now: nothing constructible needs it yet.

**Affects.** `crates/domain/src/configuration.rs`; the `define_identity` macro becomes crate-visible for domain keys that follow the identity representation convention.

## 2026-07-15 — Adopt a lightweight decision process

**Context.** The repository carried roughly fifty thousand words of design documentation against a few hundred lines of code. Normative content was duplicated across the ADRs, the decision ledger, the invariant catalog, the scenarios, the architecture narratives, and the testing strategy, and every change was required to reconcile all of them. The duplication and per-row status bookkeeping, not the existence of decision records, were the main cost to review and to agent-driven implementation.

**Decision.** Normative content lives in exactly one place; other documents link to it. The decision ledger is replaced by this log and [open-questions.md](open-questions.md). The five accepted ADRs (0001, 0003, 0004, 0005, 0027) remain the normative specification for decided semantics until superseded; executable tests progressively become the enforcement of record as slices are implemented. Ordinary decisions are made in pull requests and recorded here; full ADRs are reserved for foundation-weight changes. Derived documents (invariant catalog, architecture, testing strategy, process documents) shrink to overviews, catalogs, and links in follow-up changes, and the scenarios are frozen as design fixtures that convert to integration tests over time.

**Rejected alternatives.** Deleting `docs/decisions/` and making code comments and tests the primary specification immediately: most decided semantics have no implementing code yet, and recorded rejected alternatives are what prevent re-litigating settled questions. Keeping the full ledger process: its reconciliation cost outweighed its inventory value.

**Affects.** `docs/decision-ledger.md` (deleted), `docs/decisions.md` and `docs/open-questions.md` (created), `docs/decisions/README.md` (simplified), and ledger links in `README.md`, `CONTRIBUTING.md`, `AGENTS.md`, and `docs/architecture.md`. The foundation ADRs' `Decision-ledger questions` header lines become `Decision questions` and ADR-0003's "future ledger scope" becomes "future decision scope" as meaning-preserving reference corrections. The invariant catalog, architecture, testing strategy, and process documents follow in separate pull requests.
