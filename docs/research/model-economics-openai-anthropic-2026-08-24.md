# OpenAI and Anthropic model-economics evidence audit

> Dated research record (2026-08-24), non-normative. The machine-readable
> catalog in
> [`crates/model-reference-catalog`](../../crates/model-reference-catalog/) is
> the implemented artifact. Runtime routing remains owned by the existing
> configuration and model-runtime surfaces; this page grants no routing
> authority.

- Date verified: 2026-08-24
- Providers: OpenAI and Anthropic only
- Commercial surfaces: first-party API and batch prices; ChatGPT, Codex, Codex
  CLI, Claude, and Claude Code identity mappings
- Starting boundary: OpenAI's March 2023 ChatGPT API era and Anthropic's Claude
  3 generation. Earlier provider history was not admitted without readily
  recoverable first-party evidence.
- Outputs: canonical
  [`reference-catalog.json`](../../crates/model-reference-catalog/data/reference-catalog.json)
  and generated
  [inspection tables](../../crates/model-reference-catalog/projections/)

## Method and admission rule

Every admitted fact was checked against a public first-party provider page,
provider documentation, provider announcement, or the provider's public source
repository. A current mutable price page establishes a rate on its retrieval
date; it does not by itself backdate that rate. Announcement language such as
“next week” becomes an observation boundary, not a guessed effective day.
Archived numeric combinations that rely on a launch date plus a later retained
provider multiplier are marked medium-confidence and carry the limitation on the
rate record.

Each source has a stable ID, retrieval date, and a short statement of the fact
for which it was used. Rate and mapping records cite those IDs. The generated
[source ledger](../../crates/model-reference-catalog/projections/sources.md)
lists the public evidence, while the generated
[research-gap table](../../crates/model-reference-catalog/projections/research-gaps.md)
keeps unknowns explicit.

The catalog stores the provider's original amount and unit and an exact decimal
USD-per-million-token normalization only when the original fee is comparable.
Input, output, cache read, cache write, cache TTL, batch, context band, and
service tier remain distinct. It does not flatten an unlike operation fee into a
token rate.

## Coverage and important boundaries

The OpenAI subset records reference identities for GPT-3.5 Turbo; GPT-4 and
GPT-4 32k; GPT-4 Turbo; GPT-4o and GPT-4o mini; o1, o3, and o4-mini; GPT-4.1;
Codex-related identities; and the admitted GPT-5 through GPT-5.6 families. Rates
begin with the March 2023 GPT-3.5/GPT-4 launches. Precise changes include the
June and November 2023 rates, GPT-4o prompt caching, the June 2025 o3 reduction,
GPT-4.1 batch pricing, GPT-5.4 context/service tiers, and the admitted 2026
GPT-5.6 changes. A known model without defensible API pricing remains a known
identity with an unknown price.

The Anthropic subset records Claude 3 Haiku/Sonnet/Opus, Claude 3.5, Claude 3.7,
Claude 4/4.5/4.6/4.7/4.8, and the admitted Claude 5 family. Standard rates begin
with Claude 3 in March 2024. Cache read, five-minute cache write, and one-hour
cache write remain separate; Message Batches and precise later fast modes are
likewise separate. The pre-reduction Claude 3.5 Haiku amount and unrecovered
default-model cutovers are not inferred.

Rolling aliases and consumer labels are not snapshots. Vague labels stop at a
family; dynamic labels can remain unresolved; exact exported provider IDs can
normalize exactly even when no comparable rate exists. The checked-in mapping
table records quality (`exact`, `strong`, `family_only`, `approximate`, or
`unknown`) separately from evidence confidence (`high`, `medium`, or `low`).

## Accounting interpretation

`actual_billing_kind = api_metered` describes directly metered API use.
`actual_billing_kind = subscription` describes subscription-origin use.

> Equivalent API cost means the estimated first-party API cost of the observed
> usage at the contemporaneous applicable published API rate. It is not the
> user's actual subscription charge.

The catalog provides reference data and deterministic dated resolution only; it
neither calculates nor persists an estimate. A later calculation must not invent
usage absent from an export. Known blind spots include hidden system prompts,
consumer-product tools, retries, internal routing, compaction, cached content,
reasoning or other provider work not surfaced as usage, and provider work
performed outside the exported conversation. These omissions can make an
estimate non-comparable even when the identity and published token rate are
known.

## Separation from execution

The reference catalog is a leaf crate with no dependency on the domain, runtime,
provider-runtime, application, daemon, or persistence crates. The daemon does
not depend on it. Its tests enforce those dependency absences. Consequently,
adding a reference identity cannot make it available to model selection, adapter
validation, or invocation. This slice does not change native model-call billing
or conversation-import behavior.
