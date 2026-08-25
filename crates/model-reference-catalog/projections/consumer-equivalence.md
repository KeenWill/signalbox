<!-- Generated from ../data/reference-catalog.json; do not edit by hand. -->

# Consumer/subscription-to-API equivalence

Equivalent API cost is the estimated first-party API cost of the observed usage
at the contemporaneous applicable published API rate. It is not the user's
actual subscription charge.

```text
| Provider | Channel | Actual billing | Observed identity | Normalized reference | Interval | Quality | Confidence | Sources | Limitations |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Anthropic | claude_subscription | subscription | `Claude 3.5 Sonnet` | `anthropic:claude-3-family` | 2024-06-21..open | family_only | high | `anth-claude35-sonnet-2024-06-21`, `anth-claude35-update-2024-10-22` | The same consumer label covered at least two dated API snapshots, so the mapping stops at the family. |
| Anthropic | claude_subscription | subscription | `Claude 3.7 Sonnet` | `anthropic:claude-3-7-sonnet-20250219` | 2025-02-24..open | strong | high | `anth-claude37-code-2025-02-24` | The consumer/API relationship is direct, but exported tokens can omit hidden product work. |
| Anthropic | claude_subscription | subscription | `Auto` | - | 2024-03-04..open | unknown | high | `anth-current-models-2026-08-24` | A routed product label does not reveal one API identity. |
| Anthropic | claude_subscription | subscription | `Sonnet` | `anthropic:claude-4-family` | 2025-05-22..open | family_only | medium | `anth-claude4-2025-05-22`, `anth-sonnet46-2026-02-17`, `anth-sonnet5-2026-06-30` | Sonnet is a dynamic family label spanning multiple releases and cannot be forced to one snapshot. |
| Anthropic | claude_subscription | subscription | `Claude Sonnet 4` | `anthropic:claude-sonnet-4-20250514` | 2025-05-22..open | strong | high | `anth-claude4-2025-05-22` |  |
| Anthropic | claude_subscription | subscription | `Claude Sonnet 4.5` | `anthropic:claude-sonnet-4-5-20250929` | 2025-09-29..open | strong | high | `anth-claude45-sonnet-2025-09-29` |  |
| Anthropic | claude_code_subscription | subscription | `default` | `anthropic:claude-sonnet-4-5-20250929` | 2025-09-29..2026-06-30 | strong | medium | `anth-claude-code-default-2025-09-29`, `anth-claude-code-changelog-2026-08-24` | Sonnet 4.5 is explicitly documented as the default on September 29, but the catalog does not claim there were no intervening default changes before the next recovered explicit default. |
| Anthropic | claude_code_subscription | subscription | `default` | `anthropic:claude-sonnet-5` | 2026-06-30..open | strong | high | `anth-sonnet5-2026-06-30`, `anth-claude-code-changelog-2026-08-24` | A default mapping applies only when the import proves use of the default rather than an explicit override. |
| Anthropic | claude_code_subscription | subscription | `claude-3-7-sonnet-20250219` | `anthropic:claude-3-7-sonnet-20250219` | 2025-02-24..open | exact | high | `anth-claude37-code-2025-02-24` |  |
| Anthropic | claude_code_subscription | subscription | `claude-sonnet-4-20250514` | `anthropic:claude-sonnet-4-20250514` | 2025-05-22..open | exact | high | `anth-claude4-2025-05-22` |  |
| Anthropic | claude_code_subscription | subscription | `claude-sonnet-4-5-20250929` | `anthropic:claude-sonnet-4-5-20250929` | 2025-09-29..open | exact | high | `anth-claude45-sonnet-2025-09-29`, `anth-claude-code-default-2025-09-29` |  |
| Anthropic | claude_code_subscription | subscription | `opus` | `anthropic:claude-4-family` | 2025-05-22..open | family_only | high | `anth-model-ids-2026-08-24`, `anth-claude-code-changelog-2026-08-24` | The alias follows a moving Opus target and is not a dated model identity. |
| Anthropic | claude_code_subscription | subscription | `sonnet` | `anthropic:claude-4-family` | 2025-05-22..open | family_only | high | `anth-model-ids-2026-08-24` | The alias follows the latest Sonnet and is not a dated model identity. |
| OpenAI | chatgpt_subscription | subscription | `Auto` | - | 2023-03-01..open | unknown | high | `oai-gpt5-2025-08-07` | Auto is dynamically routed and does not identify one API model. |
| OpenAI | chatgpt_subscription | subscription | `GPT-3.5` | `openai:gpt-3.5-family` | 2023-03-01..open | family_only | high | `oai-gpt35-launch-2023-03-01` | The UI label identifies a family, not an API snapshot or rolling-alias target. |
| OpenAI | chatgpt_subscription | subscription | `GPT-4` | `openai:gpt-4-family` | 2023-03-14..open | family_only | high | `oai-gpt4-launch-2023-03-14` | The consumer label does not identify the 8k, 32k, Turbo, or later routed implementation. |
| OpenAI | chatgpt_subscription | subscription | `GPT-4o` | `openai:gpt-4o-family` | 2024-05-13..open | family_only | high | `oai-gpt4o-api-2024-05-13`, `oai-gpt4o-mini-2024-07-18` | A consumer label does not prove which dated API snapshot or internal product variant served a turn. |
| OpenAI | chatgpt_subscription | subscription | `GPT-5` | `openai:gpt-5-chat-latest` | 2025-08-07..open | strong | high | `oai-gpt5-2025-08-07` | gpt-5-chat-latest exposes the ChatGPT model through the API, but the rolling identity does not expose a dated snapshot. |
| OpenAI | chatgpt_subscription | subscription | `GPT-5.2 Thinking` | `openai:gpt-5.2` | 2025-12-11..open | strong | high | `oai-gpt52-2025-12-11` | The provider's identity table maps the consumer label to the API model; it does not make subscription use API-metered billing. |
| OpenAI | chatgpt_subscription | subscription | `o3` | `openai:o3` | 2025-04-16..open | strong | high | `oai-o3-o4-2025-04-16` | The shared product/API identity does not prove that hidden consumer-product work is represented in an export's token counts. |
| OpenAI | chatgpt_subscription | subscription | `o4-mini-high` | `openai:o4-mini` | 2025-04-16..open | approximate | medium | `oai-o3-o4-2025-04-16` | The high consumer effort preset is not an API snapshot and may entail different reasoning-token use; o4-mini is only the closest published API identity. |
| OpenAI | codex_cli_subscription | subscription | `gpt-5` | `openai:gpt-5` | 2025-08-07..open | exact | high | `oai-gpt5-2025-08-07` | The announcement identifies GPT-5 as the Codex CLI default, but defaults do not establish which model served an export that omits its model identity. |
| OpenAI | codex_cli_subscription | subscription | `gpt-5-codex` | `openai:gpt-5-codex` | 2025-09-15..open | exact | high | `oai-gpt5-codex-2025-09-15` | The model's underlying snapshot was documented as regularly updated, so exact spelling is not a dated snapshot identity. |
| OpenAI | codex_cli_subscription | subscription | `gpt-5.1-codex-max` | `openai:gpt-5.1-codex-max` | 2025-11-19..open | exact | high | `oai-gpt51-codex-max-2025-11-19`, `oai-codex-model-catalog-2026-08-24` | API access was explicitly not available at consumer launch; the recorded API price begins only at its later first-party observation. |
| OpenAI | codex_cli_subscription | subscription | `gpt-5.2` | `openai:gpt-5.2` | observed 2026-08-24..open | exact | high | `oai-gpt52-2025-12-11`, `oai-codex-model-catalog-2026-08-24` | The public Codex catalog proves use by the retrieval date, but the GPT-5.2 launch page did not claim a Codex default or release date. |
| OpenAI | codex_cli_subscription | subscription | `gpt-5.3-codex` | `openai:gpt-5.3-codex` | 2026-02-05..open | exact | high | `oai-gpt53-codex-2026-02-05`, `oai-codex-model-catalog-2026-08-24` | The launch proves subscription Codex availability, not a comparable first-party API price. |
| OpenAI | codex_cli_subscription | subscription | `gpt-5.4` | `openai:gpt-5.4` | 2026-03-05..open | exact | high | `oai-gpt54-2026-03-05`, `oai-codex-model-catalog-2026-08-24` |  |
| OpenAI | codex_cli_subscription | subscription | `gpt-5.5` | `openai:gpt-5.5` | 2026-04-24..open | exact | high | `oai-gpt55-2026-04-24`, `oai-codex-model-catalog-2026-08-24` |  |
| OpenAI | codex_cli_subscription | subscription | `gpt-5.6-luna` | `openai:gpt-5.6-luna` | 2026-07-09..open | exact | high | `oai-gpt56-2026-07-09`, `oai-codex-model-catalog-2026-08-24` |  |
| OpenAI | codex_cli_subscription | subscription | `codex-mini-latest` | `openai:codex-mini-latest` | 2025-05-16..open | exact | high | `oai-codex-2025-05-16` | Exact identity does not make subscription use API-metered actual billing. |
| OpenAI | codex_cli_subscription | subscription | `gpt-5.6-sol` | `openai:gpt-5.6-sol` | 2026-07-09..open | exact | high | `oai-gpt56-2026-07-09`, `oai-codex-model-catalog-2026-08-24` |  |
| OpenAI | codex_cli_subscription | subscription | `gpt-5.6-terra` | `openai:gpt-5.6-terra` | 2026-07-09..open | exact | high | `oai-gpt56-2026-07-09`, `oai-codex-model-catalog-2026-08-24` |  |
| OpenAI | codex_subscription | subscription | `codex-1` | `openai:o3` | 2025-05-16..open | approximate | high | `oai-codex-2025-05-16` | OpenAI says codex-1 is based on o3 and optimized for Codex; it is not an API model. o3 is a retrospective analogue, not the user's actual billed model or charge. |
```
