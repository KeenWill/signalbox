# ADR-0003: Session creation cause and transcript ancestry

- Status: Accepted
- Date: 2026-07-13
- Owners: Repository owner
- Reviewers: Codex (independent adversarial architecture review); no specialist human reviewer assigned
- Supersedes: none
- Superseded by: none
- Accepted with: ADR-0001, ADR-0004, ADR-0005, and ADR-0027 as one atomic foundation set
- Decision questions: owner-initiated baseline and typed extension of session creation cause; independent transcript ancestry; separation from versioned session configuration defaults; future multiple ancestry sources or merge

## Context

A session can be created because the owner starts a conversation, an application or schedule requests work, or parent work delegates a task. Independently, its initial semantic transcript context can be empty or derived from an earlier session frontier. Treating one of these facts as a proxy for the other would make ordinary forks look delegated and would force every delegated child to inherit its parent's transcript.

The first session types must preserve provenance without prematurely selecting transcript-copy storage, delegation-result behavior, or a general session graph.

## Decision

Every session records two required, independent, immutable creation facts:

1. **Creation cause** answers why this session exists.
2. **Transcript ancestry** answers where its initial semantic conversation context came from.

Creation cause is a typed value. The first implementable cause is `OwnerInitiated`. Application-initiated, scheduled, delegated, and any other causes are reserved extension examples rather than valid baseline values. The ADR that enables one must add a typed variant carrying the exact durable initiating domain identity; an unstructured string or generic placeholder reference is not a substitute.

Transcript ancestry in version one is either `None` or one immutable source consisting of a source session and an exact source transcript frontier. `None` explicitly means that no prior session transcript supplied initial semantic context; it does not mean that the session lacks task input, configuration, or a creation cause.

The pair is validated and stored atomically when the session is created. Neither value can be rewritten after the session identity becomes durable. Later changes to the source session do not change the descendant's initial context.

Session creation also establishes the first immutable version of model-selection-only `SessionConfigurationDefaults`, as defined by ADR-0027. Those defaults are operationally associated with the session but are not a third creation-provenance fact: an explicit owner command may install a new model-selection defaults version for future accepted origin input. Changing defaults never rewrites creation cause, transcript ancestry, or already accepted work; future configuration categories require their own ADR before joining this value.

Cause and ancestry may vary independently. For example:

| Creation cause | Transcript ancestry | Meaning |
| --- | --- | --- |
| Owner-initiated | None | Start an empty conversation |
| Owner-initiated | Single source frontier | Fork an earlier conversation |
| Future delegated cause | None | After ADR-0002 defines its initiator identity, create a child from an explicit task brief without transcript inheritance |
| Future delegated cause | Single source frontier | After ADR-0002 defines its initiator identity, delegate work and deliberately seed it from one transcript frontier |

Initial ancestry has exactly one source or none. Signalbox does not infer ancestry from related-session links, task briefs, copied text, or delegation.

## Terminology

```text
CreateSession = {
    command_id: DurableCommandId,
    provenance: SessionCreationProvenance,
    initial_configuration_defaults: SessionConfigurationDefaults
}

SessionCreationProvenance = {
    cause: CreationCause,
    ancestry: TranscriptAncestry
}

CreationCause =
    OwnerInitiated

TranscriptAncestry =
    None
  | SingleSource {
        source_session: SessionId,
        source_frontier: TranscriptFrontier
    }
```

The pseudocode states the complete first implementable cause set, not final Rust spelling. New variants are additive domain decisions made with the feature that defines their initiating identity; the initial type does not contain uninhabitable placeholders.

**Transcript frontier** here identifies an immutable source boundary in semantic history. It is related to, but need not share the storage representation of, the per-model-call context frontier.

## Invariants

- INV-003 and INV-030 are preserved and made precise by this ADR.
- Session creation must atomically validate the cause value, any reference required by a future accepted cause variant, the ancestry source, and the source frontier before acknowledgement.
- A source session cannot later rewrite a created session's initial context by advancing, archiving, or changing presentation.
- No creation-cause variant implies ancestry, and ancestry never implies a creation cause.
- Initial context has at most one transcript ancestry source.
- Session defaults are explicitly versioned separately from immutable creation provenance; later versions affect only future input acceptance.

## Strongest alternative

Represent every related session with a general graph whose typed edges include fork, delegation, merge, and derivation, then compute creation provenance and initial context from that graph. This is expressive and could avoid a later ancestry migration.

It is rejected for version one because edge combinations would need merge order, conflict, retention, and cancellation semantics before the first session can be created. Separate cause and single-source ancestry preserve a clear extension point without claiming those semantics.

## Rejected alternatives

- **One `parent_session` field.** It cannot distinguish why the session exists from which transcript seeded it.
- **Delegation always inherits parent transcript.** It leaks context implicitly and prevents a brief-only child.
- **Fork as a creation cause only.** A user fork and an application-created fork have different causes even when ancestry matches.
- **Mutable ancestry that tracks the source.** This would rewrite historical model context and make provenance depend on current source state.
- **Multiple ancestry sources now.** Ordering, deduplication, conflicts, and merge authorship are not defined.

## Consequences

Session creation has slightly more required data and validation. Clients and future protocols must distinguish an explicit empty ancestry from an omitted or invalid field.

Fork and delegation can evolve independently. Provenance remains inspectable even if the physical transcript representation later changes from references to copies or a hybrid.

## Scenario walkthroughs

- **S01:** `OwnerInitiated` plus `None` creates an empty interactive session and its initial model-selection-defaults version. The first accepted input is not ancestry and freezes its own effective model configuration from that exact version.
- **S17:** An owner-created fork stores `OwnerInitiated` and the selected source session/frontier. Later source activity cannot change the fork.
- **S18:** ADR-0002 must add the delegated-cause variant and define its exact durable parent-work identity together with delegation lifecycle. The child may then have no ancestry and receive a task brief as new input, or explicitly select one source frontier.
- **S19:** Parent cancellation cannot erase or rewrite the child's cause or ancestry. ADR-0002 remains blocking before a child-wait phase or parent-cancellation transition is exposed.

## Extension implications

A future merge feature may add a new typed derivation or ancestry model after defining source ordering and conflict semantics. Existing `None` and `SingleSource` values must retain their exact meaning through any migration.

Related-session links may be added for navigation, results, or lifecycle policy without becoming transcript ancestry. Storage may copy derived context, retain a reference, or use both, provided the immutable source provenance is preserved.

## Open questions

- Application, schedule, and delegation ADRs must decide whether they add creation causes and, if so, the exact durable initiating identity carried by each new variant.
- Copy, reference, or hybrid transcript storage remains open under the persistence ADR.
- Retention behavior when an ancestry source is later subject to destructive deletion remains open.
- Multiple-source ancestry and merge remain future decision scope.

## Explicit non-decisions

This ADR does not decide delegation-result representation, cancellation propagation, detached children, archive coupling, transcript merge, destructive retention, session ownership, authentication, or storage schema. It does not define placeholder application, schedule, or delegation variants before their initiating identities exist.
