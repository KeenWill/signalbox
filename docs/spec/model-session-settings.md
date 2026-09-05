# Model and session settings

Model and session settings resolve a session's reasoning level, fast mode, and
service tier from layered overrides, check them against the selected model's
declared capabilities, and record the result before any provider traffic.

## Overview

The subsystem owns three provider-neutral settings: a reasoning level, a
fast-mode switch, and a provider-tagged service tier. They travel with the
generation controls that [runtime-substrate.md](runtime-substrate.md) owns. The
vocabulary, precedence chain, and capability record live in
`crates/domain/src/model_settings.rs`: `ModelSettingsPrecedence` resolves a
value, and `ModelCapabilities` decides whether the selected model supports it.

Settings resolve highest to lowest: per-call override, session override, the
selected model's named settings profile, then the deployment global default.
Each nullable override is inherit, clear, or set(value): inherit consults the
next layer, clear selects the provider default, and set requests the value. A
fast-mode override admits only inherit or set(value). The profile catalog and
the global default are daemon configuration, described on
[configuration-and-credentials.md](configuration-and-credentials.md). The three
lower layers resolve when a session defaults epoch is installed; the per-call
layer resolves at input acceptance, under the turn-binding rule on
[sessions-and-transcript.md](sessions-and-transcript.md).

A model's capability record lists the reasoning levels and service tiers it
supports, and how it supports fast mode: unsupported, a request control on the
selected target, or a declared alternate serving target; a model that declares
no fast-mode support is unsupported. Input acceptance needs the exact record for
its target whenever the per-call overlay has a member other than inherit,
including clear, or the stored settings were validated for a different model;
provider defaults carry no validation selection, so an all-inherit overlay over
them needs none even when the selection changes. A model change carries the
inherited settings to the new model, adjusts those the new model does not
support, and records each adjustment.

Two durable events record settings outcomes: `SessionModelSettingsChanged` when
a defaults replacement changes a setting or model, and
`TurnModelSettingsResolved` when an accepted input produces an origin turn.
[persistence-protocol.md](persistence-protocol.md) owns their storage;
[process-protocol.md](process-protocol.md) owns their wire shape, the
capability-catalog read, and the settings overlay on session commands. Each
provider adapter translates the resolved settings onto its request fields during
preparation; the tables live in the adapter crates, and the advisory exceptions
for controls a CLI cannot enforce are on
[runtime-substrate.md](runtime-substrate.md).

## Design decisions

Incompatibility caused by a model change never rejects the model change, because
model choice outranks setting compatibility.

An unsupported service tier clears to the provider default instead of clamping
to a nearby tier, because tiers have no cross-provider ordering.

Alias retargeting is a model change for the adjustment rule, so input acceptance
repeats capability resolution and records any adjustment with the origin turn.
Why: an alias can acquire a different definition between inputs without a
defaults replacement.

Explicit per-call changes and model-change adjustments are recorded as durable
events, so neither is left as process-local preparation state.

Sampling controls beyond the output-token ceiling, temperature, top-p, and stop
sequences are outside this subsystem.

Claude Code's ultracode is workflow orchestration, not a model reasoning level,
and is not in the settings vocabulary.

Signalbox does not infer capability support from model-name prefixes and does
not run a provider CLI during request preparation.

Codex CLI context-window overrides are keyed by exact provider-model name. The
daemon accepts an override only for a configured Codex CLI model, and its
configured context-window limit remains the usable total context ceiling used
for accounting.

## Boundary contracts

A reasoning level of none is an explicit provider value, distinct from an absent
reasoning setting, which selects the provider default. Every adapter maps
absence to an omitted field and maps none to a provider value or refuses it as
unsupported; the Anthropic and Claude Code adapters refuse it.

Fast mode is not a model name and is independent of service tier, even where an
adapter translates both controls onto one provider field.

A service tier tagged for another adapter is unsupported; no adapter passes it
through as a string.

Override provenance is part of command equality. A copied value is not
interchangeable with the same explicitly set value.

Profile and global layers are resolved and copied when a session defaults epoch
is installed, and a restart or a configuration edit never rewrites an existing
epoch, so an operator file edit cannot silently change an acknowledged session.
A per-call override is resolved at input acceptance, and the complete result is
frozen into the origin turn.

Durable selection provenance retains the requested direct target even when fast
mode serves from a mapped target.

Compatibility is decided before credential access, file creation, subprocess
spawn, or HTTP traffic. An adapter never relies on provider rejection, provider
clamping, an open CLI enum, or silent field dropping. An adapter-specific
unsupported combination is also a preparation-time error.

An incompatibility counts as model-change-induced, and is adjusted instead of
rejected, only when the affected setting is inherited.

Reasoning-level mappings are exhaustive per-adapter tables, evaluated during
preparation, in which every adapter answers every level with a provider value or
a typed refusal.

## Planned

A Swift settings user interface that derives every offered value from the
daemon's per-model capability record and preserves explicit-versus-inherited
override provenance:
[model-session-settings design](../design/model-session-settings.md).
