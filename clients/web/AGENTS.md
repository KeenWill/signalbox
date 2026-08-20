# Signalbox web agent guidance

This file governs all work under `clients/web/`. The repository-wide `AGENTS.md`
remains authoritative. Read GitHub issue #988, the active child issue, and the
relevant skills under `.agents/skills/` before changing this client.

This bootstrap does not decide open browser transport, client language, wire, or
cross-component questions. The implementing stack records foundation-weight
choices in its owning living specification and ordinary choices in its
pull-request description before implementation.

## Architecture

- React renders projections; it does not own transport, protocol recovery, or
  synchronization authority.
- Keep transport, decoding, synchronization, state projection, and presentation
  as distinct modules with distinct types.
- The implementing stack's owning specification decides the browser contract,
  client language, and validation approach. Consume that implemented contract;
  do not hand-maintain a second protocol.
- Components never open network connections directly. They invoke typed client
  services or commands and subscribe through selectors.
- Use Redux Toolkit for explicit control/application transitions and bounded
  diagnostic traces. Do not place an unbounded transcript or artifact corpus in
  one Redux tree.
- Use TanStack Query for ordinary request/response state. Dedicated session and
  fleet synchronization machines own live streams and resynchronization.
- A deterministic scenario adapter and the production adapter selected by the
  owning contract implement the same transport-neutral client seam. Scenarios
  must exercise real reducers, decoders, commands, selectors, and renderers
  rather than duplicate fake UI logic.

## Resource behavior

The
[signalbox-web-performance skill](../../.agents/skills/signalbox-web-performance/SKILL.md)
owns browser resource ceilings, loading and windowing rules, virtualization,
stream batching, and their proof obligations. Follow that skill rather than
restating its limits here.

## Interaction and presentation

- Build a professional expert workstation: dense, precise, restrained, and
  highly legible. Avoid marketing layouts, giant headings, gratuitous cards,
  excessive rounding, decorative gradients, and wasted space.
- Conversation remains the primary surface. Focus mode can become quiet;
  workbench mode may expose dense live tables and inspectors.
- Turns organize data but do not force expanded content into a large nested box.
  Use gutter, rail, typography, and compact boundaries.
- `full`, `condensed`, and `results` are client presentation modes. Individual
  turns and items expand independently.
- Every important action belongs to the central command registry. Menus,
  buttons, hotkeys, and the command palette invoke the same command.
- Modal/Vim-inspired navigation applies outside editing contexts. Text inputs,
  editors, and the composer retain ordinary editing behavior.
- Use semantic native elements and Radix/shadcn primitives. Accessible names,
  focus order, keyboard behavior, and visible focus are implementation
  requirements, not polish.
- Only validated generic records remain visible through a safe generic renderer.
  Reject or quarantine unknown discriminators and contradictory correlations
  instead of rendering them as generic content.

## Browser and agent evidence

Every substantial UI change includes or updates deterministic scenarios and
Playwright coverage. Prefer role/name/user-facing locators; add stable domain
identifiers only where semantics cannot identify one exact object.

Capture pinned Chromium screenshots for visual changes. Major responsive paths
also require mobile evidence. Assert no unexpected console message or page
error. When the active slice implements bounded diagnostics, failure traces
include its allowlisted, redacted snapshot and recent permitted application
actions where relevant.

Visual review checks hierarchy, density, alignment, typography, information
priority, interaction states, light/dark behavior, and responsive composition;
it does not accept a screen merely because all controls are present.

## Dependencies

Issue #988 records the approved initial React, Vite, TanStack, Redux,
shadcn/Radix, Tailwind, Lucide, and Playwright stack. Explain focused dependency
choices in the pull-request description. Ask before adding another large or
architecturally constraining dependency.

## Review

Block on incorrect authority, fabricated server semantics, inaccessible
interaction, unbounded work, missing deterministic evidence, or divergence from
the implemented browser contract. When the owning specification selects a
generated contract, also block divergence from that generated artifact. Do not
require speculative handling of functionality that the active issue explicitly
defers.
