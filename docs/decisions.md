# Decision log

An append-only, dated record of decisions below foundation weight. Each entry states context, the decision, rejected alternatives, and what it affects, in roughly ten to twenty lines. Foundation-weight changes — altering accepted ADR semantics, moving a boundary between domain, storage, wire, or framework representations, weakening an invariant, or introducing a technology that constrains several components — require a full record under [decisions/](decisions/README.md) instead. Unresolved questions live in [open-questions.md](open-questions.md).

## 2026-07-15 — Adopt a lightweight decision process

**Context.** The repository carried roughly fifty thousand words of design documentation against a few hundred lines of code. Normative content was duplicated across the ADRs, the decision ledger, the invariant catalog, the scenarios, the architecture narratives, and the testing strategy, and every change was required to reconcile all of them. The duplication and per-row status bookkeeping, not the existence of decision records, were the main cost to review and to agent-driven implementation.

**Decision.** Normative content lives in exactly one place; other documents link to it. The decision ledger is replaced by this log and [open-questions.md](open-questions.md). The five accepted ADRs (0001, 0003, 0004, 0005, 0027) remain the normative specification for decided semantics until superseded; executable tests progressively become the enforcement of record as slices are implemented. Ordinary decisions are made in pull requests and recorded here; full ADRs are reserved for foundation-weight changes. Derived documents (invariant catalog, architecture, testing strategy, process documents) shrink to overviews, catalogs, and links in follow-up changes, and the scenarios are frozen as design fixtures that convert to integration tests over time.

**Rejected alternatives.** Deleting `docs/decisions/` and making code comments and tests the primary specification immediately: most decided semantics have no implementing code yet, and recorded rejected alternatives are what prevent re-litigating settled questions. Keeping the full ledger process: its reconciliation cost outweighed its inventory value.

**Affects.** `docs/decision-ledger.md` (deleted), `docs/decisions.md` and `docs/open-questions.md` (created), `docs/decisions/README.md` (simplified), and ledger links in `README.md`, `CONTRIBUTING.md`, `AGENTS.md`, and `docs/architecture.md`. The invariant catalog, architecture, testing strategy, and process documents follow in separate pull requests.
