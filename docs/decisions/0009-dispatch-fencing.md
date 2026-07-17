# ADR-0009: Dispatch fencing

- Status: Proposed
- Date: 2026-07-17
- Owners: Repository owner
- Reviewers: none yet; this record is authoritative only if the owner accepts it
- Supersedes: none
- Superseded by: none
- Depends on: the accepted foundation set ([ADR-0001](0001-domain-terminology-and-identity.md), [ADR-0003](0003-session-creation-and-transcript-ancestry.md), [ADR-0004](0004-turn-and-attempt-lifecycle.md), [ADR-0005](0005-model-call-retry-semantics.md), [ADR-0027](0027-input-delivery-lifecycle.md))
- Coordinated with: Proposed [ADR-0010](0010-initial-scheduler-mechanics.md) (the transactions that carry this fence) and Proposed [ADR-0022](0022-persistence-representation.md) (the guarded-row discipline and reserved dispatch-generation placement); if either changes, the references here follow it
- Decision questions: what fence a stale dispatch or result presents; the generation value's scope and ordering; the exact result-acceptance compare-and-set (INV-011); duplicate-delivery disposition; late-result routing after terminal classification

## Context

The open question ["Fencing that rejects stale dispatches and results"](../open-questions.md#scheduling-and-runners-reserved-adr-0008-adr-0009-adr-0010) blocks runner dispatch (S05, S06, S12), with a recorded leaning toward durable attempt identity plus generation or equivalent compare-and-set. The behavior is already accepted in outline: INV-011 requires that a stale physical attempt or dispatch generation cannot advance or overwrite current logical state, INV-021 requires every runner result to identify its tool attempt and authorized dispatch generation or equivalent fence, and the [glossary](../glossary.md#dispatch-generation) defines a dispatch generation as the token identifying which scheduler dispatch is currently authorized to report for a physical attempt while marking its representation and monotonicity provisional. This record proposes the fence mechanics those rows leave open. The catalog rows remain the statements of record; nothing here is normative unless the owner accepts it.

The accepted lifecycle fixes what the fence must protect. ADR-0001 fixes the ownership cardinalities: each tool attempt belongs to exactly one tool request and exactly one issuing turn attempt, and a tool attempt is not "a scheduler delivery retry." ADR-0004's terminalization guard closes every owned logical tool request "so a scheduler cannot dispatch it after the turn releases its slot," and its ambiguity rules make a terminal `Ambiguous` operation immutable: separate resolving evidence may clear the blocking uncertainty, but nothing reopens the physical disposition. The domain crate already implements `ToolRequestId` and `ToolAttemptId` as distinct identities and carries exact `ToolAttempt(ToolAttemptId)` references in `NonEmptyIssuedOperationRefs` ([`crates/domain/src/turn_lifecycle.rs`](../../crates/domain/src/turn_lifecycle.rs)); tool state machines do not exist yet, so this proposal constrains a future slice rather than existing code.

## Decision

### The fence pair

Every authorization to deliver one tool attempt to an executor is a durable **dispatch** identified by the pair of the attempt's identity and a **dispatch generation**: a per-attempt ordinal starting at one and incremented only by the guarded dispatch-authorization transition. Generations are totally ordered within one attempt and meaningless across attempts; comparing generations of different attempts establishes nothing, exactly as matching identifier bytes establish nothing under INV-001. The fence applies to both placements — a hub-local executor and a selected runner — so result acceptance has one shape regardless of where the effect ran.

This resolves the glossary's provisional points as follows: the generation is monotonic, and its scope is one tool attempt. A monotonic ordinal is chosen over an equality-only token because "stale" must be decidable, not merely "not current": an ordered value lets the hub classify a late envelope as superseded rather than unknown, and the audit trail reads in dispatch order.

### Fence at dispatch authorization

The transaction that authorizes a dispatch validates, with current-state predicates over durable rows, that the tool request is authorized and not closed or terminally disposed, that the issuing turn attempt is the turn's current live attempt and not stop-requested or dispatch-guarded under ADR-0004, and that the turn holds its slot in `Active(Running)`. The same transaction increments the attempt's current generation and records the dispatch fact. A stale scheduler pass therefore cannot mint a dispatch: after ADR-0004's terminalization guard closes the request, the guarded update matches zero rows and fails loudly rather than dispatching (the transaction discipline proposed by ADR-0022). Whether a second dispatch of the same attempt is ever permitted — as opposed to a new policy-authorized attempt — is tool-effect policy reserved for ADR-0011 through ADR-0014; this record only guarantees that when one is authorized, exactly one generation is current.

### Fence at result acceptance

Every result envelope names its tool attempt and echoed dispatch generation (INV-021). Result acceptance is one compare-and-set transaction whose predicate validates all of the following against current durable state, matching INV-011's wording exactly:

1. the named attempt exists and its owning tool request is the recorded owner — the correlation is read from durable ownership rows, never trusted from the envelope;
2. that attempt's issuing turn attempt is the recorded issuer — likewise read from durable ownership rows, never trusted from the envelope;
3. the attempt is nonterminal; and
4. the echoed generation equals the attempt's current generation.

The first envelope satisfying the predicate advances the attempt exactly once to its terminal classification under the turn aggregate's rules. The compare-and-set gates *state advancement* only: an envelope that fails it never advances or overwrites current logical state, but failing the predicate is not by itself an error. Whether such an envelope is acknowledged so the runner stops retrying, rejected, or retained as evidence is decided by the duplicate-and-stale classification below — which is evaluated even when predicate 3 now fails because a prior result already terminalized the attempt.

### Duplicate and stale delivery

Runners retry delivery until acknowledged (S12), so redelivery is normal, and INV-012 leaves result-delivery deduplication operation-specific. This classification runs for every envelope that does not advance state under the compare-and-set above, including the ordinary case where the attempt it names is already terminal from the first applied result. For tool results: a redelivered envelope structurally equal to the one already applied for the same attempt and generation is acknowledged idempotently without applying anything again, even though the now-terminal attempt makes it fail the acceptance predicate. An envelope for the same attempt and generation whose content differs from the applied result is conflicting delivery: it is rejected and retained as typed audit evidence when policy requires. An envelope with an older generation, a terminal attempt, or a turn that has released its slot is stale: it is rejected from state advancement and, per S12, recorded as duplicate/stale evidence if audit policy requires or discarded, without being applied.

One late arrival is deliberately not discarded silently. When the named attempt is terminal `Ambiguous`, an authenticated late result is a candidate for ADR-0004's separate resolving-evidence path: it may clear the blocking uncertainty or refine an `AwaitingRecoveryDecision` wait to the exact remaining set, without ever reopening the terminal physical disposition or being applied as a second first-class result. Who may record such evidence, and its exact representation, remain the open questions S06 already records.

### What the fence is not

A valid attempt-and-generation pair fences transport staleness only. It is not proof of outcome truth, does not bypass classification precedence, policy, or approval rules, and cannot construct any purpose-specific proof reserved by ADR-0001. Model calls carry no dispatch generation: provider interactions originate in the hub, and staleness there is already governed by ADR-0005's call identity and outcome-authority transfer. This record adds no lease, heartbeat, or liveness token; nothing durable references a live process or connection (INV-010).

## Invariants

If accepted, this record proposes the enforcement mechanics of INV-011 (the exact result-acceptance predicate above), INV-021 (the envelope's required attempt and generation fields), and the tool-result case of INV-012's operation-specific result deduplication, and it relies on INV-006 (terminal dispositions never reopen) and INV-010. The catalog rows remain unchanged while this record is Proposed; acceptance adds enforcement links without duplicating these rules.

## Strongest alternative

**Equality-only random token per dispatch.** Rejected as the primary representation: a random token still needs a durable "current" pointer for the compare-and-set, so it saves nothing, while losing the ability to classify a late envelope as superseded and losing dispatch-order audit. The glossary's "or equivalent fence" language stays satisfiable by protocol design later; the durable value proposed here is ordered.

## Rejected alternatives

- **Lease- or heartbeat-based fencing.** Rejected: expiry-based ownership reintroduces the live-process dependence INV-010 excludes and invents a wall-clock split-brain window that compare-and-set does not have.
- **Fence keyed on runner identity or connection.** Rejected: runner identity, enrollment, and authentication are reserved (ADR-0015 through ADR-0018), and connection state is transient by the architecture's sources-of-truth table.
- **One global generation sequence.** Rejected: a hub-wide counter cannot express "the current dispatch of this attempt" without a per-attempt pointer anyway, and it manufactures cross-attempt ordering that means nothing.
- **Attempt identity alone, no generation.** Rejected: it cannot distinguish an authorized redelivery from a superseded one within a single attempt, and the accepted glossary behavior already requires a generation or equivalent.

## Scenario walkthroughs

- **S03 (restart):** Restart discards scheduler memory. A pre-restart pass that never committed its dispatch authorization left no durable generation, so nothing was dispatched. One that committed but never sent leaves a durable current generation; the startup scan ends the abandoned turn attempt and classifies the tool attempt from evidence, after which the attempt is terminal and predicate 3 rejects any later envelope. A stale pass surviving as a queued in-process action cannot authorize anything after the scan: its guarded predicates match zero rows.
- **S05 (runner disconnects during a harmless tool):** The disconnect is classified; the attempt ends known-failed or ambiguous. If policy permits repetition for the proven read-only operation, the repeat is a new tool attempt with a new identity whose generations start again at one. A late first result names the old attempt; predicate 3 rejects it, and it is retained as evidence per policy. It cannot overwrite the current attempt because it does not name it.
- **S06 (runner disconnects during a potentially irreversible tool):** The attempt ends terminal `Ambiguous` and the turn waits on exactly that reference. A late authenticated result is routed to the resolving-evidence path: it may refine or close the exact wait under ADR-0004 without a second application and without rewriting the terminal disposition. Duplicate redeliveries of that late result add nothing: the evidence path is governed by its own typed records, and the attempt remains terminal.
- **S12 (stale or duplicated runner result):** An envelope for generation 3 arriving after generation 4 was assigned fails predicate 4 and is recorded or discarded as stale. A redelivered copy of the already-applied generation-4 result is acknowledged idempotently without a second application. A stale success can never overwrite a newer failure, cancellation, or reconciliation state, because every acceptance path runs the same compare-and-set.

## Open questions

- Result acknowledgement protocol, retention policy for rejected stale or conflicting envelopes, and the compatibility window remain open (S12).
- Who may record separate resolving evidence for a terminal-ambiguous attempt, and its representation, remain open (S06).
- Storage placement of the current generation and per-dispatch records (an attempt column, a dispatch record, or both) is reserved to the persistence slice under ADR-0022's open question; this record fixes the semantics, not the DDL.
- Wire encoding of the fence fields belongs to the protocol decisions (reserved ADR-0019 through ADR-0021).

## Explicit non-decisions

This record does not decide runner placement, capabilities, selection, pinning, or enrollment (reserved ADR-0008, ADR-0015 through ADR-0018), tool execution lifecycles, risk taxonomy, retry or repetition policy (reserved ADR-0011 through ADR-0014), resource limits, or any protocol or storage encoding. It selects no dependency and adds no code. Field and table names above are illustrative shapes for review, not a final Rust, storage, or wire API.
