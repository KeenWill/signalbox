# Codex CLI subscription wire protocol

> Dated research intake (2026-07-23), non-normative. This page records study
> findings as input to future adapter work; it states no requirements. Decisions
> live in the [decision ledger](../decisions.md); current requirements live in
> the [living specification](../spec/README.md), which supersedes anything
> stated here.

- Date: 2026-07-23
- Status: research intake, proposal-grade input only; describes the publicly
  observable behavior of a third-party client, not a contract
- Audited snapshot: the open-source
  [`openai/codex`](https://github.com/openai/codex) repository (shallow clone at
  HEAD `fb4e6ba2`), Apache-2.0; everything below was read from that source tree
  — no API was called and no credential was touched
- Scope: the wire protocol the open-source Codex CLI speaks against the
  ChatGPT-subscription backend — endpoint and base-URL selection, the OAuth/PKCE
  token lifecycle, token storage and refresh, the SSE event taxonomy, notable
  headers, rate-limit and error signals, and a drift-risk assessment
- Intended use: input to the "Codex-subscription Rust reimplementation" track in
  the [backlog](../agents/backlog.md) — which remains blocked on an owner
  ToS-cost decision; pairs with the
  [ModelRuntime adapter conformance template](runtime-adapter-conformance.md)

File references below are `codex-rs/<crate>/path:line` inside the audited
snapshot; the upstream tree moves daily, so treat line numbers as hints.

## 1. License and source layout

- **License: Apache-2.0** (`LICENSE`, copyright line "Copyright 2025 OpenAI"),
  with a `NOTICE` file; standard Apache-2.0 boilerplate, no MIT dual-license.
  This is permissive: specific crates may legally be read, adapted, or vendored
  provided attribution/NOTICE is preserved — which materially changes the
  build-vs-borrow economics for the OAuth/refresh logic and the Responses SSE
  parser (see §9).
- **Language: overwhelmingly Rust.** The real client is a large Cargo workspace
  at `codex-rs/` (~100+ member crates). The `codex-cli/` (npm) and `sdk/`
  (TypeScript) trees are thin wrappers/distribution; the protocol logic is all
  Rust.
- **Crates that matter for a reimplementation** (all under `codex-rs/`):
  - `login/` (`codex-login`) — the entire
    OAuth/PKCE/device-code/refresh/token-storage surface.
  - `codex-api/` — the Responses wire client: request/response types, SSE
    parsing, rate-limit header parsing, error taxonomy, plus a
    Responses-over-WebSocket transport.
  - `model-provider-info/` — provider/base-URL/wire-API selection and the
    subscription base-URL constant.
  - `model-provider/` — `AuthProvider` implementations that stamp the bearer and
    account headers.
  - `core/src/client.rs` + `core/src/client_common.rs` — per-turn session
    orchestration and header assembly.
  - Key third-party versions (`codex-rs/Cargo.lock`): `reqwest` 0.12.28 (0.13.4
    also present), `tokio` 1.52.3, `tokio-tungstenite` 0.28,
    `eventsource-stream` 0.2.3.

## 2. Auth: OAuth 2.0 authorization code + PKCE

**Mechanism: OAuth 2.0 Authorization Code + PKCE (S256), via a loopback
redirect.** A device-code flow is also supported for headless machines.

- **Client ID (hardcoded public app registration):**
  `app_EMoamEEZ73f0CkXaXp7hrann` — `login/src/auth/manager.rs:1448`
  (`pub const CLIENT_ID`; env override `CODEX_APP_SERVER_LOGIN_CLIENT_ID`).
- **Issuer (hardcoded):** `https://auth.openai.com` (`login/src/server.rs:57`).
- **Loopback server:** port **1455** (fallback 1457), redirect
  `http://localhost:1455/auth/callback`. The callback server is `tiny_http`,
  opens the browser via `webbrowser`, validates `state`, and handles
  `/auth/callback`, `/success`, `/cancel`.
- **PKCE** (`login/src/pkce.rs`): 64 random bytes → base64url-no-pad
  `code_verifier`; `code_challenge` = base64url-no-pad(SHA-256(verifier));
  `code_challenge_method=S256`.
