# Authentication recovery after profile changes

This document describes committed work that is not built; it extends
[credential availability](../spec/credential-availability.md) and
[configuration and credentials](../spec/configuration-and-credentials.md).

A pool configured to park may retain an active turn when authentication
rejection excludes its last member. Its authentication exclusion can be released
by a successful configuration reload that changes that member's configured Codex
home. An unchanged profile, a process restart, or time passage does not release
it.

Reload admits changes to existing Codex-home delivery paths and retains them in
the durable replacement snapshot. Profile identities, adapters, delivery kinds,
invocation bounds, and pool policies retain their existing reload restrictions.
Replacement validation and runtime construction complete before installation.

Successful installation releases authentication exclusions for already parked
turns naming changed profiles and grants their waits eligibility. The release is
durable and idempotent under reload recovery. Other exclusions still apply. The
existing wait-release path retains each turn's input, frontier, target, policy,
and predecessor failure; it prepares at most one successor and never revives a
terminal turn. Failure evidence remains readable after release.

The Codex adapter retains the latest correlated retry error's typed facts when a
failed terminal notification supplies only the generic `other` classification. A
specific terminal classification takes precedence. Retry telemetry alone does
not establish terminal failure or non-acceptance, and successful completion
discards it. Diagnostic prose never determines authentication or network class.

Acceptance covers authentication parking, unchanged-profile reload, changed-home
wake and successful continuation, reload replay, competing release attempts, and
typed retry diagnostics followed by generic failure or successful completion. No
credential copying, automatic login, new retry setting, or recovery command is
added.
