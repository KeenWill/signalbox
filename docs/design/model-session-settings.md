# Model and session settings design

This design is not built. It extends
[model-session-settings.md](../spec/model-session-settings.md) with a Swift
settings user interface.

## Goal

A Swift client screen reads a session's settings, sets session and per-call
overrides, and offers only values the daemon declares as supported for the
selected model.

## Design

The screen reads the capability catalog through the catalog read on
[process-protocol.md](../spec/process-protocol.md). For the selected direct
model it offers the reasoning levels in that record, fast mode only when the
record supports it, and the service tiers in that record. An alias is offered
through the record of the direct selection it currently resolves to. Inherit is
offered for every setting; provider default is offered for reasoning level and
service tier.

The screen submits each member in the overlay form the settings-carrying
commands accept: inherit, clear, or set(value) for a reasoning-level or
service-tier member, and inherit or set(value) for a fast-mode member. It
displays an inherited value distinctly from an explicitly set value, submits
inherit for a member the user did not touch, and never converts an inherited
value into set(value) on the user's behalf.

The screen shows the automatic adjustments carried on the settings events the
daemon records; it derives none itself.

## Compatibility constraints

The capability projection on [process-protocol.md](../spec/process-protocol.md)
remains the only source of offered values, and it continues to name only
client-selectable models. The settings overlay on session commands keeps its
inherit, clear, and set members, so a client can express provenance. No daemon
change adds a capability derived from a model name or a provider probe.

## Acceptance criteria

Every set(value) choice the screen offers for a direct model appears in that
model's daemon-provided capability record; inherit and clear are overlay states,
not capability values. No Swift source carries a reasoning-level, fast-mode, or
service-tier table of its own beyond the protocol types. A submitted overlay
marks untouched members as inherit and chosen members as set or clear, and a
defaults read returns the same provenance. Automatic adjustments shown to the
user are the ones the daemon recorded.
