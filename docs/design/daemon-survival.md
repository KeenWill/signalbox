# Daemon survival design

This design is not built; it extends
[turn lifecycle and scheduling](../spec/turn-lifecycle-and-scheduling.md),
[session lifecycle](../spec/session-lifecycle.md),
[model-call execution](../spec/model-call-execution.md),
[tool loop](../spec/tool-loop.md),
[persistence protocol](../spec/persistence-protocol.md),
[process protocol](../spec/process-protocol.md), and
[configuration and credentials](../spec/configuration-and-credentials.md).

## Goal

One session's execution or reconstitution failure leaves other sessions running.
Database guard loss pauses work and reacquires authority within the configured
bound. An orphan socket identity pin cannot prevent boot.

## Design

A fatal execution classification means that the affected session cannot safely
continue from its current evidence. The supervisor suspends that session,
records its typed cause durably, and parks it for the operator. Retained
execution evidence, ambiguous commits, unwinds, and unexpected cancellation
never authorize replay. Other sessions remain eligible. A failed park write
retains the local suspension and reports the persistence failure; database guard
recovery reconstitutes durable evidence before scheduling resumes.

The operator queue exposes the session and cause. Operator resume reconciles
retained work before making the session eligible; a failed reconstitution keeps
the item open. Resume never fabricates execution or non-execution evidence.
Fatal parking also covers unmonitored sessions without changing ownership.

Guard loss cancels admission and the affected runtime incarnation, closes its
fenced pool, and releases its dedicated guard connection. The process remains
alive and retries singleton acquisition with exponential backoff bounded by
configured initial and maximum delays. A required configured elapsed recovery
bound admits `none` for unlimited recovery. Expiry stops the process with the
typed recovery-exhausted reason. The elapsed bound includes pool shutdown,
acquisition, and fence establishment. A successful acquisition advances the
generation and reconstructs runtime services and session recovery before
admission resumes. A contending daemon never shares authority.

Startup handles a session-scoped reconstitution failure by retaining a durable
operator item identifying the session and typed cause, excluding that session
from scheduling, and continuing the inventory. It does not repair the row.
Inventory-wide infrastructure failures remain startup failures. An operator can
retry reconstitution after correcting the underlying data; only successful
reconstitution clears the exclusion.

Socket cleanup holds the exclusive path lock, verifies the pin is an owned
socket inode, and probes the pin for a live listener before unlinking it.
Connection refusal proves the inode has no listener. A successful connection or
inconclusive probe preserves the pin. Cleanup revalidates the inode before
unlinking; a present public socket must match it. An absent public socket is
permitted, preserving the existing orphan-pin boot path.

The model-call contract's fatal-signal/process-exit paragraph and the tool-loop
contract's supervisor-stops-scheduling sentence become session-scoped parking.
The scheduling contract's corruption-stops-startup sentence becomes per-session
exclusion, and its guard-loss/process-exit paragraph becomes bounded recovery.
The session contract admits fatal parking for either ownership state. The
persistence contract admits successive fenced incarnations in one process; the
process contract requires listener absence before stale-pin removal.

## Compatibility constraints

The database singleton and generation fence remain mandatory. No old pool is
reused after guard loss. Recovery preserves at-most-once external execution and
durable evidence; a restart or operator resume supplies no execution proof.
Socket ownership, path locking, and inode revalidation remain mandatory.

Not built: multi-daemon operation, automatic repair of corrupt rows, or retry
policies beyond guard backoff.

## Acceptance criteria

A session with fatal execution evidence parks with its cause while another
session completes a turn. Operator resume reconciles before scheduling it.

A simulated guard stall below the configured elapsed bound resumes scheduling;
one above the bound stops with the typed recovery-exhausted reason. A second
daemon cannot acquire authority while either incarnation owns it.

An orphan identity pin with no listener permits boot; a live listener reachable
through the pin prevents cleanup.

A corrupt session is skipped with a durable operator item while other sessions
reconstitute. Repeated boot does not erase the item or admit the corrupt row.
