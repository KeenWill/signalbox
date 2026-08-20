# Signalbox web campaign

> **Non-authoritative planning scratchpad — do not review for consistency.**
> This file decides nothing and is not a statement of record. Owner direction
> and the campaign outcome live in GitHub issue #988 and its child issues.
> Cross-crate, persistence, and wire behavior belongs in the implementing
> stack's owning living specification. This file exists only to make launch
> order, collisions, and evidence expectations easy for coordinating agents.

The campaign builds a comprehensive Signalbox web application as a set of
bounded Goal-mode tracks rather than one open-ended run.

## Track map

| Track | Issue | Starts when | Primary ownership |
| --- | --- | --- | --- |
| Client platform | #989 | immediately | `clients/web/**`, web CI and evidence |
| HTTP contract | #990 | immediately | browser transport and generated DTOs |
| Session projection | #991 | #990 contract slice exists | session windows, detail, live follow |
| Fleet and repo watch | #992 | immediately where current stacks permit | monitor and operator read models |
| Blobs and derivations | #993 | #990 plus blob stack seams | range delivery, capabilities, previews |
| Search and usage | #994 | stable timeline addressing exists | lexical search and accounting reads |
| Imports, reviews, runners | #995 | #990 plus owning server stacks | bounded discovery and inspection |
| Product integration | #996 | parent slices are usable | routes, workflows, production adapters |
| Dogfood and hardening | #997 | #996 is integrated | profiling, real deployment, final proof |

## Dependency shape

```text
#989 client platform ───────────────────────────────┐
                                                   │
#990 HTTP contract ──┬─ #991 session projection ───┤
                     ├─ #992 monitor/repo watch ───┤
                     ├─ #993 blobs/derivations ────┤
                     └─ #995 secondary reads ──────┤
                              │                    │
#991 timeline addresses ──────┴─ #994 search/usage│
                                                   ▼
                                             #996 integration
                                                   │
                                                   ▼
                                             #997 dogfood
```

Tracks use separate worktrees and branches. The coordinator owns shared
composition roots, generated indexes, dependency order, merge-forward, dogfood,
and the final integration view. A track does not edit another track's owned
surface merely to unblock itself.

## Checkpoints

A progress checkpoint states:

1. the active track and current stack head;
2. what has been verified with commands or browser artifacts;
3. the next bounded slice;
4. any semantic, dependency, or external blocker; and
5. the review-wave history for pull requests being finished.

UI-facing checkpoints include scenario URLs and review screenshots. Large-data
checkpoints also report mounted-row counts, retained records or memory trends,
request/window sizes, and the bound being proved.

## Evidence baseline

Every substantial screen has deterministic scenarios and Playwright coverage.
The pinned Chromium environment is the visual authority. Major workflows also
run functionally and accessibly in current Firefox and WebKit.

Failure artifacts include:

- a Playwright trace and screenshot;
- browser console messages and page errors;
- the relevant bounded Redux action trace;
- the Signalbox diagnostic summary; and
- network/request evidence when transport is involved.

The agent-facing diagnostic interface is read-only, bounded, and absent from
ordinary production builds unless deliberately enabled.

## Operator surface

Issue #992 owns the product questions currently answered by personal dogfood
queries. The production application must expose explicit daemon facts for
repository ingestion, webhook projection, held and queued dispatch work, PR
convergence, sessions acting on each PR, blocked goals, judge outcomes, and
last-observed/actioned/dispatched/settled events. It must not preserve direct SQL
or inference shortcuts as product semantics.

## Review boundary

This planning file is not a wire contract, persistence contract, API schema, or
visual specification. Review it only for a usable dependency map and faithful
links to the owner-approved issues. Exact behavior is reviewed with the code
that implements it.
