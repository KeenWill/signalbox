# Living specification

These pages, together with INV-tagged tests indexed in
[invariants.md](../invariants.md) and public API shapes in
[domain-spine.md](../domain-spine.md), are the normative specification of
Signalbox's implemented cross-component and wire behavior. `AGENTS.md` is the
guidance for agents working on the repository; this README owns the conventions
the pages follow. Each page names the code ref it was last verified against and
is updated in the same pull request as any behavior change it describes. A
verification reference names that pull request as `` PR #N (`branch-ref`) `` and
may narrow the claim to the surface the pull request settled, either as a
semicolon tail inside the parentheses — `` PR #N (`branch-ref`; <scope>) `` — or
as prose following the parenthetical; a scope tail is free-form prose that must
render as more than whitespace and block-quote markers, may name code in
backticks without the span's own parentheses closing the reference, and must
stay inside the reference's own block, and a page may carry one reference per
verified surface. One tail form is semantically special: a pull request that
landed inside another pull request's merge (a stack merged from its top leaves
inner pull requests with no first-parent merge commits) cites its carrier with
the exact tail `` PR #N (`branch-ref`; via PR #M `carrier-branch`) ``, and the
reference is accepted only when `#N` itself has no first-parent merge commit and
the carrier's number and branch either match one or name the single in-flight
pull request (the event identity, or the checked-out branch locally, and never
the reference's own pull request) — a carrying merge cannot precede the
carrier's own pull request. Inheritance preserves a carried reference's tail
exactly, so a base page's carrier cannot be dropped by a child that merely
repeats it; a matching inherited carrier that has not yet entered integration
history stays accepted on that basis alone, deferring judgment to the
integration branch's own run once the stack lands, while a primary with its own
merge commit or a reference naming itself as carrier rejects even when
inherited. `scripts/check_docs_consistency.py` enforces this form.

Every paragraph on a page belongs to exactly one of three categories; a page
that cannot say which category a paragraph is in has a defect:

- **Implemented behavior**: what a page states by default.
- **Committed unimplemented functionality**: a capability the owner has decided
  will exist, recorded because it constrains what a present change may do. Such
  a paragraph names itself unimplemented, states that no present surface
  provides it, and carries only that compatibility constraint. It is neither a
  description of the system nor an open question, and it is admitted only where
  a present contract must stay compatible with it.
- **Deferred or undecided work**: recorded in
  [open-questions.md](../open-questions.md), its one home; a page's "Open edges"
  section points to it and carries no speculative prose.

Conventions: pages state implemented behavior, plus the committed unimplemented
functionality that constrains it, per the three categories above; pages state
behavior, not rationale — a load-bearing choice may carry one "Why:" sentence;
invariant references use INV tags resolved through the generated
[invariants.md](../invariants.md) index; deferred or undecided items are
recorded in [open-questions.md](../open-questions.md) and surfaced as pointers
in each page's "Open edges" section; a topic normatively owned by a sibling page
is linked, never restated.

## Pages

- [Conversation import](conversation-import.md)
- [Sessions and the transcript](sessions-and-transcript.md)
- [Turn lifecycle and scheduling](turn-lifecycle-and-scheduling.md)
- [Goal mode](goal-mode.md)
- [Model-call execution](model-call-execution.md)
- [Usage evidence](usage-evidence.md)
- [Tool loop](tool-loop.md)
- [Git authority threat model](git-authority-threat-model.md)
- [Web egress threat model](web-egress-threat-model.md)
- [Runner protocol and placement](runner-protocol.md)
- [Review workflows](review-workflows.md)
- [Persistence protocol](persistence-protocol.md)
- [Blob storage](blob-storage.md)
- [File and media interpretation](file-and-media.md)
- [Identity, commands, and telemetry correlation](identity-and-commands.md)
- [Model-runtime substrate](runtime-substrate.md)
- [Model and session settings](model-session-settings.md)
- [Configuration and credentials](configuration-and-credentials.md)
- [Credential availability](credential-availability.md)
- [Process protocol](process-protocol.md)
- [Repository watch and event dispatch](repo-watch.md)
- [Pull-request convergence reconciliation](convergence-reconciliation.md)
- [Workspace instructions and skills](workspace-instructions.md)
- [Program substrate](program-substrate.md)
- [Evaluation system](eval-system.md)
