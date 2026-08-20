# Signalbox web agent guidance

This file governs all work under `clients/web/`. The repository-wide `AGENTS.md`
remains authoritative. Read GitHub issue #988, the active child issue, and the
relevant skills under `.agents/skills/` before changing this client.

This bootstrap does not decide open browser transport, client language, wire, or
cross-component questions. The implementing stack records foundation-weight
choices in its owning living specification and ordinary choices in its
pull-request description before implementation.

## Architecture

- Keep transport, decoding, synchronization, state projection, and presentation
  as distinct modules with distinct types.
- The implementing stack's owning specification decides the browser contract,
  client language, and validation approach. Consume that implemented contract;
  do not hand-maintain a second protocol.
- Components never open network connections directly. They invoke typed client
  services or commands and subscribe through selectors.
- When the owning specification selects the JavaScript stack recorded in issue
  #988:
  - React renders projections; it does not own transport, protocol recovery, or
    synchronization authority.
  - Redux Toolkit owns explicit control and application transitions.
  - TanStack Query owns ordinary request/response state. Dedicated session and
    fleet synchronization machines own live streams and resynchronization.

## Resource behavior

The
[signalbox-web-performance skill](../../.agents/skills/signalbox-web-performance/SKILL.md)
owns browser resource ceilings, loading and windowing rules, virtualization,
stream batching, and their proof obligations. Follow that skill rather than
restating its limits here.

## Interaction and presentation

[The signalbox-web-design skill](../../.agents/skills/signalbox-web-design/SKILL.md)
owns layout, visual character, responsive composition, transcript presentation,
and component composition.
[The signalbox-web-keyboard skill](../../.agents/skills/signalbox-web-keyboard/SKILL.md)
owns commands, modal interaction, focus, keyboard behavior, and interaction
accessibility.

## Browser and agent evidence

[The signalbox-web-browser-qa skill](../../.agents/skills/signalbox-web-browser-qa/SKILL.md)
owns deterministic browser scenarios, Playwright workflow and locators,
screenshots, visual review, diagnostics, and retained failure evidence. Follow
that skill rather than restating its evidence contract here.

## Dependencies

If the implementing specification selects the JavaScript client, issue #988
records its initial React, Vite, TanStack, Redux, shadcn/Radix, Tailwind,
Lucide, and Playwright stack. Otherwise follow the selected client-language
stack. Explain focused dependency choices in the pull-request description. Ask
before adding another large or architecturally constraining dependency.

## Review

Block on incorrect authority, fabricated server semantics, inaccessible
interaction, unbounded work, missing deterministic evidence, or divergence from
the implemented browser contract. When the owning specification selects a
generated contract, also block divergence from that generated artifact. Do not
require speculative handling of functionality that the active issue explicitly
defers.