- **Authorize request** (`build_authorize_url`, `login/src/server.rs`):
  `GET {issuer}/oauth/authorize` with `response_type=code`, `client_id`,
  `redirect_uri`,
  `scope="openid profile email offline_access api.connectors.read api.connectors.invoke"`,
  `code_challenge` + `code_challenge_method=S256`,
  `id_token_add_organizations=true`, `codex_cli_simplified_flow=true`, `state`
  (32 random bytes, base64url), and `originator`.
- **Token exchange** (`exchange_code_for_tokens`, `login/src/server.rs`):
  `POST {issuer}/oauth/token`,
  `Content-Type: application/x-www-form-urlencoded`, body
  `grant_type=authorization_code&code&redirect_uri&client_id&code_verifier`.
  Response JSON: `id_token`, `access_token`, `refresh_token`.
- **Secondary token-exchange for an API key** (`obtain_api_key`,
  `login/src/server.rs`): `POST {issuer}/oauth/token` with
  `grant_type=urn:ietf:params:oauth:grant-type:token-exchange`,
  `requested_token=openai-api-key`, `subject_token=<id-token>`,
  `subject_token_type=urn:ietf:params:oauth:token-type:id_token` — trades the
  id_token for an API key stored as `OPENAI_API_KEY`. Best-effort; the
  ChatGPT-subscription mode does not require it.
- **Device-code flow** (`login/src/device_code_auth.rs`):
  `POST {auth_base}/deviceauth/usercode`, poll
  `POST {auth_base}/deviceauth/token`, user verifies at `{base}/codex/device`.

## 3. Token storage

Tokens live in `$CODEX_HOME/auth.json` (default `~/.codex/auth.json`) —
`login/src/auth/storage.rs`. On Unix the file is written mode `0o600`.
Alternative backends (OS keyring "Codex Auth", encrypted secrets, ephemeral) are
selectable via `AuthCredentialsStoreMode` = `File | Keyring | Auto`; the
plain-file backend is the canonical path.

Schema (`AuthDotJson` in `storage.rs`, `TokenData` in `login/src/token_data.rs`)
— field names only, values elided:

- `AuthDotJson`: `auth_mode`, `openai_api_key` (serde rename `OPENAI_API_KEY`),
  `tokens`, `last_refresh`, plus fields for other auth modes (`agent_identity`,
  `personal_access_token`, `bedrock_api_key`).
- `TokenData`: `id_token` (a JWT, serialized as the raw string and parsed into
  claims on load), `access_token` (itself a JWT), `refresh_token`, `account_id`.
- `IdTokenInfo` (parsed from the id_token JWT payload): `email`,
  `chatgpt_plan_type`, `chatgpt_user_id`, `chatgpt_account_id`,
  `chatgpt_account_is_fedramp`. Claims are read from the namespaced JWT paths
  `https://api.openai.com/auth` and `https://api.openai.com/profile`.

Account and plan identification come entirely from decoding the id_token JWT
locally (`chatgpt_account_id`, `chatgpt_plan_type` →
free/plus/pro/business/enterprise/edu). No separate profile call is required.
JWT payloads are base64url-decoded without client-side signature verification.

## 4. Token refresh

File: `login/src/auth/manager.rs`.

- **Endpoint:** `POST https://auth.openai.com/oauth/token` (`REFRESH_TOKEN_URL`;
  env override `CODEX_REFRESH_TOKEN_URL_OVERRIDE`). A revoke endpoint also
  exists (`https://auth.openai.com/oauth/revoke`).
- **Request** (`request_chatgpt_token_refresh`):
  `Content-Type: application/json`, JSON body
  `{ client_id, grant_type: "refresh_token", refresh_token }` →
  `{ id_token, access_token, refresh_token }`, persisted along with
  `last_refresh = now`.
- **Proactive-refresh trigger** (`should_refresh_proactively`): refresh if the
  access-token JWT `exp` is within **5 minutes**
  (`CHATGPT_ACCESS_TOKEN_REFRESH_WINDOW_MINUTES = 5`), else if `last_refresh` is
  older than **8 days** (`TOKEN_REFRESH_INTERVAL = 8`).
- **Failure classification** (`classify_refresh_token_failure`): body codes
  `refresh_token_expired` (Expired), `refresh_token_reused` (Exhausted),
  `refresh_token_invalidated` (Revoked), or HTTP 401, are **permanent** (force
  re-login); anything else is transient/retryable.

