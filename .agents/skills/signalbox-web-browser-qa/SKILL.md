______________________________________________________________________

## name: signalbox-web-browser-qa description: Verify Signalbox web changes with deterministic scenarios, Playwright, accessibility-first locators, structured diagnostics, screenshots, and failure traces.

# Signalbox web browser QA

Use this skill for browser-driven development, regression tests, visual review,
accessibility checks, and autonomous debugging.

## Deterministic first

Every substantial state has a stable development scenario. Scenarios generate
large data rather than committing giant fixture files and run the real client
reducers, decoders, selectors, commands, and renderers through a scenario
transport.

Prefer scenarios such as:

- idle, streaming, queued, approval, failure, ambiguity, and resync;
- full, condensed, results, and independent expansion;
- blocked goals, delegation, and runner loss;
- 100k timeline items and million-scale indexes;
- webhook bursts, convergence, held slots, and queued obligations;
- huge images, source, logs, JSON, CSV, PDF, audio/video, and unknown files; and
- desktop, workbench, focus, dark, and phone layouts.

## Playwright workflow

1. Open the narrowest deterministic scenario reproducing the behavior.
2. Inspect semantic roles, names, text, focus, console, page errors, network,
   and the Signalbox diagnostic summary.
3. Reproduce with user-facing locators before using test IDs.
4. Make the smallest owning-layer correction.
5. Run the focused test, then the relevant scenario group.
6. Capture trace and screenshots for failures or meaningful visual changes.

## Locators

Prefer `getByRole`, accessible names, labels, and user-visible text. Use stable
`data-testid` values for exact domain identities or virtualized rows when
semantic selection cannot distinguish the target. Do not select generated CSS
classes or DOM depth.

## Diagnostics

The development client exposes bounded, read-only summaries for:

- transport and synchronization phase;
- selected session and durable cursor;
- pending commands, approvals, and provider drafts;
- recent Redux actions and state diffs;
- loaded pages/windows and virtual ranges;
- TanStack Query state; and
- active scenario and fixture parameters.

A diagnostic endpoint returns summaries rather than arbitrary mutable internals
or complete large content.

## Failure artifacts

On failure retain:

- Playwright trace;
- screenshot;
- console messages and page errors;
- accessibility/semantic snapshot where useful;
- relevant network evidence;
- bounded Signalbox diagnostic dump; and
- recent bounded Redux actions.

CI messages link to or name these artifacts rather than reporting only a missing
selector.

## Visual authority

Use one pinned Chromium environment for pixel goldens. Run major functional and
accessibility paths on Chromium, Firefox, and WebKit. Review screenshots for
information hierarchy and interaction state, not only pixel drift.
