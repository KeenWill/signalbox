# ADR-0037: Baseline accepted-input user content

- Date: 2026-07-18
- Supersedes: none
- Superseded by: none
- Depends on: [ADR-0001](0001-domain-terminology-and-identity.md), [ADR-0019](0019-process-protocol.md), [ADR-0021](0021-compatibility-and-negotiation.md), [ADR-0022](0022-persistence-representation.md), [ADR-0027](0027-input-delivery-lifecycle.md), [ADR-0034](0034-durable-command-storage-and-equality.md), and [ADR-0036](0036-initial-semantic-transcript-entries.md)
- Refines: ADR-0027's `UserContent` placeholder, ADR-0034's structural equality for `SubmitInput`, and ADR-0036's accepted-input content reference
- Resolves: the concrete accepted-input content algebra needed for the first `SubmitInput` slice
- Decision questions: initial content variants; constructibility; canonical equality; persistence and protocol mappings; semantic-entry correlation; extension boundary

## Context

ADR-0027 makes `UserContent` a caller-supplied semantic field of `SubmitInput`, requires the accepted input to own that content immutably, and includes it in durable-command replay equality. ADR-0034 requires replay to compare the reconstructed typed domain value rather than serialized bytes. ADR-0036 makes `OriginAcceptedInput` reference that authoritative accepted input instead of copying content into semantic history. The concrete `UserContent` algebra remains undefined, blocking the first complete input-acceptance transaction and its storage mapping.

The baseline needs a value narrow enough to implement without choosing attachment identity, rich-text structure, media retention, prompt rendering, or client presentation. It must also have equality that survives storage and wire round trips without allowing a mapper, database collation, or serializer to change accepted intent.

## Decision

### One closed text variant

The complete baseline algebra is:

```text
UserContent =
    Text {
        value: NonEmptyUnicodeText
    }
```

This is semantic pseudocode, not final Rust, storage, Protobuf, or client API spelling. `Text` is the only constructible variant. There is no generic block, metadata map, attachment, URI, byte payload, role, rendered message, or “other” case.

`NonEmptyUnicodeText` is a decoded Unicode scalar-value sequence that:

- contains at least one scalar value; and
- contains no U+0000, which PostgreSQL character types cannot store.

Zero-length input and input containing U+0000 cannot construct the canonical typed command. They are boundary failures before owner-global command lookup and claim no durable command identifier. Whitespace-only text is constructible: spaces, tabs, line endings, and other Unicode whitespace are content, not evidence that the user supplied no value.

The domain performs no trimming, line-ending rewriting, Unicode normalization, case folding, whitespace collapsing, language interpretation, markup parsing, or semantic equivalence. In particular, canonically equivalent-looking but differently encoded Unicode scalar sequences remain different domain values unless boundary decoding already produced the same scalar sequence.

### Exact equality and immutable ownership

`UserContent` equality compares the closed variant and the exact ordered Unicode scalar-value sequence. For the baseline's one variant, two values are equal exactly when their decoded text values are equal. Database collation, rendered appearance, tokenization, a digest, and serializer bytes are not equality authorities.

After purpose-specific boundary decoding and construction, this equality participates directly in ADR-0034's structural `SubmitInput` comparison. Different JSON escaping or Protobuf encodings that decode to the same text therefore replay equally. A change in whitespace, line endings, normalization form, case, or any other scalar makes the payload different and makes reuse of an already claimed command identifier conflicting reuse.

The accepted input owns the one immutable authoritative `UserContent` value. Delivery mode, configuration, semantic-history projection, provider rendering, and client rendering do not rewrite it. ADR-0027's exclusion of editing accepted input remains unchanged.

No maximum length is part of the domain value or its equality. Frame limits, admission limits, quotas, and other resource governance remain separate boundary or policy decisions. Such a limit may reject before typed command construction, but it must not truncate, summarize, normalize, hash-substitute, or otherwise create a different `UserContent`; a policy whose evolution could prevent replay of a formerly constructible command requires explicit compatibility treatment rather than a silent equality change.

### Persistence mapping

The normalized accepted-input record stores the baseline text value in a non-null PostgreSQL `text` field within a purpose-specific typed content record or equivalent checked shape. Its mapping is explicit and fallible:

- writing maps only a constructed `UserContent::Text`;
- reading rejects missing, empty, unknown-variant, forbidden-payload, or otherwise undecodable records as storage corruption under ADR-0035; and
- reading reconstructs the exact text without applying collation-based equality or normalization.

The storage representation may carry a closed content-kind discriminator to support checked shape and later migrations, but table and column names are implementation choices. A future content variant receives its own typed columns or relation and migration; it is not serialized into a generic JSON or byte envelope merely to avoid schema evolution.

### Semantic-history correlation

ADR-0036 remains the semantic-history authority. `OriginAcceptedInput` carries only the authoritative `AcceptedInputId`; resolving that entry obtains the immutable content from the referenced accepted input. It does not copy text into the semantic entry, mint a second content identity, or use rendered text as the correlation.

