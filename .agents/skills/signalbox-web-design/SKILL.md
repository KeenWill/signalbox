---
name: signalbox-web-design
description: Design and implement Signalbox web surfaces as dense, professional, keyboard-first expert-workstation interfaces with responsive focus and workbench layouts.
---

# Signalbox web design

Use this skill for layout, visual hierarchy, component composition, responsive
behavior, transcript presentation, data-heavy views, and visual review under
`clients/web/`.

## Start

1. Read the active issue and `clients/web/AGENTS.md`.
2. Open the closest deterministic scenarios before editing components.
3. Identify the primary operator question and the highest-priority action.
4. Decide whether the surface is a focus view, a workbench view, or both.
5. Reuse established tokens, primitives, table patterns, inspectors, and
   commands before adding another visual language.

## Visual character

Signalbox is a professional expert workstation that can become quiet on demand.
Aim for:

- high information density without accidental clutter;
- small but highly legible typography;
- strong alignment and tabular treatment of operational data;
- subtle separators and surface changes rather than nested cards;
- restrained semantic color for status, urgency, selection, and provenance;
- precise hover, active, selected, focus, loading, and disabled states; and
- minimal ornament and no consumer-marketing composition.

Avoid making every event a rounded card. Do not use a giant card around an
expanded turn, then place tool and message cards inside it. Prefer document flow
with a compact turn rail or gutter.

## Layout

Use two deliberate workspace states:

- **Focus** gives the primary conversation or artifact nearly the full window.
- **Workbench** composes navigation, primary content, and optional inspectors or
  live tables in resizable, collapsible panes.

Do not build arbitrary end-user docking or a dashboard builder. Build reusable
pane components and a small set of good code-composed layouts.

On narrow widths, turn navigation into a drawer and inspectors into sheets or
full-page routes. Preserve useful density on phones rather than converting every
row into a tall card.

## Transcript

Treat turns as information groups, not permanent visual boxes.

- Results mode emphasizes user origin and durable final result.
- Condensed mode keeps meaningful progress, tools, warnings, and telemetry in a
  compact structure.
- Full mode exposes all supported detail in document flow.
- Individual expansion never expands unrelated items.
- Inline expert telemetry may show model, tokens, cost, elapsed time, and tool
  count; deep evidence belongs in reusable inspectors.
- Code and structured output receive the width they need.

## Tables and live activity

Prefer one dense table over a grid of summary cards when rows answer the
operator's question. Use compact status markers, aligned numbers, short
identities, exact hover/detail affordances, and keyboard selection. Keep sorting,
filtering, and visible columns discoverable without consuming excessive space.
Follow the [performance
skill](../signalbox-web-performance/SKILL.md) for virtualization requirements.

## Components

Use shadcn/Radix as accessible primitives, not as a mandate to retain demo-page
styling. Compose domain components for Signalbox concepts such as turn, tool,
approval, model call, goal, runner, PR convergence, and artifact capability.

Known domain content gets a typed renderer. Follow the [protocol
skill](../signalbox-web-protocol/SKILL.md) for record validity and generic
fallback eligibility; give a record it admits as generic a safe, useful,
inspectable presentation.

## Visual criteria

For a meaningful screen change, inspect hierarchy, alignment, wrapping,
clipping, focus, hover, and selected states; compare dense and focus modes where
both exist; and fix visual defects before treating functional completeness as
done. The [browser-QA skill](../signalbox-web-browser-qa/SKILL.md) owns scenario,
viewport, screenshot, and browser-evidence requirements.