## 5. AuthMode and backend selection

`AuthMode` (`protocol/src/auth.rs`, serde lowercase): `ApiKey`, `Chatgpt`,
`ChatgptAuthTokens`, `Headers`, `AgentIdentity`, `PersonalAccessToken`,
`BedrockApiKey`. The flag that switches base URL is
`AuthMode::uses_codex_backend()` — true for
Chatgpt/ChatgptAuthTokens/Headers/AgentIdentity/PersonalAccessToken, false for
ApiKey/BedrockApiKey.

Base-URL selection (`ModelProviderInfo::to_api_provider`,
`model-provider-info/src/lib.rs`): subscription modes →
`https://chatgpt.com/backend-api/codex` (`CHATGPT_CODEX_BASE_URL`); API-key mode
→ `https://api.openai.com/v1`. A user config `chatgpt_base_url`/`base_url` can
override.

## 6. Request transport and headers

- **Endpoint path:** `/responses` (`RESPONSES_ENDPOINT`, `core/src/client.rs`),
  i.e. subscription requests go to
  `POST https://chatgpt.com/backend-api/codex/responses`; remote compaction uses
  `/responses/compact`. **Only the "responses" wire API remains** — the
  chat-completions wire API has been removed (`model-provider-info/src/lib.rs`
  hard-errors on `wire_api="chat"`).
- **Two transports:**
  1. **HTTP + SSE** — `POST /responses` with `Accept: text/event-stream`
     (`codex-api/src/endpoint/responses.rs`). The request body may be
     zstd-compressed.
  2. **Responses-over-WebSocket** —
     `codex-api/src/endpoint/responses_websocket.rs`, gated by a dated beta
     value (`responses_websockets=2026-02-06`), with automatic HTTP fallback; a
     per-turn `ModelClientSession` caches the WS connection. SSE-over-HTTP is
     the primary path.
- **Request body** `ResponsesApiRequest` (`codex-api/src/common.rs`): `model`,
  `instructions`, `input: Vec<ResponseItem>`, `tools`, `tool_choice`,
  `parallel_tool_calls`, `reasoning`, `store`, `stream`, `stream_options`,
  `include`, `service_tier`, `prompt_cache_key`, `text`, `client_metadata`.
  Conversation history is sent as `input` items each turn. Model naming is plain
  model-id strings passed straight through.
- **Auth headers** (`BearerAuthProvider::add_auth_headers`,
  `model-provider/src/bearer_auth_provider.rs`):
  - `Authorization: Bearer <access-token>` (the access token is itself a JWT).
  - `ChatGPT-Account-ID: <account-id>` — from the id_token's
    `chatgpt_account_id` claim, seeded into `TokenData.account_id` at login.
  - `X-OpenAI-Fedramp: true` only when the account is FedRAMP (which also
    changes routing).
  - API-key mode uses the same bearer provider with the API key as token and no
    account id; agent-identity and header-provider variants exist for other
    modes.
- **Session/conversation semantics:** `session-id` and `thread-id` propagate via
  headers (`codex-api/src/requests/headers.rs`) and `client_metadata`
  (`core/src/responses_metadata.rs`); `x-client-request-id` carries the thread
  id. A **sticky-routing token** `x-codex-turn-state` is returned on the first
  request of a turn and must be replayed on subsequent requests within that turn
  — but must NOT leak across turns (`core/src/client.rs`).
- **Notable headers** (`core/src/client.rs`, `default_client.rs`): `originator`
  (default `codex_cli_rs`; env override `CODEX_INTERNAL_ORIGINATOR_OVERRIDE`),
  `User-Agent` (`{originator}/{version} ({os} {ver}; {arch}) ...`),
  `OpenAI-Beta`, `x-codex-beta-features`, `x-codex-installation-id`,
  `x-codex-turn-state`, `x-codex-turn-metadata`, `x-openai-subagent`,
  `x-oai-attestation`, `x-responsesapi-include-timing-metrics`,
  `x-openai-internal-codex-responses-lite`, and optional residency
  `x-openai-internal-codex-residency`. In the agent-identity path an
  `x-openai-actor-authorization` header also appears.
