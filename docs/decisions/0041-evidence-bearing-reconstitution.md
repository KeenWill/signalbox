# ADR-0041: Evidence-bearing active-turn reconstitution

- Date: 2026-07-18
- Supersedes: none
- Superseded by: none
- Depends on: [ADR-0004](0004-turn-and-attempt-lifecycle.md), [ADR-0027](0027-input-delivery-lifecycle.md), [ADR-0031](0031-direct-fatal-terminalization.md), and [ADR-0035](0035-domain-owned-persistence-reconstitution.md)
- Refines: ADR-0035's completeness rule for reconstituting an evidence-bearing active turn
- Decision questions: owning-turn correlation for stop proofs and wait subjects; exact recovery-wait operation evidence; session-scoped acceptance-tail completeness; fail-closed validation

## Context

ADR-0035 requires a purpose-specific complete projection and already names the current attempt, wait subject, owned operation classifications, and proof sources as turn-reconstitution facts. It does not make the validation pattern for those facts explicit enough to exclude two weaker implementations.

First, a loader could treat an active-phase discriminator and its locally well-formed payload as the durable authority: a stop value with an opaque proof, an approval request identifier, or a list of recovery-wait operation references. Each value can be valid in isolation while belonging to another turn, omitting an owned blocker, or contradicting the owning turn's attempt and operation history. Foreign keys and closed discriminators do not establish those aggregate correlations.

Second, a loader could fetch only accepted inputs currently labelled as pending steering for the active turn. That result cannot prove its own completeness. Omitting one matching row produces the same local shape as a session with no such row. This matters before safe-point consumption and every terminal path that must reclassify pending steering before releasing the slot. It also matters to restart scheduling, where omitted later accepted input can make acknowledged work disappear from the recovered session view.

The refinement needed here is a validation pattern, not a new lifecycle model: evidence-bearing phases are conclusions derived from the owning turn's complete facts, and accepted-input completeness is established over a session-scoped acceptance tail rather than inferred from a filtered result.

## Decision

### Active phases are correlated conclusions

A purpose-specific seam that reconstitutes an active turn supplies inert domain facts and returns the active phase only after domain-owned validation. The phase discriminator, nested identifiers, storage constraints, and caller assertions are inputs to that validation; none is authority by itself.

The complete input is scoped to one session and one owning turn. It includes every fact that can change whether the returned phase and its payload are the canonical conclusion for that turn at the seam's observation point. The domain validates ownership, identity equality, exact-set membership, lifecycle compatibility, and proof provenance together:

- `Active(Running)` correlates the turn with exactly one current attempt. A `StopRequested` attempt derives its complete canonical stop causes from that attempt's matching evidence and applied-result facts. An applied interrupt or stop-choice proof must be reconstructed from its matching applied command result and exact affected facts under ADR-0035; a proof copied into the attempt row or supplied independently is insufficient.
- `Active(AwaitingApproval)` correlates the exact open logical tool request with the same turn, its normalized request facts, its owning attempt history, and the transition that ended the prior current attempt. The projection proves that this request, rather than merely a request with a valid identifier, is the turn's one current approval subject and that no current attempt coexists with the wait.
- `Active(AwaitingRecoveryDecision)` correlates every referenced operation with the same turn and its issuing attempt or owned dependency. Its wait set is the exact canonical nonempty remainder obtained after classifying the complete owned operation set and applying all matching resolving evidence and accepted-risk facts. A referenced operation owned elsewhere, an omitted still-blocking operation, an extra resolved operation, or an unclassified owned issued operation fails reconstitution.

The same rule applies when startup reconstitution will end an abandoned attempt: the old attempt, its already-recorded stop causes, all owned issued operations and logical dependencies, and the evidence observed by the scan form one complete owning-turn input. The domain derives the matching `Lost` branch and any resulting wait or terminal candidate from that input. Persistence does not preassemble the stop proof, wait set, or recovery conclusion.

The seam rejects a phase whose complete owner facts support a different phase, a different stop-cause set, a different wait subject, or a different recovery-wait remainder. It does not trim extra facts, fill missing facts, or prefer the lifecycle row over the evidence. No effect is authorized from a failed reconstruction.

### Session-scoped acceptance-tail evidence

