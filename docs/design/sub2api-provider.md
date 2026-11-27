# Sub2API Provider: Full Integration by Group Platform (Design)

> Status: **P1 implemented** (§8); P2 upstream quota, rejection handling and platform detection implemented (§9); the remaining P2 items and P3 are not implemented yet. Baseline: nyro v2.1.57 (`6674406`); sub2api `81ce8356d` (local `~/code/sub2api`).
> 中文版：[sub2api-provider_CN.md](sub2api-provider_CN.md)

## 1. Background, goals, and non-goals

**Current state**

- nyro has a single `openai/sub2api` channel ([openai/mod.rs](../../crates/nyro-core/src/provider/openai/mod.rs)): fixed `openai-responses`, reusing the Codex consumer egress contract (`CodexOAuthResponses` flavor, parameter stripping, reasoning defense, tool-ID normalization, Fast tier). Usage is local log accounting only (`LocalUsageAccounting::Sub2Api`).
- Every other sub2api group platform has to borrow the custom / anthropic / google presets with hand-built addresses. Typical problems:
  - Antigravity requires the `/antigravity` route prefix and native Gemini lives under `/v1beta`; a mismatched address/protocol pair yields 404 / 400.
  - The google preset defaults to `?key=` auth, which sub2api's `/v1/*` rejects (400 `api_key_in_query_deprecated`).
  - It is unclear which protocols sub2api forwards natively and which depend on group switches (`allow_messages_dispatch`, Claude Code only), producing unexplained 403s.
  - sub2api's real quota / balance / subscription windows are invisible, so routing cannot move away before the quota runs out.
  - URL-substring detection in the usage backends (`z.ai`, `api.kimi.com`, `token-plan.`, ...) can misclassify a relay host as a direct vendor.