Equal text in separate accepted inputs remains separate accepted content occurrences and produces distinct origin entries when each turn becomes eligible. Content equality establishes neither accepted-input identity nor semantic-entry identity.

### Protocol mapping and evolution

ADR-0019's process-protocol boundary maps a version-one typed text content variant to `UserContent::Text` explicitly. Protobuf and proto3 JSON decoding happen before domain construction; malformed text, empty text, U+0000, an unset variant, and content outside the negotiated version or capability cannot enter the domain. Boundary failures follow ADR-0019's non-claiming rejection rules, while unnegotiated variants follow ADR-0021's incompatibility rules.

The first `SubmitInput` protocol fixture corpus must cover:

- an ordinary text command and its equal replay through an equivalent wire spelling;
- rejection of empty text and U+0000 before command lookup;
- preservation of whitespace and line endings;
- inequality of normalization-distinct Unicode and other one-scalar differences; and
- an unsupported content variant as negotiated incompatibility rather than silent omission.

A future non-text content kind requires an additive typed domain variant plus a foundation decision defining its identity or value boundary, equality, ownership, durability, limits, protocol capability, and semantic-history projection. It cannot be introduced by treating a URI, attachment descriptor, rich-text document, or serialized block list as baseline text.

## Invariants

- INV-005: accepted content, its semantic-entry reference, provider rendering, protocol representation, and client presentation remain distinct correlated representations.
- INV-007: acknowledgement follows durable storage of the exact immutable constructed content together with the rest of input acceptance.
- INV-012: `SubmitInput` replay compares exact reconstructed `UserContent` domain values; encodings, collations, digests, and rendering do not determine equality.

## Strongest alternative

Define an extensible block algebra now with text, images, files, URIs, and structured metadata. It would reduce the chance of a later top-level API shape change and could match contemporary multimodal provider APIs.

It is rejected because none of those values yet has an accepted identity, retention, authority, size, security, provider-rendering, or client-capability boundary. A generic early block would either make those semantics optional conventions or freeze a wire/provider model into the domain. The selected one-variant algebra is intentionally additive: a later decision can add a typed variant once it can define the whole boundary.

## Rejected alternatives

- **Normalize Unicode, newlines, case, or surrounding whitespace.** This can improve search and cross-platform display consistency, but it changes caller content and makes replay depend on a normalization policy. Search and presentation may maintain derived normalized projections without changing the authoritative value.
- **Reject whitespace-only text.** That requires interpreting formatting as absence. The baseline has a mechanically exact empty/nonempty boundary and preserves content without language- or UI-specific judgment.
- **Store arbitrary bytes.** It would preserve U+0000 and non-text payloads but would abandon the decoded-text contract required by the first clients and push encoding interpretation into every renderer and provider mapper.
- **Store a generic JSON content document.** It would avoid migrations for later variants but create an unconstrained second domain algebra and contradict the typed normalized persistence direction.
- **Copy text into `OriginAcceptedInput`.** ADR-0036 already rejects two authoritative copies that can diverge.
- **Use rendered or provider-ready text as the accepted value.** Prompt construction and client presentation may vary without changing accepted intent; collapsing them violates INV-005.

## Consequences

The first input slice can use a small checked domain value and a native PostgreSQL text mapping. Exact preservation makes replay and restart deterministic, but visually similar Unicode spellings, line-ending styles, and whitespace remain distinct. Search, moderation, display cleanup, and provider prompt construction need derived projections when introduced.

U+0000 is excluded from the baseline even though it is a Unicode scalar value because the selected PostgreSQL text representation cannot round-trip it. Supporting arbitrary Unicode including U+0000 would require a different storage representation and a superseding or refining decision.

## Scenario walkthroughs

- **S01:** The client submits nonempty text. Acceptance stores its exact scalar sequence with the accepted input before acknowledgement. A retransmission whose wire spelling decodes to the same text returns the recorded result; changing a line ending, whitespace, or normalization form under that identifier is conflicting reuse. Empty text or U+0000 never constructs the command and claims no identifier.
- **S03:** Restart reconstitutes the exact immutable text from the accepted-input record without trimming or normalization. A queued turn still has no origin semantic entry until eligibility; activation then creates the ADR-0036 reference to that accepted input rather than a second content copy.

## Open questions

- Rich text, attachments, images, audio, files, external resource references, and structured non-text user commands remain unconstructible until their owning decisions define typed boundaries.
- Concrete resource-size, quota, abuse, and admission limits remain resource-governance scope and must preserve the replay and equality boundary above.
- Provider-prompt rendering, tokenization, client transcript rendering, search normalization, moderation projections, and accessibility presentation remain separate decisions.
- Exact Protobuf field spelling and generated client API shape land with the first ADR-0019 protocol fixture slice.

## Explicit non-decisions

This record adds no Rust type, table, migration, protocol schema, fixture, dependency, renderer, prompt template, tokenizer, search index, moderation policy, size limit, attachment mechanism, or client UI. It does not define assistant content, tool content, steering commit granularity, semantic-entry variants beyond ADR-0036, or how a provider consumes the accepted text.
