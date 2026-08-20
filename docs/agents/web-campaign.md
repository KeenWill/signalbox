# Signalbox web campaign

> **Non-authoritative planning scratchpad — do not review for consistency.**
> This file decides nothing and is not a statement of record. The approved
> direction and campaign outcome live in GitHub issue #988 and its child issues.
> Cross-crate, persistence, and wire behavior belongs in the implementing
> stack's owning living specification. This file exists only to make launch
> order, collisions, and evidence expectations easy for coordinating agents.

The campaign builds a comprehensive Signalbox web application as a set of
bounded Goal-mode tracks rather than one open-ended run.

## Track map

| Track                     | Issue | Starts when                             | Primary ownership                       |
| ------------------------- | ----- | --------------------------------------- | --------------------------------------- |
| Client platform           | #989  | immediately                             | `clients/web/**`, web CI and evidence   |
| HTTP contract             | #990  | immediately                             | browser contract and DTO surface        |
| Session projection        | #991  | #990 contract slice exists              | session windows, detail, live follow    |
| Fleet and repo watch      | #992  | immediately where current stacks permit | monitor and operator read models        |
| Blobs and derivations     | #993  | #990 plus blob stack seams              | range delivery, capabilities, previews  |
| Search and usage          | #994  | stable timeline addressing exists       | lexical search and accounting reads     |
| Imports, reviews, runners | #995  | #990 plus owning server stacks          | bounded discovery and inspection        |
| Product integration       | #996  | parent slices are usable                | routes, workflows, production adapters  |
| Dogfood and hardening     | #997  | #996 is integrated                      | profiling, real deployment, final proof |

## Dependency shape

```text
#989 client platform ───────────────────────────────────────┐
                                                          │
#990 HTTP contract ──┬─ #991 session projection ───────────┤
                     ├─ #993 blobs/derivations ────────────┤
                     └─ #995 imports/reviews/runners ──────┤
#991 timeline addresses ──────── #994 search/usage ─────────┤
#992 monitor/repo watch ───────────────────────────────────┤
                                                          ▼
                                                    #996 integration
                                                          │
                                                          ▼
                                                    #997 dogfood
```

Track execution and shared-coordinator ownership follow
[Goal mode's execution rules](goal-mode.md#executing). This map records only
campaign-specific track surfaces and dependency collisions.

## Checkpoints

Progress checkpoints follow
[Goal mode's execution rules](goal-mode.md#executing); this map adds only the
active campaign track and current stack head. Milestone completion, finalized
review-wave histories, and post-delivery alignment follow
[Goal mode's finishing rules](goal-mode.md#finishing). Next-milestone selection
follows the [target-model priority order](../target-model.md#priority-order) and
accepted blocking decisions.

## Evidence baseline

The
[signalbox-web-browser-qa skill](../../.agents/skills/signalbox-web-browser-qa/SKILL.md)
owns browser scenarios, coverage, target browsers, diagnostics, and retained
failure artifacts. The
[signalbox-web-performance skill](../../.agents/skills/signalbox-web-performance/SKILL.md)
owns scale proof. This planning map adds no evidence contract.

## Operator surface

Issue #992 carries the operator-surface questions. Any resulting product or
daemon-facts requirement belongs to the implementing stack's owning living
specification; this planning map adds none.

## Review boundary

This planning file is not a wire contract, persistence contract, API schema, or
visual specification. Review it only for a usable dependency map and faithful
links to the approved issues. Exact behavior is reviewed with the code that
implements it.
