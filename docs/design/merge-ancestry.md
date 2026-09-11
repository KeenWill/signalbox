# Merge ancestry verification

## Decision

Configured merge pushes verify parent ancestry; required validation on the pull request merge ref determines whether the combined tree is acceptable.

## Shape

Two-parent merges retain exactly one parent containing the dispatch fence, in either parent order. File contents do not affect admission.

## Transitions

A proven merge proceeds to the existing snapshot and configured transport.

## Compatibility constraints

Repository authority, push destination and snapshot checks remain as specified in [tool-loop](../spec/tool-loop.md).

## Acceptance criteria

Conflict resolutions may rewrite production lines and delete obsolete fixtures. Missing or ambiguous fence bindings remain refused.