**Goal**: add a dedicated **Sub2API** provider preset (vendor `sub2api`). The user only ① picks the platform (the sub2api group's platform type), ② enters the Sub2API address, and ③ enters the API key. nyro then handles protocol endpoints and route prefixes, uniform auth, model discovery, protocol negotiation and conversion direction, upstream dialect policies, quota queries and scheduling, and error hints.

**Non-goals**

- TypeSafe / Jev (only `/v1/systemone`, not a chat protocol).
- Images, video, voice, Realtime, Responses WebSocket, `/v1/messages/count_tokens` (nyro has no matching ingress yet).
- sub2api admin APIs (they need a user JWT; an API key cannot call them).
- Impersonating clients to bypass group policies (for example "Claude Code only").

## 2. Findings: sub2api's client-facing API

### 2.1 Group platforms

A sub2api API key is bound to one group, and each group has exactly one platform (`backend/internal/domain/platforms.go`). The group platform decides which gateway pipeline serves a request (`backend/internal/server/routes/gateway.go`).

| Platform id | Name | Gateway family | Upstream |
|---|---|---|---|
| `anthropic` | Anthropic | Anthropic gateway | Claude OAuth / setup token / API key / Bedrock / upstream passthrough |
| `openai` | OpenAI | OpenAI gateway | ChatGPT Codex OAuth / OpenAI API key |
| `gemini` | Gemini | Anthropic gateway (Gemini compat service) | AI Studio key / Code Assist OAuth / Vertex |
| `antigravity` | Antigravity | Antigravity gateway | Google v1internal (serves both Claude and Gemini models) |
| `grok` | Grok | OpenAI gateway | xAI OAuth (cli-chat-proxy) / xAI API key |
| `kimi` `zhipu` `deepseek` `minimax` | Chinese vendors | OpenAI gateway (multi-protocol API-key providers) | Native upstream endpoint chosen by the account's `api_protocol` |
| `opencode_go` `command_code` | Aggregators | OpenAI gateway | Routed per model to chat / messages / responses |
| `cline` | Cline | OpenAI gateway | Upstream offers Chat Completions only |
| `composite` | Composite group | Resolved per model to a concrete platform | Multiple platforms |
| `typesafe` | TypeSafe / Jev | — | Only `/v1/systemone` (**unsupported**) |

### 2.2 Endpoint × platform matrix

Legend: ✅ forwarded natively (zero conversion inside sub2api); 🔁 works after sub2api converts internally; ⚙️ needs a group switch; ❌ unavailable.

| Downstream endpoint | anthropic | openai | gemini | antigravity | grok | kimi / deepseek / minimax | zhipu | opencode_go / command_code | cline | composite |
|---|---|---|---|---|---|---|---|---|---|---|
| `POST /v1/messages` | ✅ | ⚙️🔁 `allow_messages_dispatch` | 🔁 | ✅ (prefer `/antigravity/v1/messages`) | 🔁 | ✅ | ✅ | ✅ / 🔁 (per model) | 🔁 | Resolved per model |
| `POST /v1/chat/completions` | 🔁 (rejected by Claude-Code-only groups) | 🔁 / ✅ | 🔁 | 🔁 | ✅ | ✅ | ✅ | ✅ / 🔁 | ✅ | Resolved per model |
| `POST /v1/responses` | 🔁 | ✅ | ❌ (no Responses path for Gemini accounts found) | 🔁 | ✅ | ✅ | 🔁 | ✅ / 🔁 | 🔁 | Resolved per model |
| `POST /v1beta/models/{m}:{action}` | ❌ | ❌ | ✅ | ✅ (only via `/antigravity/v1beta/...`) | ❌ | ❌ | ❌ | ❌ | ❌ | When resolved to Gemini |
| `POST /v1/embeddings` | ❌ | ✅ (API-key accounts) | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | When resolved to openai |

Notes:

- `/v1beta` requires a gemini group and otherwise returns 400 `API key group platform is not gemini`. The `/antigravity/*` routes carry `ForcePlatform(antigravity)`: they schedule Antigravity accounts only and never mix in other accounts.
- `/antigravity/v1/messages` serves both Claude and Gemini models (sub2api converts the Claude protocol to v1internal). There is no chat / responses under `/antigravity/v1/*`; those use the root `/v1/...` routes, which follow the group platform.

### 2.3 Authentication

- `/v1/*`: `Authorization: Bearer` → `x-api-key` → `x-goog-api-key`; a `?key=` query returns 400.
- `/v1beta/*`, `/antigravity/v1beta/*`: `x-goog-api-key` → Bearer → `x-api-key` → `?key=`.
- **Conclusion: use Bearer for every endpoint.**

### 2.4 Read-only helper endpoints (callable with the API key)

| Endpoint | Purpose | Notes |
|---|---|---|
| `GET /v1/models` | Group model list | Three shapes: OpenAI (openai), Grok (with `reasoningEfforts`), Claude (all other platforms); all contain `data[].id` |
| `GET /v1beta/models` | Gemini-shaped list | gemini groups only; when accounts declare no mapping it passes the upstream list through, which is more complete than the static `/v1/models` fallback |
| `GET /v1/usage` | Quota / balance / subscription / usage | Three modes, see §3.9 |
| `GET /v1/sub2api/billing[?include=group]` | Rate multiplier, group name | 404 in simple mode; **no platform field** |

No endpoint callable with an API key returns the group platform today, so auto-detection must rely on heuristics (§3.10).

### 2.5 Upstream adaptation sub2api already performs (nyro must not duplicate it)

- Claude OAuth: Claude Code impersonation, `metadata.user_id`, billing header.
- Antigravity: Claude / Gemini → v1internal envelope, identity prompt patch, forced upstream streaming with non-stream aggregation, thinking-tier model mapping, thought-signature injection and rectifying retries.
- Gemini: `thoughtSignature` injection (`ensureGeminiFunctionCallThoughtSignatures`), filtering empty parts.
- OpenAI OAuth: stripping fields Codex rejects (`openai_codex_transform.go`).
- OpenCode: `x-opencode-session` injection. Grok: Responses body sanitization.
- Account stickiness, concurrency queueing, account failover, and billing.

### 2.6 Constraints to surface to users

- **Claude-Code-only groups**: sub2api validates the `claude-cli/x.y.z` UA, system-prompt similarity, `X-App`, and more (`claude_code_validator.go`). Only a genuine Claude Code client passing natively through nyro works; nyro never impersonates clients.
- **`/v1/messages` on OpenAI groups** requires the group's `allow_messages_dispatch` (`openai_gateway_handler.go:310`); grok, the Chinese vendors, and the aggregators do not need it.
- **Antigravity Claude and Anthropic Claude must not be mixed in one conversation**: their thinking signatures differ.
- **Error semantics**: insufficient balance / subscription / key quota → **403 `billing_error`**; key 5h / 1d / 7d windows, RPM, and per-platform daily / weekly / monthly quotas → **429 `rate_limit_exceeded`** (some with `Retry-After`); billing-service failure → 503; no available account → 503 / 502.

## 3. Design

### 3.1 Division of responsibility

- **nyro owns the client side**: parsing the ingress protocol, choosing the egress protocol and sub2api route, any needed protocol conversion, and cross-provider routing, health, quota scheduling, and failover.
- **sub2api owns the account side**: account pools, upstream impersonation / envelopes / signatures, and billing.
- For sub2api providers nyro runs **no** OAuth, subscription envelope, Claude Code cloak, Antigravity tier rewrite, or thought-signature policy (all are gated on vendor identity, which a sub2api vendor never matches).

### 3.2 Data model (no database schema change)

| Field | Value |
|---|---|
| `vendor` / `preset_key` | `sub2api` |
| `channel` | Platform id, identical to sub2api's (for example `antigravity`, `opencode_go`) |
| `protocol_mode` | `adaptive` |
| `protocol_endpoints` | One row per checked protocol; every `base_url` is the normalized Sub2API root; the same `api_key`; `auth_scheme = bearer` |
| `base_url` | The root address |
| `models_source` | `{root}/v1/models` (`{root}/v1beta/models` for the gemini platform, see §3.8) |
| `fast_mode` | Effective on the openai platform only |
| `provider_keys` | Optional: a multi-key / multi-site pool for the same platform; per-key `base_url` overrides are normalized to roots too |

Standalone YAML needs an optional `channel` field on `YamlProvider` ([yaml_config.rs:43](../../src-server/src/yaml_config.rs#L43)).

### 3.3 Platform channel table (core)

"Checked by default" lists only protocols that sub2api forwards natively without any group switch. "Optional" protocols start unchecked; checking one hands its conversion to sub2api. An ingress protocol with no checked endpoint is converted by nyro to the default protocol.

| channel | Checked by default | Optional | Default protocol | Route prefix | Dialect (§3.6) | Capabilities source | Hint |
|---|---|---|---|---|---|---|---|
| `anthropic` | Messages | Chat, Responses | Messages | — | anthropic | models.dev `anthropic` | Claude-Code-only groups accept only genuine Claude Code passthrough |
| `openai` | Responses | Chat, Messages⚙️ | Responses | — | openai + Codex contract | `openai` | Messages needs the group's allow_messages_dispatch; Fast tier supported |
| `gemini` | Gemini | Messages, Chat | Gemini | — | google (API dialect, not a subscription channel) | `google` | nyro converts Responses clients to Gemini |
| `antigravity` | Messages, Gemini | Chat, Responses | Messages | Messages / Gemini → `/antigravity` | None (left to sub2api) | Auto | Do not mix with the Anthropic platform in one conversation |
| `grok` | Responses, Chat | Messages | Responses | — | xai | `xai` | nyro converts Claude Code to Responses by default (XaiStrictResponses) |
| `kimi` | Chat, Messages, Responses | — | Chat | — | moonshotai | `moonshotai` | |
| `zhipu` | Chat, Messages | Responses | Chat | — | zhipuai | `zhipuai` | No native Responses upstream |
| `deepseek` | Chat, Messages, Responses | — | Chat | — | deepseek | `deepseek` | |
| `minimax` | Chat, Messages, Responses | — | Chat | — | minimax | `minimax` | |
| `opencode_go` | Chat, Messages, Responses | — | Chat | — | opencode-go | `opencode-go` | sub2api picks the endpoint per model |
| `command_code` | Chat, Messages, Responses | — | Chat | — | Model-name rules only | Auto | Same as above |
| `cline` | Chat | Messages, Responses | Chat | — | Model-name rules only | Auto | Upstream offers Chat only |
| `composite` | Chat, Responses, Messages, Gemini | — | Chat | — | Transparent relay (like nyro-gateway) | Auto | sub2api resolves the platform per model |

Trade-off: a request is always converted exactly once, either in nyro or in sub2api. nyro converts by default because its converters pass the cc-switch parity audit, its logs keep full conversion evidence, and it does not depend on group switches. Per-model routing platforms (opencode_go / command_code / composite) pass all three or four protocols through and leave conversion to sub2api, so any ingress converts at most once.

### 3.4 URL rules

```
endpoint_url = sub2api_root(base_url) + platform.route_prefix(protocol) + encoder_egress_path
```

- `encoder_egress_path` is the encoder's standard path: `/v1/messages`, `/v1/chat/completions`, `/v1/responses`, `/v1/embeddings`, `/v1beta/models/{m}:generateContent` or `:streamGenerateContent?alt=sse`.
- `sub2api_root` trims trailing slashes, then removes one known suffix (case-insensitive, matched in order): `/antigravity/v1beta`, `/antigravity/v1`, `/antigravity`, `/backend-api/codex`, `/v1beta`, `/v1`. Reverse-proxy path prefixes are kept, and a host is never mistaken for a path segment (same rules as [nyro_gateway::gateway_root](../../crates/nyro-core/src/provider/nyro_gateway/mod.rs)).
- The backend normalizes `base_url`, every endpoint `base_url`, and key-pool `base_url` values on save; the vendor's `build_url` normalizes again at runtime as a safeguard.

| User input | Root |
|---|---|
| `https://s.example.com/v1` | `https://s.example.com` |
| `https://s.example.com/antigravity` | `https://s.example.com` |
| `https://s.example.com/antigravity/v1beta/` | `https://s.example.com` |
| `https://edge.example.com/sub2api/v1` | `https://edge.example.com/sub2api` |
| `http://v1` | `http://v1` |

| channel | Egress protocol | Final URL (root = `https://s.example.com`) |
|---|---|---|
| antigravity | Messages | `https://s.example.com/antigravity/v1/messages` |
| antigravity | Gemini (streaming) | `https://s.example.com/antigravity/v1beta/models/gemini-3.8-flash:streamGenerateContent?alt=sse` |
| antigravity | Chat (optional) | `https://s.example.com/v1/chat/completions` |
| gemini | Gemini | `https://s.example.com/v1beta/models/gemini-2.5-pro:generateContent` |
| openai | Responses | `https://s.example.com/v1/responses` |

Auth: the channel metadata declares `auth_schemes = bearer` for every protocol (like nyro-gateway), which stops the pipeline from rewriting Anthropic egress to `x-api-key` and Gemini egress to `?key=`. If a legacy row or YAML config carries `auto`, the sub2api vendor still treats it as Bearer.

### 3.5 Request-flow examples

**antigravity**

| Client ingress | nyro negotiation | Conversion | Upstream |
|---|---|---|---|
| Claude Code `/v1/messages` | Hits the Messages endpoint | Native passthrough | `/antigravity/v1/messages` |
| Gemini CLI `/v1beta/models/...` | Hits the Gemini endpoint | Native passthrough | `/antigravity/v1beta/models/...` |
| Codex `/v1/responses` | Responses unchecked → default Messages | nyro: Responses → Messages (compat) | `/antigravity/v1/messages` |
| OpenAI SDK `/v1/chat/completions` | Chat unchecked → default Messages | nyro: Chat → Messages | `/antigravity/v1/messages` |

**openai**

| Client ingress | Handling | Upstream |
|---|---|---|
| Codex | Responses passthrough + Codex contract + Fast tier | `/v1/responses` |
| Claude Code | Messages → Responses (compat, `CodexOAuthResponses`) | `/v1/responses` |
| OpenAI SDK (Chat) | Chat → Responses (IR) | `/v1/responses` |
| Gemini CLI | Gemini → Responses (IR) | `/v1/responses` |

### 3.6 Wire dialect mapping

Add a crate-internal `wire_dialect_vendor(provider)`: an ordinary provider returns `provider.vendor`; a sub2api provider returns the nyro vendor id matching its platform (the "Dialect" column of §3.3). **It is used only by wire-level policies:**

| Policy | Today | Value under sub2api |
|---|---|---|
| `apply_vendor_effort_policy` ([pipeline.rs:430](../../crates/nyro-core/src/provider/common/pipeline.rs#L430)) | Keyed on `provider.vendor` | Platform dialect (openai: off → `low`; grok: drop none, max → xhigh; opencode_go: off → low; ...) |
| Codex consumer contract (parameter stripping / reasoning defense / tool IDs / Fast; Responses egress only) | `codex`, plus the channel-only `sub2api` check | Adds `sub2api/openai` |
| compat Anthropic → Responses flavor | codex / sub2api → `CodexOAuthResponses`; xai → `XaiStrictResponses` | openai → Codex; grok → XaiStrict; others → Standard |
| Responses → Responses third-party rewrite | Excludes openai / codex / sub2api / xai | openai: no rewrite; grok: xAI native path; composite: no rewrite (transparent relay); other platforms: third-party rules |
| Anthropic → Anthropic DeepSeek history-replay normalization | deepseek / mimo | deepseek platform (to verify end to end) |

The dialect must **never** feed identity decisions: OAuth drivers, runtime bindings, `is_google_antigravity` / `is_google_gemini_cli`, Claude Code egress, usage backends, account pools, subscription envelopes, `apply_tier_model_rewrite`, or the thought-signature policy. sub2api owns these, or they do not apply.

Vendor-owned hooks such as `post_encode` (for example MiniMax reasoning defaults) do not run for sub2api; only the pipeline-level dialect policies are reused.

### 3.7 Compatibility with the legacy `openai/sub2api` channel

- One classifier, `Sub2ApiPlatform::of(provider)`: `sub2api/<platform>` returns that platform; `openai/sub2api` and the `custom/sub2api` form seen in historical tests both return `OpenAI`. Every Codex-contract, Fast-tier, and usage decision calls only this function, replacing today's scattered `channel == "sub2api"` checks.
- The legacy channel stays in the OpenAI metadata marked `deprecated`: hidden when creating providers, rendered normally when editing, with a "Migrate to the Sub2API preset" action (vendor / preset → `sub2api`, channel → `openai`, `base_url` normalized to the root, fixed protocol → adaptive with a single Responses endpoint, key and Fast tier kept, `models_source` → `{root}/v1/models`). Both forms go through the same classifier and behave identically; regression tests are parameterized over both.
- No automatic migration at startup, so user-customized addresses (for example `/backend-api/codex`) are never touched.

### 3.8 Model discovery and capabilities

- Model discovery **always uses Bearer**. Today [build_model_headers](../../crates/nyro-core/src/admin/model_catalog.rs#L104) sends no auth header for the Gemini protocol when the vendor is not google, and sub2api's `/v1/*` rejects `?key=`, so sub2api needs its own branch.
- The gemini platform uses `{root}/v1beta/models` (`models[].name`; the existing parser strips the `models/` prefix). Antigravity and every other platform use `{root}/v1/models` (which includes both Claude and Gemini models from the account mappings).
- Capabilities come from the "Capabilities source" column of §3.3. The OpenCode Go and Antigravity-subscription visibility filters are identity-gated and do not apply to sub2api (sub2api's list is already filtered by group).

### 3.9 Usage and scheduling (new backend `sub2api_key`)

Request `GET {root}/v1/usage` (Bearer, 20s timeout). P3 may also read `GET {root}/v1/sub2api/billing` for the multiplier, ignoring 404.

| sub2api field | nyro `ProviderUsage` |
|---|---|
| `mode=quota_limited`: `quota{limit, used, remaining}` | Tier `key_quota` (used / limit, no reset time) |
| `rate_limits[{window: 5h / 1d / 7d, limit, used, reset_at}]` | `five_hour` / `daily` / `weekly_limit`, `resets_at = reset_at` |
| `status` (active / quota_exhausted / expired), `isValid` | `is_available` |
| `expires_at`, `days_until_expiry` | Expiry information on the card |
| `mode=unrestricted` + `subscription{daily/weekly/monthly_usage_usd, *_limit_usd, weekly_window_start, expires_at}` | Windows with a configured limit become `daily` / `weekly_limit` / `monthly`; weekly `resets_at = weekly_window_start + 7d`; `level = planName` |
| Subscription `remaining` (0 = exhausted, -1 = unlimited) | `is_available` |
| Wallet mode `balance` | `balances[{currency: USD, total}]`; `is_available = balance > 0` |
| `usage.today.actual_cost`, `usage.total.actual_cost` | `spends[today, total]` |
| Local logs | `api_price` (existing local accounting, per calendar month) |

The daily window resets at midnight in sub2api's configured time zone, and the monthly window is `monthly_window_start + 30d`; `/v1/usage` returns neither anchor, so those `resets_at` values stay empty.

Scheduling rules (consistent with the existing backends):

- `is_available = false` pauses routing and is rechecked on the normal monitor cadence.
- A primary window at 100% with `resets_at` pauses routing until the reset; `key_quota` at 100% pauses routing and is rechecked periodically.
- A failed query or unknown data keeps the provider schedulable.
- Passive observation: a 403 `billing_error` or 429 `rate_limit_exceeded` fails over to the next target, is health-neutral locally, and triggers one rate-limited usage refresh.

Detection: `UsageBackend::detect` recognizes sub2api **by identity** (both the new vendor and the legacy channel) before the URL fallback ([usage.rs:523](../../crates/nyro-core/src/admin/usage.rs#L523)), and the WebUI's [provider-usage.ts](../../webui/src/lib/provider-usage.ts) mirrors it. If `/v1/usage` returns 404 or non-JSON (older sub2api), the card falls back to today's local accounting (kind `sub2api`) and scheduling is unaffected.

### 3.10 Connection test, platform detection, and error hints

**Connection test** (a new sub2api branch in `test_provider`; spends no tokens):

1. `GET /v1/usage`: validates the key and reads the group name; 401 reports "invalid API key".
2. Checks each enabled endpoint: a Gemini endpoint requests `GET {root}{prefix}/v1beta/models`, where 400 "not gemini" reports a platform mismatch; other endpoints request `GET /v1/models` and check the response shape.
3. The "test models" probe uses the vendor URL builder and Bearer (today [providers.rs:282](../../crates/nyro-core/src/admin/providers.rs#L282) calls `openai_build_url` directly and would drop the `/antigravity` prefix).

**Platform detection** (P2, a "Detect platform" button; it only suggests, and the user confirms):

| Signal | Conclusion |
|---|---|
| `GET /v1beta/models` returns 200 | gemini (or composite) |
| `/v1/models` in OpenAI shape (`owned_by: openai`) | openai |
| `/v1/models` in Grok shape (`reasoningEfforts` / `owned_by: xai`) | grok |
| Claude shape with `claude-*` plus tier-suffixed `gemini-*` / `gpt-oss-*` / `tab_flash_lite_preview` | antigravity |
| Claude shape with `claude-*` only | anthropic |
| Only one family among `deepseek-*` / `kimi-*` / `glm-*` / `MiniMax-*` | That Chinese platform |
| Models from several vendors | composite / opencode_go / command_code / cline (user chooses) |

Saving with a platform that disagrees with the detection shows a warning. Optional upstream enhancement: add `group_platform` to sub2api's `BillingGroupInfo`; nyro uses it for exact detection when present and falls back to the heuristics otherwise.

**Error classification** (`provider/sub2api/errors.rs`, called from all three upstream error exits: non_stream / stream / compat):

| Upstream response | Meaning | nyro handling |
|---|---|---|
| 403 `billing_error` | Insufficient balance / subscription / key quota | Allow failover; health-neutral locally; trigger a usage refresh; return the original error when no other target remains |
| 429 `rate_limit_exceeded` | Key window / RPM / platform quota | Existing 429 logic plus a usage refresh; honor `Retry-After` |
| 403 `restricted to Claude Code clients` | Claude-Code-only group | No retry; health-neutral; explain the cause |
| 403 `does not allow /v1/messages dispatch` | OpenAI group without dispatch enabled | Suggest unchecking Messages (nyro will convert instead) |
| 400 `API key group platform is not gemini`, 404 `... not supported for this platform` | Platform does not match the group | Suggest changing the platform; mark the model probe failed |
| 403 `model ... not in whitelist` | The group does not include the model | No retry; health-neutral |
| 503 `No available accounts`, 502 `Upstream request failed after retries` | sub2api account pool temporarily unavailable | Retry / fail over (existing logic) |

### 3.11 Session affinity (P2)

- **OpenAI-gateway platforms** (openai, grok, Chinese vendors, aggregators, composite): when the client sends none of `session_id` / `conversation_id` / `x-session-affinity` / `x-session-id` / `x-opencode-session` / `x-conversation-id` and the body has no `prompt_cache_key`, inject `x-session-affinity: nyro-<hex>` at the dispatcher's egress header merge point. The value is derived from the nyro API key and the conversation's first-turn fingerprint, using the same algorithm as `opencode_go::session`. sub2api uses it as its third-priority sticky key, pinning the conversation to one account and hitting the upstream prompt cache.
- **Anthropic-gateway platforms**: sub2api keys stickiness on `metadata.user_id` or `cache_control` content. nyro does not synthesize `metadata.user_id`, because that would change how sub2api classifies Claude Code clients. Optional P3: place cache breakpoints automatically when converting to Messages.

### 3.12 WebUI

- A new **Sub2API** preset card. Platforms are shown in three groups: subscription platforms (Anthropic / OpenAI / Gemini / Antigravity / Grok), Chinese vendors (Kimi / Zhipu / DeepSeek / MiniMax), and aggregators (OpenCode / Command Code / Cline / Composite).
- Generalize nyro-gateway's address field into a metadata-driven "relay address" component instead of hard-coding `isNyroGatewayPreset` ([providers.tsx:2111](../../webui/src/pages/providers.tsx#L2111)). The placeholder is `https://<sub2api-host>`, and the hint says addresses ending in `/v1`, `/antigravity`, or `/v1beta` may be pasted.
- Protocol picker: native protocols checked by default, optional ones unchecked; each row shows the final route (for example `…/antigravity/v1/messages`); optional rows are labeled "converted by Sub2API" or "requires a group switch". The platform's hint appears under the platform selector.
- The Fast tier shows only for `sub2api/openai` and the legacy channel ([provider-speed-mode.ts:9](../../webui/src/lib/provider-speed-mode.ts#L9) must also check the vendor).
- Usage card: kind `sub2api_key`, titled "Sub2API Quota", showing windows, balance, spend, and local equivalent price.
- Editing a legacy `openai/sub2api` channel shows the migration prompt. All new strings default to English with Chinese alternatives (per AGENTS.md).

## 4. Implementation checklist

### 4.1 New

- `crates/nyro-core/src/provider/sub2api/`: `mod.rs` (`Sub2ApiVendor`, the 13-channel `METADATA`, `is_sub2api`), `platform.rs` (`Sub2ApiPlatform`: route prefixes, dialect, capabilities), `url.rs` (`sub2api_root`, `endpoint_url`), `usage.rs`, `errors.rs`, `detect.rs` (P2).
- `webui/src/lib/relay-address.ts` (generalized from `nyro-gateway.ts`), `webui/src/lib/sub2api.ts`, and an icon (the sub2api repository is LGPL-3.0; prefer a generic relay icon, or confirm the logo's license first).

### 4.2 Metadata extensions ([metadata.rs](../../crates/nyro-core/src/provider/metadata.rs))

Add the following fields to `ChannelDef` together with a `ChannelDef::DEFAULT` constant, and switch the existing literals to `..ChannelDef::DEFAULT` once (a mechanical change):

- `relay: Option<RelayAddressing { models_path, route_prefixes, root_suffixes }>`
- `optional_protocols: &[&str]`
- `deprecated: bool`
- `notes: Option<Label>`

nyro-gateway moves to `relay` as well. The WebUI type lives in [types.ts:791](../../webui/src/lib/types.ts#L791).

### 4.3 Call sites to change

- [registry.rs:226](../../crates/nyro-core/src/provider/registry.rs#L226): insert `sub2api` after `nyro-gateway` in `LEGACY_ORDER`, and update the `tests/fixtures/providers_legacy.json` snapshot.
- [resolver.rs:138](../../crates/nyro-core/src/conversion/resolver.rs#L138), [resolver.rs:163](../../crates/nyro-core/src/conversion/resolver.rs#L163), [pipeline.rs:31](../../crates/nyro-core/src/provider/common/pipeline.rs#L31), [pipeline.rs:51](../../crates/nyro-core/src/provider/common/pipeline.rs#L51), [pipeline.rs:83](../../crates/nyro-core/src/provider/common/pipeline.rs#L83), [pipeline.rs:419](../../crates/nyro-core/src/provider/common/pipeline.rs#L419): switch to the provider-aware `is_codex_consumer` and `wire_dialect_vendor`; the Fast-tier functions take a flag derived from the provider instead of only the channel.
- `proxy/dispatcher/compat.rs`: flavor selection and the service-tier override. `proxy/dispatcher/mod.rs`: the error-classification hook and the affinity header (P2).
- [providers.rs:282](../../crates/nyro-core/src/admin/providers.rs#L282): the model probe URL goes through the vendor builder; `test_provider` gains a sub2api branch; create / update normalize the root, force Bearer, and validate the channel (rejecting `typesafe` and unknown values).
- [model_catalog.rs:69](../../crates/nyro-core/src/admin/model_catalog.rs#L69), [model_catalog.rs:112](../../crates/nyro-core/src/admin/model_catalog.rs#L112): sub2api model discovery always uses Bearer.
- [usage.rs:370](../../crates/nyro-core/src/admin/usage.rs#L370), [usage.rs:424](../../crates/nyro-core/src/admin/usage.rs#L424): a new `Sub2Api` upstream backend recognized by identity first, with the legacy local accounting as the fallback; `provider_api_price.rs` attaches `api_price` for `sub2api_key`.
- WebUI: [providers.tsx](../../webui/src/pages/providers.tsx) (relay address, optional protocols, hints, hiding deprecated channels, migration prompt), [provider-usage.ts](../../webui/src/lib/provider-usage.ts) (identity first, title, price support), [provider-speed-mode.ts](../../webui/src/lib/provider-speed-mode.ts).

### 4.4 Channel-name collision audit (vendor checks required)

sub2api channel ids reuse existing channel names (`antigravity`, `openai`, `grok`, ...). These checks look at the channel alone and must add a vendor condition:

- [providers.rs:158](../../crates/nyro-core/src/admin/providers.rs#L158): the Gemini probe identifies antigravity / gemini-cli by channel only. Left unchanged, `sub2api/antigravity` would be wrapped in a v1internal envelope and sent to `/v1internal:streamGenerateContent`; this bug would certainly occur.
- [providers.rs:448](../../crates/nyro-core/src/admin/providers.rs#L448): `google_subscription_not_found_hint`.
- [google_display.rs:24](../../crates/nyro-core/src/quota_buckets/google_display.rs#L24): `google_quota_channel`.
- `is_antigravity_channel` at `provider/google/antigravity.rs:493` and `admin/usage.rs:2977` (both currently sit inside Google branches; add the check defensively).
- New regression test: for every sub2api channel, assert that the Google subscription, Claude Code, Codex OAuth, xAI Grok, OpenCode Go, nyro-gateway, and Devin / Kiro predicates all return false; also assert that `UsageBackend::detect` still returns sub2api for hosts containing `z.ai`, `api.kimi.com`, or `token-plan.`.

### 4.5 Existing issues found during the investigation

1. [providers.rs:399](../../crates/nyro-core/src/admin/providers.rs#L399): the Gemini probe used to append `?key=` after `?alt=sse`, producing `...?alt=sse?key=...`; P1 changed it to `&key=`.
2. URL-substring matching in the usage backends (`z.ai` at [usage.rs:533](../../crates/nyro-core/src/admin/usage.rs#L533), with the same check in the WebUI) matches any host containing `z.ai` (such as `xyz.ai`); host-suffix matching is recommended.

## 5. Test plan

- **Unit**: the root normalization table; the URL matrix for 13 platforms × protocols; metadata invariants (shared key, empty `base_urls`, Bearer for every protocol, optional protocols a subset of `base_urls`, default protocol not optional, no typesafe); the dialect mapping; the Codex contract and effort policy parameterized to behave identically for `openai/sub2api` and `sub2api/openai`; the §4.4 collision regression; `/v1/usage` fixtures for each mode (quota_limited with windows, subscription, wallet, expired, exhausted, `isValid=false`, 404 fallback) and their scheduling outcomes; error classification.
- **Integration** (`crates/nyro-core/tests/sub2api_*.rs`): an axum mock sub2api recording method, path, headers, and body. For each platform × ingress (chat / responses / messages / gemini), assert the correct path and prefix, Bearer auth, no `?key=`, the expected passthrough or conversion, and Codex sanitization on the openai platform; a 403 `billing_error` fails over to a second backend; the `/v1/usage` mapping and the routing pause.
- **WebUI**: relay-address normalization, the initial picker state (optional protocols unchecked), Fast-tier visibility, usage kind and title; run `npm run lint` and `npm run build`.
- **Manual E2E**: with one key per platform group on a real sub2api, verify model discovery, the connection test, the model probe, and Claude Code, Codex, Gemini CLI, and the OpenAI SDK through nyro.
- **Commands**: `cargo test -p nyro-core sub2api`, `cargo clippy -p nyro-core --all-targets`, `make check`.

## 6. Phases

| Phase | Scope |
|---|---|
| **P1 usable** | Vendor and 13 channels; URL and auth; model discovery; dialect mapping and Codex-contract parity; collision audit; probe / connection-test fixes; WebUI preset, address, and picker; legacy channel marked deprecated; docs and tests |
| **P2 operations** | `/v1/usage` backend and scheduling; error classification (failover on billing errors); platform detection; session affinity; legacy migration action; YAML `channel` field |
| **P3 enhancements** | Rate-multiplier display; Embeddings endpoint (endpoint-level ids in the picker); signature-domain mixing warning; count_tokens passthrough (needs a new ingress); upstream PR (`group_platform` and window anchors) |

## 7. Risks and open questions

1. The sub2api behavior above comes from the `81ce8356d` source, and sub2api moves quickly. The platform table lives in one module; unknown channels are rejected on save, while existing rows keep running with generic behavior.
2. "gemini groups do not support `/v1/responses`" comes from reading the source and needs confirmation against a live instance.
3. Whether DeepSeek's Anthropic history-replay normalization is still needed behind sub2api needs end-to-end confirmation.
4. nyro cannot know which platform a composite model resolves to, so composite groups are treated as a transparent relay.
5. Antigravity model names follow sub2api's `/v1/models` (account mapping keys); nyro does no tier rewriting.

## Appendix: Standalone YAML example (after P2 adds `channel`)

```yaml
providers:
  - name: sub2api-antigravity
    vendor: sub2api
    channel: antigravity
    default_protocol: anthropic
    endpoints:
      anthropic:
        base_url: https://sub2api.example.com
      gemini:
        base_url: https://sub2api.example.com
    api_key: ${SUB2API_ANTIGRAVITY_KEY}
    models_source: https://sub2api.example.com/v1/models
```

## 8. P1 implementation record

| Area | Delivered |
|---|---|
| Metadata | `ChannelDef` gains `default_protocol`, `relay` (`RelayAddressing`: `models_path` / `route_prefixes` / `root_suffixes`), `optional_protocols`, `deprecated`, `notes`, plus `ChannelDef::DEFAULT`; nyro-gateway now declares `relay` too |
| Vendor | [sub2api/mod.rs](../../crates/nyro-core/src/provider/sub2api/mod.rs) (13 platform channels, `Sub2ApiVendor`, save-time `normalize_admin_fields`), [platform.rs](../../crates/nyro-core/src/provider/sub2api/platform.rs) (`Sub2ApiPlatform`: route prefixes, dialect, Codex contract, transparent relay, discovery path), [url.rs](../../crates/nyro-core/src/provider/sub2api/url.rs) (root normalization and URL building) |
| Dialect | [dialect.rs](../../crates/nyro-core/src/provider/dialect.rs): `wire_dialect_vendor`, `is_codex_consumer`, `is_openai_codex_consumer`, `is_openai_native_responses`, `is_transparent_relay`; the resolver, the pipeline (three Codex sanitizers, Fast tier, effort policy) and the Codex speed-routing hint use them |
| Admin | Save-time platform validation, root normalization, forced Bearer and default discovery URL; discovery always uses Bearer; connectivity checks list models on the platform's routes; model probes use the platform prefix and Bearer; the probe channel is vendor-qualified (`probe_channel`), so a Sub2API Antigravity probe is never wrapped in a v1internal envelope |
| Usage | Sub2API (new preset and legacy channel) is local accounting by identity; `UsageBackend::detect` returns `None` |
| Fixed along the way | Gemini probe URL `?alt=sse?key=` → `?alt=sse&key=` |
| WebUI | Sub2API preset card and icon; a metadata-driven relay address field ([relay-address.ts](../../webui/src/lib/relay-address.ts)); the protocol picker marks optional protocols and previews final routes; platform notes; channel default protocols; the Fast tier only for Codex consumer channels; the legacy `openai/sub2api` channel is hidden when creating |
| Tests | [sub2api/tests.rs](../../crates/nyro-core/src/provider/sub2api/tests.rs) (root, URL matrix, metadata invariants, platform classification, dialects, name-collision regression), unit tests in [dialect.rs](../../crates/nyro-core/src/provider/dialect.rs), [sub2api_provider.rs](../../crates/nyro-core/tests/sub2api_provider.rs) (end-to-end with a mock Sub2API: Antigravity prefix, Gemini conversion and discovery, OpenAI Codex contract, multi-protocol passthrough, platform-mismatch hint, unknown-platform rejection, model probe routes), WebUI [relay-address.test.ts](../../webui/src/lib/relay-address.test.ts) / [sub2api.test.ts](../../webui/src/lib/sub2api.test.ts) |

Progress after P1 is recorded in §9.

## 9. P2 implementation record (quota, rejections, platform detection)

| Area | Delivered |
|---|---|
| Upstream quota | [usage.rs](../../crates/nyro-core/src/provider/sub2api/usage.rs): `GET {root}/v1/usage?days=1&start_date=…&end_date=…` (today's statistics only, so polls stay cheap). `quota_limited`: the key's total quota → `key_quota` (no reset), 5h/1d/7d → `five_hour`/`daily`/`weekly_limit` (`reset_at` normalized to UTC seconds); `unrestricted` subscription: daily/weekly/monthly USD limits → `daily`/`weekly_limit`/`monthly` (weekly reset = `weekly_window_start + 7d`; daily/monthly anchors are not reported, so no reset time), `planName` → level; wallet: `balance` → a USD balance; `usage.today/total.actual_cost` → `spends`. Key status `quota_exhausted`/`expired`/`disabled`, `isValid=false` or a balance ≤ 0 → unavailable |
| Usage backend | [admin/usage.rs](../../crates/nyro-core/src/admin/usage.rs): `UsageBackend::Sub2Api` (kind `sub2api_key`), only when one key speaks for the provider (a usable key and no key pool); key pools and keyless rows stay local accounting. 404/405/501 or a non-Sub2API JSON body (`UsageUnsupported`) falls back to the local card, and forwarding reports `unsupported`. The admin card shows used and total USD amounts per window and the expiry time when supplied. Local all-time and monthly totals appear below |
| Scheduling | Reuses `ProviderQuotaRegistry`: a full window pauses until its reset; `key_quota`, reset-less windows and unavailability pause with a 60s recheck; a failed quota check releases an existing Sub2API pause and backs off the next check |
| Rejection classification | [errors.rs](../../crates/nyro-core/src/provider/sub2api/errors.rs): `classify(status, body)` recognizes Sub2API's own billing refusals by code and message (`billing_error`, `insufficient_quota`, `INSUFFICIENT_BALANCE`, `API_KEY_QUOTA_EXHAUSTED`, `API_KEY_EXPIRED`, subscription/limit/RPM codes, plus `rate_limit_exceeded` and Google envelopes where only the message survives) and four configuration refusals; upstream account 429s/throttles never match |
| Failover | All three dispatcher HTTP error exits (non-stream, stream, compat) tag matches with `Sub2ApiRejection` and health neutrality; tagged responses fail over to the next route target whatever the status (403/404 included); billing refusals trigger one usage refresh (429s keep the existing refresh) |
| Hints | Request logs append an English hint to `error_causes` (`LogDiagnostic::add_cause`, leaving outcome/kind/stage unchanged); model probe and connection-test errors carry the same hint, localized in the Chinese WebUI |
| Platform detection | [detect.rs](../../crates/nyro-core/src/provider/sub2api/detect.rs): reads `/v1/models`, `/v1beta/models` and `/v1/sub2api/billing?include=group` in parallel; the pure `infer` returns a platform, confidence (high/medium/low), candidates and evidence; the group name only breaks ties. Admin API `POST /api/v1/sub2api/detect-platform`, desktop command `detect_sub2api_platform` |
| WebUI | A "Detect platform" button and result panel ([sub2api-platform-detect.tsx](../../webui/src/components/sub2api-platform-detect.tsx); evidence localized; one-click switch to the suggestion). Saving a new provider, or an edit that changed the platform/address/key, reuses or runs detection and asks for confirmation when a medium/high-confidence result disagrees; detection failures never block saving. The usage card gains the `sub2api_key` view |
| Tests | Unit tests for quota parsing, rejection classification, platform inference and health-neutral tagging; [sub2api_provider.rs](../../crates/nyro-core/tests/sub2api_provider.rs) adds quota and pause, the no-`/v1/usage` fallback, billing-refusal failover with hint and pause, 404 configuration-refusal failover, probe hints and platform detection; WebUI tests cover the quota card and detection rules |

Not yet implemented: the legacy-channel migration action (§3.7), session affinity (§3.11), the YAML `channel` field, and all P3 items.
