# Model and session settings

This page specifies the cross-crate contract for model reasoning, speed, and
service-tier settings; the per-model capability catalog; durable settings
provenance and adjustment events; provider-adapter translation; and the local
process representation.

Sampling controls outside the existing output-token, temperature, top-p, and
stop-sequence fields are not part of this contract. Context-window and
compaction controls are outside this contract. Tool, Git, web, repository-watch,
delegation, and Swift user-interface implementation are also outside it.

## Setting vocabulary

`ReasoningLevel` is one ordered provider-neutral enum. In ascending order it is
`none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, and `ultra`. `none`
is an explicit provider value and is therefore distinct from an absent reasoning
setting, which selects the provider default. `ultra` is the Codex model effort
value. Claude Code's `ultracode` is workflow orchestration rather than a model
reasoning level and has no representation here.

`FastMode` is the closed `disabled` or `enabled` request/session setting. It is
not a model name and is independent of service tier even where an adapter must
translate both controls onto one provider field.

Service-tier values are a provider-tagged union because the provider enums have
different meanings:

- Anthropic: `auto`, `standard_only`;
- OpenAI: `auto`, `default`, `flex`, `scale`, `priority`, `fast`; and
- Codex CLI: `default`, `priority`, `flex`.

Claude Code has no service-tier value. A value tagged for another adapter is
unsupported rather than a string that can be passed through.

`ModelSettings` is the complete effective value: an optional reasoning level,
one fast-mode value, and an optional provider-tagged service tier, together with
the existing generation controls at the Layer-1 runtime boundary. The default is
absent reasoning, disabled fast mode, and absent service tier.

## Layers and immutable provenance

Settings resolve in this fixed highest-to-lowest order:

1. per-call override;
2. session override;
3. the selected model's named settings profile; and
4. the deployment global default.

Absence after all four layers selects the provider default. Each nullable
override is `inherit`, `clear`, or `set(value)`: `inherit` consults the next
layer, `clear` explicitly selects the provider default, and `set` requests the
value. Fast mode uses `inherit` or `set(disabled|enabled)` because its provider
default is represented by `disabled`. This provenance is part of command
equality; a copied value is not interchangeable with the same explicitly set
value.

The daemon configuration owns the global layer and named profile catalog. Each
direct model definition names at most one profile. Profile and global layers are
resolved and copied when a session defaults epoch is installed; a restart or
configuration edit never rewrites an existing epoch. The epoch stores its
session override, the copied lower layers, and the complete result so later
resolution is independent of mutable deployment files. A per-call override is
resolved at input acceptance and the complete result is frozen into the origin
turn. Steering inherits its source turn and cannot carry an override.

Why: copying every lower layer makes all effective changes durable and prevents
an operator file edit from silently changing an acknowledged session.

## Per-model capability catalog

Every direct model definition carries one first-class capability record:

- the exact set of supported `ReasoningLevel` values;
- fast mode as unsupported, supported on the selected target, or supported by
  one declared serving-target mapping; and
- the exact set of supported provider-tagged service-tier values.

An empty reasoning set means the model exposes no reasoning setting. An empty
service-tier set means every explicit service-tier value is unsupported. Sets
are duplicate-free and use domain order. A mapped fast target is distinct from
the selected target, is internal authorization evidence, and is not another
client-selectable model; same-target request controls use the same-target
capability variant.

The catalog is declared per configured model. Signalbox does not infer support
from model-name prefixes and does not run a provider CLI during request
preparation. The local process catalog projects the reasoning set, a single
supported-or-not fast flag, and service-tier set. Aliases join to the record of
the direct selection frozen for the operation.

Where fast mode uses another serving identity, the adapter's capability record
must name that exact target. Preparation uses it as the effective provider
target and consumes the toggle without also emitting that provider's same-target
fast request control, while durable selection provenance retains the requested
direct target. Lineage verification compares provider evidence with the declared
effective target; an undeclared suffix or substitute remains a mismatch
(INV-014).

## Compatibility and adjustments

Compatibility is decided before credential access, file creation, subprocess
spawn, or HTTP traffic.

An explicitly set reasoning level, enabled fast mode, or service tier absent
from the selected model's capability record is a typed invalid request. The
adapter never relies on provider rejection, provider clamping, an open CLI enum,
or silent field dropping. An adapter-specific unsupported combination is also a
preparation-time error.

Incompatibility caused by a model change never rejects the model change. The
source of each value decides the treatment:

- reasoning clamps to the highest supported level at or below the prior level;
  when none lies below it, it clamps to the lowest supported level;
- when the new model exposes no reasoning setting, reasoning clears to the
  provider default;
- enabled fast mode becomes disabled when unsupported; and
- an unsupported service tier clears to the provider default because tiers have
  no cross-provider ordering.

Each adjustment is recorded. Reasoning never clamps upward when any supported
lower value exists. A caller that explicitly sets the same incompatible value in
the model-change command receives the ordinary unsupported-value error;
`inherit` is what identifies incompatibility as model-change-induced.

The installed snapshot writes an automatically adjusted value back at the
inherited source layer that supplied it. Its precedence chain therefore resolves
to its recorded effective value without consulting adjustment history. The event
retains the prior snapshot, caller overlay, and adjustment list, so this
normalization does not erase either the caller's provenance or the reason the
installed inherited contribution differs from its predecessor.

Alias retargeting is a model change for this rule. Because an alias can acquire
a different immutable definition between inputs without a defaults replacement,
input acceptance repeats capability resolution and records any automatic
adjustment with that origin turn.

## Durable events

Every session defaults epoch stores the complete settings snapshot and its layer
provenance. Creation records version one. A successful defaults replacement that
changes a setting or model appends one `SessionModelSettingsChanged` event
carrying the prior and installed defaults versions, prior and installed model
selections, prior and installed settings, the caller override, and the ordered
automatic-adjustment list. Defaults snapshots never contain a per-call layer.
The event and new current pointer commit atomically.

Every accepted origin records one `TurnModelSettingsResolved` event carrying the
accepted-input and turn identities, defaults version, frozen direct selection,
per-call override, complete effective settings, and ordered automatic
adjustments. When adjustments are present, it also carries the distinct prior
direct validation identity whose model change caused them. It is committed with
input acceptance. This records explicit per-call changes and model-change
adjustment rather than leaving either as process-local preparation state.

Adjustment variants are closed: `reasoning_level_clamped { from, to }`,
`reasoning_level_cleared { from }`, `fast_mode_disabled`, and
`service_tier_cleared { from }`. Stored and wire representations use distinct
types but preserve every field. Equal durable-command replay returns the first
recorded result and events; conflicting override provenance is conflicting reuse
(INV-012). Automatic adjustments are server-derived evidence, not caller
payload: they do not participate in command comparison. A first application
stores them with the event, while an equal replay returns that recorded evidence
instead of deriving it again from the current capability catalog.

## Adapter translation

Mappings are exhaustive tables evaluated during preparation:

| Domain level | Anthropic   | OpenAI Chat Completions | Codex CLI | Claude Code CLI |
| ------------ | ----------- | ----------------------- | --------- | --------------- |
| `none`       | unsupported | `none`                  | `none`    | unsupported     |
| `minimal`    | unsupported | `minimal`               | `minimal` | unsupported     |
| `low`        | `low`       | `low`                   | `low`     | `low`           |
| `medium`     | `medium`    | `medium`                | `medium`  | `medium`        |
| `high`       | `high`      | `high`                  | `high`    | `high`          |
| `xhigh`      | `xhigh`     | `xhigh`                 | `xhigh`   | `xhigh`         |
| `max`        | `max`       | `max`                   | `max`     | `max`           |
| `ultra`      | unsupported | unsupported             | `ultra`   | unsupported     |

Anthropic sends effort as `output_config.effort`, fast mode as `speed: "fast"`
with the required public beta header, and its exact service-tier spelling. Fast
mode defaults an absent tier to `standard_only`; explicit `auto` with fast mode
is rejected because it can select incompatible Priority capacity. Its sampling
controls and tool-choice translation are owned by
[runtime substrate](runtime-substrate.md#direct-http-adapters).

OpenAI Chat Completions sends `reasoning_effort` and `service_tier` at the top
level. Fast mode maps an absent tier to `fast`; an explicit `fast` tier agrees,
and every other simultaneous explicit tier conflicts.

Codex CLI passes the closed effort string through a fixed `--config` argument.
It enables the `fast_mode` feature whenever it passes a tier, preventing the CLI
from silently dropping the field. Fast disabled admits `default` or `flex`; fast
enabled admits `priority`; every conflicting combination rejects before spawn.
The legacy tier spelling `fast` is never emitted. Output-token ceiling,
temperature, top-p, and stop sequences remain advisory because this CLI exposes
no enforcing controls for them.

Claude Code passes `--effort`, writes the fast Boolean into its private
per-operation settings document, and supplies `CLAUDE_CODE_MAX_OUTPUT_TOKENS`
through the adapter's cleared-and-allowlisted child environment. Temperature,
top-p, and stop sequences remain advisory. It rejects every service tier.

No adapter logs.

## Local process representation

Protocol version one includes a capability-catalog list request and ordered item
stream, complete settings on creation/defaults reads and receipts, provenance-
preserving overrides on defaults replacement and origin-producing input, typed
unsupported-setting results, and the two durable settings events above. A
transcript snapshot completeness is owned by
[process-protocol](process-protocol.md#transcript-snapshots), and the
legacy-null cutover is owned by
[persistence-protocol](persistence-protocol.md#relational-representation).

The capability projection never exposes a mapped fast serving identity. Client
choices name only the durable direct selection and supported setting values.

**Committed unimplemented functionality (Swift settings UI).** No present Swift
user interface reads this capability catalog, submits session or per-call
settings, or presents automatic adjustments. A future Swift UI must derive every
offered value from the daemon-provided per-model capability record and must
preserve explicit-versus-inherited override provenance. No present Swift screen
provides this functionality.

## Open edges

Compaction threshold, target size, and never-compact/full-context controls are
deferred to the
[session-runtime settings question](../open-questions.md#configuration-categories).
Provider reasoning summaries, thinking display, verbosity, prompt caching,
retention, inference geography, task budgets, and tool-choice subcontrols are
not committed by this contract.
