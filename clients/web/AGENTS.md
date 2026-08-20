# Signalbox web agent guidance

This file governs all work under `clients/web/`. The repository-wide
`AGENTS.md` remains authoritative. Read GitHub issue #988, the active child
issue, and the relevant skills under `.agents/skills/` before changing this
client.

## Architecture

- React renders projections; it does not own transport, protocol recovery, or
  synchronization authority.
- Keep transport, decoding, synchronization, state projection, and presentation
  as distinct modules with distinct types.
- Rust owns the browser contract. Consume generated or mechanically checked
  TypeScript types and runtime decoders; do not hand-maintain a second protocol.
- Components never open network connections directly. They invoke typed client
  services or commands and subscribe through selectors.
- Use Redux Toolkit for explicit control/application transitions and bounded
  diagnostic traces. Do not place an unbounded transcript or artifact corpus in
  one Redux tree.
- Use TanStack Query for ordinary request/response state. Dedicated session and
  fleet synchronization machines own live streams and resynchronization.
- A `ScenarioTransport` and production `HttpTransport` implement the same client
  seam. Deterministic scenarios must exercise real reducers, decoders, commands,
  selectors, and renderers rather than duplicate fake UI logic.

## Resource behavior

- No UI operation may require materializing an unbounded collection in browser
  memory.
- Use TanStack Virtual for transcripts and large lists, and TanStack Table plus
  Virtual for large tables, from the first implementation.
- Virtualization does not excuse unbounded fetching, decoding, indexing, state
  retention, syntax highlighting, or DevTools history.
- Consume server windows and stable logical cursors. Never use array offsets as
  durable history addresses.
- Load small histories greedily only through the explicit client resource
  policy. Cancellation or changing network conditions must fall back to the
  same incremental model without changing semantics.
- Batch ephemeral provider display updates. Never drop, reorder, or debounce
  durable Signalbox events.
- Keep Redux DevTools and custom traces bounded and redact or summarize large
  content.

## Interaction and presentation

- Build a professional expert workstation: dense, precise, restrained, and
  highly legible. Avoid marketing layouts, giant headings, gratuitous cards,
  excessive rounding, decorative gradients, and wasted space.
- Conversation remains the primary surface. Focus mode can become quiet;
  workbench mode may expose dense live tables and inspectors.
- Turns organize data but do not force expanded content into a large nested
  box. Use gutter, rail, typography, and compact boundaries.
- `full`, `condensed`, and `results` are client presentation modes. Individual
  turns and items expand independently.
- Every important action belongs to the central command registry. Menus,
  buttons, hotkeys, and the command palette invoke the same command.
- Modal/Vim-inspired navigation applies outside editing contexts. Text inputs,
  editors, and the composer retain ordinary editing behavior.
- Use semantic native elements and Radix/shadcn primitives. Accessible names,
  focus order, keyboard behavior, and visible focus are implementation
  requirements, not polish.
- Unknown typed content remains visible through a safe generic renderer instead
  of disappearing or crashing.

## Browser and agent evidence

Every substantial UI change includes or updates deterministic scenarios and
Playwright coverage. Prefer role/name/user-facing locators; add stable domain
identifiers only where semantics cannot identify one exact object.

Capture pinned Chromium screenshots for visual changes. Major responsive paths
also require mobile evidence. Assert no unexpected console message or page
error. Failure traces must include the bounded Signalbox diagnostic snapshot
and recent application actions where relevant.

Visual review checks hierarchy, density, alignment, typography, information
priority, interaction states, light/dark behavior, and responsive composition;
it does not accept a screen merely because all controls are present.

## Dependencies

Issue #988 records the owner-approved initial React, Vite, TanStack, Redux,
shadcn/Radix, Tailwind, Lucide, and Playwright stack. Explain focused dependency
choices in the pull-request description. Ask before adding another large or
architecturally constraining dependency.

## Review

Block on incorrect authority, fabricated server semantics, inaccessible
interaction, unbounded work, missing deterministic evidence, or divergence from
the generated contract. Do not require speculative handling of functionality
that the active issue explicitly defers.