Every evidence-bearing active-turn projection also carries a **session acceptance tail**. The tail is anchored at the owning turn's origin accepted input and extends through the authoritative last acceptance position observed for that session by the same complete read. It contains every accepted input at every position in that closed interval, with the immutable session and delivery facts and the current disposition and correlation facts needed by the requested seam.

The domain validates that:

1. the anchor is the owning turn's exact origin in the same session;
2. all tail entries belong to that session and their identities and positions are unique;
3. the positions form the exact checked-successor sequence from the anchor through the observed last position, with no gap or later unrepresented position inside the claimed observation;
4. every origin, steering binding, consuming call, and reclassified turn named by a disposition has the ownership and lifecycle relationship required by the accepted decisions; and
5. the projection's pending-steering set, queue facts, and any consumption or reclassification candidates are derived from this validated tail rather than accepted as caller-selected subsets.

The last-position observation and tail must be session-scoped. A maximum computed only over rows already filtered by target turn, disposition, queue state, or relation kind is not a completeness witness. A boolean such as `all_steering_loaded`, a count without the corresponding ordered facts, or a list beginning after the turn's origin cannot substitute for the tail.

A purpose-specific scheduling seam may need earlier accepted-input and turn facts in addition to this tail to derive total order and eligibility. ADR-0035's general completeness rule still governs that projection. The tail requirement does not make facts before its anchor irrelevant, and it does not turn every reconstitution use case into a universal session document.

The persistence boundary obtains the anchor, session-scoped last-position observation, tail, and owner facts from one consistent observation suitable for the requested operation. The domain checks their semantic correlation. A later accepted input or lifecycle transition is ordinary concurrent staleness handled by guarded write and reload under ADR-0035; omission inside the claimed observation is durable corruption or an incompatible projection and fails closed.

### Boundary of the refinement

This record refines only the evidence-bearing active-turn and scheduling projections that need these facts. It does not change ADR-0035's permission for other read use cases to own smaller purpose-specific projections. A command-replay seam, historical audit read, or immutable snapshot load does not acquire an active-turn acceptance tail merely because it also uses domain-owned reconstitution.

The validated result remains a canonical owner aggregate, not a collection of free-standing proof factories. Internal validators may be shared with live transitions, but reconstitution performs no transition, chooses no owner action, generates no identity, and claims no commit.

## Invariants

- INV-001: stop and interrupt authority is reconstructed only from the matching applied result and complete owning-turn correlations; a stored identifier or nested proof-shaped payload is insufficient.
- INV-002: the session tail, owner evidence, and reconstitution failures are domain projections and values, not SQL rows, query objects, or framework types.
- INV-006 and INV-009: an active phase is returned only when the owning turn's exact attempt, dependency, operation, and wait facts establish that one closed phase shape and its exact payload.
- INV-007 and INV-016: the validated session acceptance tail prevents acknowledged input or pending steering from disappearing through a target- or disposition-filtered load.
- INV-010: an approval or recovery wait survives restart only as the exact subject derived from its complete owning-turn facts.
- INV-025: recovery preserves every physically ambiguous operation and derives the exact nonempty blocking remainder rather than trusting stored membership.
- INV-029: applied interrupt authority, stop retention, pending-steering reclassification, and slot release are evaluated from one correlated owner projection.
- INV-034: startup derives complete stop causes and recovery classification from the abandoned attempt's owner facts; reconstitution cannot manufacture a weaker restart-only phase.

## Strongest alternative

**Load the lifecycle row and follow only the references it names.** The phase discriminator selects one current attempt, approval request, or recovery-membership relation; foreign keys and per-row shape constraints ensure each reference exists, and persistence passes the resulting values to the domain.

This is rejected because it lets the stored conclusion select its own evidence. It can prove that every named child exists, but not that an unnamed owned blocker does not exist, that a named proof came from the matching applied result, or that a filtered steering query omitted nothing. The accepted aggregate rules require the conclusion to be derived from complete facts, not verified only against the facts it chose to name.

## Rejected alternatives

