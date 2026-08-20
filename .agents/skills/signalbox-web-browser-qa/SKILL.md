---
name: signalbox-web-browser-qa
description: Verify Signalbox web changes with deterministic scenarios, the selected browser-test stack, accessibility-first locators, structured diagnostics, screenshots, and failure traces.
---

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

## Browser-test workflow

1. Open the narrowest deterministic scenario reproducing the behavior.
2. Inspect semantic roles, names, text, focus, console, page errors, and network.
   Inspect bounded diagnostics only when the active slice implements them.
3. Reproduce with user-facing locators before using test IDs.
4. Make the smallest owning-layer correction.
5. Run the focused test, then the relevant scenario group.
6. Capture the selected runner's trace when available and screenshots for
   failures or meaningful visual changes.

## Locators

Prefer `getByRole`, accessible names, labels, and user-visible text. Use stable
`data-testid` values for exact domain identities or virtualized rows when
semantic selection cannot distinguish the target. Do not select generated CSS
classes or DOM depth.

## Diagnostics

This bootstrap assumes no diagnostic endpoint. When an implementing slice adds
one through its owning contract, the development client may expose bounded,
read-only summaries for:

- transport and synchronization phase;
- selected session and durable cursor;
- pending commands and approvals;
- provider drafts only when an implemented relay contract also authorizes their
  diagnostic representation and redaction;
- recent Redux actions and state diffs;
- loaded pages/windows and virtual ranges;
- TanStack Query state; and
- active scenario and fixture parameters.

Any diagnostic interface and retained dump uses an allowlisted, redacted schema.
It excludes user content, command payloads, credentials, tokens, provider
drafts without the contract authorization and redaction required above, and
sensitive identifiers rather than exposing arbitrary internals or complete
large content.

## Failure artifacts

On failure retain the artifacts supported by the selected browser-test stack:

- browser-runner trace, such as a Playwright trace when the implementing
  specification selects Playwright;
- screenshot;
- console messages and page errors;
- accessibility/semantic snapshot where useful;
- relevant network evidence;
- bounded, allowlisted Signalbox diagnostic dump when the active slice provides
  one; and
- bounded state-transition evidence for the selected client stack, such as
  recent bounded Redux actions when that stack selects Redux.

Retain diagnostics and failure artifacts only in protected CI or test contexts.
Before retaining any artifact, sanitize cookies, authorization headers,
credentials, sensitive response bodies, and user content, or guarantee that the
scenario uses only synthetic credentials and data approved for retention. Test
that sanitization or synthetic-data guarantee together with either production
exclusion or access protection. CI messages link to or name permitted artifacts
rather than reporting only a missing selector.

## Visual authority

Use one pinned Chromium environment for pixel goldens. Run major functional and
accessibility paths on Chromium, Firefox, and WebKit. Review screenshots for
information hierarchy and interaction state, not only pixel drift.