- **Response headers** carry side-channel data: `openai-model`, `x-request-id`,
  `x-reasoning-included`, `x-codex-turn-state`, `X-Models-Etag`, and the
  rate-limit family below.

## 7. SSE event taxonomy

Parser: `codex-api/src/sse/responses.rs` (uses `eventsource_stream::Eventsource`
— Codex uses its own SSE handling rather than `reqwest-eventsource`). Events are
typed by a `"type"` discriminator; strings parsed in `process_responses_event`:

- `response.created` → `ResponseEvent::Created`
- `response.output_item.added` / `response.output_item.done` → `OutputItemAdded`
  / `OutputItemDone`
- `response.output_text.delta` → `OutputTextDelta`
- `response.custom_tool_call_input.delta` → `ToolCallInputDelta`
- `response.function_call_arguments.delta` — function-call argument deltas
- `response.reasoning_summary_text.delta` / `.done` → `ReasoningSummaryDelta` /
  `ReasoningSummaryDone`
- `response.reasoning_text.delta` → `ReasoningContentDelta`
- `response.reasoning_summary_part.added` → `ReasoningSummaryPartAdded`
- `response.metadata` → model/turn-state/verification/moderation side events
- `response.failed` → mapped to a typed error (see §8)
- `response.incomplete` → stream error
- `response.completed` → `Completed { response_id, token_usage, end_turn }` —
  the **terminal marker**, carrying usage including `input_tokens_details` /
  `output_tokens_details` (cached tokens)

There is no `[DONE]` sentinel — the terminal marker is the `response.completed`
event, and a stream that closes without one is treated as an error. A mid-stream
`rate_limits` event also exists (see §8).

## 8. Rate-limit signals and error shapes

- **Rate-limit signals** (`codex-api/src/rate_limits.rs`): a bespoke
  response-header family — `x-codex-primary-used-percent`,
  `x-codex-primary-window-minutes`, `x-codex-primary-reset-at` (plus
  `secondary-*` and `-limit-name` variants) and credits headers — parsed into a
  `RateLimitSnapshot`; plus the mid-stream `rate_limits` SSE event carrying
  primary/secondary windows, `credits`, and `plan_type`.
- **Error shapes** (`codex-api/src/error.rs`, `sse/responses.rs`): errors
  surface as `response.failed` with `response.error { code, message }`:
  - `rate_limit_exceeded` → `ApiError::Retryable { delay }`, where the delay is
    regex-parsed **from the message text** ("Please try again in 11.054s"), not
    from a `Retry-After` header.
  - `insufficient_quota` → `ApiError::QuotaExceeded` (**fatal**, deliberately
    not retried).
  - `context_length_exceeded` → `ContextWindowExceeded`.
  - `usage_not_included` → `UsageNotIncluded`.
  - Policy classes also appear (`cyber_policy`, `invalid_prompt`/bio-policy,
    `server_overloaded`/slow-down).
  - HTTP-level failures → `ApiError::Api { status, message }`; other transport
    issues → `ServerOverloaded` / `Stream`.

Notably, Codex already keeps **quota-exhausted distinct from rate-limited**,
matching the [runtime-substrate](../spec/runtime-substrate.md) invariant that
the substrate's `ProviderErrorKind` vocabulary demands.

## 9. Drift risk

**High-churn, internally-versioned surface.** Concrete drift vectors:

- **Non-versioned internal path.** The subscription endpoint is
  `chatgpt.com/backend-api/codex/responses` — an internal product path with no
  `/v1`. There is no public contract; it can change without notice (the repo's
  HEAD moves daily).
- **Hardcoded identity constants.** The client id, issuer, base URL, and scopes
  are baked-in constants. A reimplementation must track these and breaks if the
  client id is rotated or the scopes change.
- **Dated/experimental beta gates.** `responses_websockets=2026-02-06` (a
  literal date), `OpenAI-Beta`, `x-codex-beta-features`,
  `x-openai-internal-codex-responses-lite`, and the attestation header
  (`x-oai-attestation`) are all internal client-gating mechanisms that change
  frequently. The WebSocket transport in particular looks new and
  version-pinned.