- **Trust database constraints to establish owner correlation.** Constraints remain defense in depth, but they do not derive complete stop causes, the canonical blocking-ambiguity remainder, or the absence of omitted accepted input.
- **Accept free-standing reconstructed proofs or waits.** Later code could pair a locally valid proof or request with another turn. Evidence-bearing values remain nested in the validated owner result.
- **Use only rows currently marked pending for the target turn.** The query predicate makes an omitted row indistinguishable from no row and cannot support safe-point or terminal completeness.
- **Use a caller-supplied count or completeness flag.** It repeats a conclusion without carrying the ordered facts needed for domain validation.
- **Always load the entire session history.** This would replace ADR-0035's purpose-specific projections with a universal document and add unrelated archived facts to the hot boundary. An anchored acceptance tail plus the seam's other complete owner facts is sufficient.
- **Redesign reconstitution around event replay.** ADR-0022 keeps guarded current-state records authoritative, and this proposal changes only validation of their domain projection.

## Consequences

Evidence-bearing loads are deliberately relational. They may read more accepted-input, attempt, dependency, operation, applied-result, and evidence facts than the lifecycle row directly names. Domain errors can distinguish a malformed phase, cross-owner evidence, a noncanonical stop or wait set, and an incomplete acceptance tail, while persistence adds safe record and query context.

Tests for these seams must include omission and cross-wiring cases, not only invalid discriminators: a proof from another turn, a wait naming another turn's request, an omitted ambiguous operation, an extra resolved operation, a missing interior acceptance position, and a tail whose claimed last position or session does not match.

The added completeness has a read and validation cost. It buys one authority path for live and restarted state: the same owner facts determine stop causes and waits, and accepted steering cannot disappear merely because a query filtered it out.

No existing domain, application, or persistence implementation is claimed by this proposal. Concrete implementation arrives only in a separately authorized slice.

## Scenario walkthroughs

- **S03:** Restart scheduling includes the complete queue projection ADR-0035 already requires. When that projection includes an evidence-bearing active turn, its session acceptance tail reaches the same session observation used for scheduling; when there is no active turn, the scheduling seam still proves its complete queue scope under ADR-0035. A queued accepted input cannot vanish because a later-state filter omitted its row, and eligibility is not derived from an incomplete claimed acceptance interval.
- **S04:** The startup scan reconstitutes the abandoned attempt with all of its turn-owned calls, dependencies, issued operations, prior stop causes, and newly observed evidence. The domain derives the matching `Lost` end and exact recovery wait or terminal candidate; a copied stop value or smaller ambiguous-operation set cannot control recovery.
- **S07:** The applied interrupt proof is correlated with its exact predecessor and applied result, any retained fatal causes remain part of the same current attempt, and a recovery-wait interrupt uses that wait's exact derived operation remainder. Before a terminal path releases the slot, the validated acceptance tail supplies every pending steering input that must be reclassified in original acceptance order.
- **S08:** Restart validates every accepted input from the active turn's origin through the session observation, so it cannot reconstruct one input as absent merely by loading only current pending rows. A safe-point transaction derives the complete ordered pending set from that tail; terminalization derives the complete ordered reclassification set from the same evidence.
- **S10:** Approval-wait reconstitution correlates the exact open tool request, normalized request facts, owner turn, and ended prior attempt, while proving that no current attempt coexists with the wait. The acceptance tail preserves later steering that remains pending while approval blocks; a bare request identifier cannot recreate the phase or permit dispatch.

## Open questions

- The implementation slice may choose projection types and internal validator sharing without exposing proof constructors or storage representations.
- Query planning and a consistent-observation technique remain persistence choices, subject to the completeness and guarded-staleness behavior above.
- Operational response to reported corruption remains open under ADR-0035; scheduling and effect authorization for the failed aggregate remain disabled.

## Explicit non-decisions

This record defines no new turn phase, attempt state, stop cause, wait kind, accepted-input disposition, queue priority, safe point, terminal outcome, startup precedence, recovery action, approval rule, or lifecycle transition. It does not change when steering is consumed or reclassified, what blocks terminalization, which evidence classifies a physical operation, or how an owner resolves ambiguity.

It does not choose SQL, table or column names, indexes, repository traits, aggregate API names, transaction isolation, locks, snapshots, caches, serialization, migration steps, diagnostics, repair behavior, or rollout policy. It does not require a durable last-position counter specifically; an implementation must supply an authoritative session-scoped observation that satisfies the decided tail validation.

It does not redesign ADR-0035's general reconstitution boundary, require every purpose-specific read to load the whole session, alter accepted ADR semantics outside the stated refinement, or implement any part of this proposal.