- **Client fingerprinting.** `originator: codex_cli_rs` and the
  `codex_cli_rs/<version>` User-Agent are first-party markers the backend checks
  (`is_first_party_originator` enumerates `codex_cli_rs`, `codex-tui`,
  `codex_vscode`, `Codex …`). The backend can gate features or access on these.
  A reimplementation spending a subscription would need to present a first-party
  originator/UA to be accepted — the pragmatic path, and also the main
  ToS/stability risk, since it means tracking the real client's UA/version
  cadence.
- **Attestation.** `generate_attestation_header_for` (referenced from
  `core/src/client.rs`) suggests device/client attestation may become mandatory
  on some routes; if enforced, a third-party reimplementation could be locked
  out.

## 10. Mechanical vs. genuinely tricky

**Mechanical (straightforward to reimplement):**

- Wire types: `AuthDotJson`, `TokenData`, `IdTokenInfo`,
  `RefreshRequest`/`RefreshResponse`, the responses stream events and usage
  structs.
- PKCE generation (S256), the localhost callback server, authorize/token URL
  building.
- The SSE parsing loop and event-type → typed-event mapping.
- Base-URL and header selection.
- JWT payload decoding for claims/`exp` (unsigned base64url decode; no signature
  verification is done client-side).

**Genuinely tricky (needs care):**

- **Token lifecycle:** the proactive-refresh windows (5-minute `exp` buffer,
  8-day fallback), permanent-vs-transient failure classification, single-flight
  locking so concurrent requests do not double-refresh, and account-mismatch
  handling on refresh.
- **`account_id` derivation precedence:** stored `TokenData.account_id` versus
  the `chatgpt_account_id` id_token claim, plus workspace-restriction
  enforcement (`ensure_workspace_allowed`).
- **Anti-abuse / identity headers:** `originator` (and the first-party allowlist
  logic), the detailed `User-Agent`, `ChatGPT-Account-ID`, `X-OpenAI-Fedramp`,
  the residency header, `x-openai-subagent`, and — in the agent-identity path —
  `x-oai-attestation` and `x-openai-actor-authorization`. FedRAMP accounts
  additionally change routing.
- **Storage backends:** keyring / encrypted-secrets / auto-fallback beyond the
  plain `auth.json` file, plus `0o600` permissions and atomic-ish writes.
- **Error-mapping semantics in the SSE stream** (retry-after regex parsing,
  context-window/quota/policy classification) that any downstream retry logic
  would depend on.

## 11. Fit against the Signalbox runtime substrate

This is effectively a **third wire type**, not a config tweak of the existing
OpenAI `/v1/chat/completions` adapter: it targets `/responses` (a different
request/response and SSE grammar) against `chatgpt.com/backend-api/codex`,
authorized by a **refreshable OAuth access-token JWT plus a `ChatGPT-Account-ID`
header** rather than a static API key. Against
[runtime-substrate](../spec/runtime-substrate.md) and the
[adapter conformance template](runtime-adapter-conformance.md):

- The two-stage `prepare`/`execute`, single-POST, SSE-framing,
  typed-terminal-evidence model aligns well; Codex's own error taxonomy already
  honors the rate-limited-vs-quota distinction the spec demands.
- The **credential boundary is richer than the current file-key model.**
  `CredentialAccess::resolve` would need to yield a (possibly just-refreshed)
  bearer JWT plus the account id and FedRAMP flag, and something outside the
  runtime must own the 5-minute/8-day proactive OAuth refresh (Codex does this
  in its `AuthManager`, out of band from the request path). The substrate's
  per-request resolve semantics accommodate a rotating token, but the credential
  value becomes a small struct, not raw bytes.
- Because the license is Apache-2.0, the cheapest de-risking move is to study or
  selectively vendor `codex-login` (OAuth/PKCE/refresh/token storage) and the
  `codex-api` Responses SSE decoder as reference, keeping only the constants and
  header set to re-derive, rather than reimplementing the fast-moving
  beta/attestation header surface from scratch. Expect to re-sync against
  upstream periodically because the internal headers and the WS beta version
  move continuously.

The corresponding backlog track is deliberately deferred behind the wrapped-CLI
tracks and blocked on an owner decision recording the ToS/endpoint-drift
accepted cost; nothing here changes that sequencing.
