> This repository is an independently maintained Rust fork of Nyro.
> It is not affiliated with or endorsed by the original maintainers.
> The Go rewrite is preserved in the `upstream-go` branch.

<p align="center">
  <img width="120" src="docs/images/NYRO-logo.png">
</p>

<h2 align="center">Nyro AI Gateway</h2>

<p align="center">
  Run your AI coding tools on any model, from any provider.<br>
  One gateway. All protocols. No code changes.
</p>

<p align="center">
  <a href="https://github.com/Link2Link/nyro/releases/latest"><img src="https://img.shields.io/github/v/release/Link2Link/nyro" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-Apache%202.0-blue.svg" alt="License"></a>
  <a href="README_CN.md"><img src="https://img.shields.io/badge/文档-中文-8A2BE2" alt="中文"></a>
</p>

---

<p align="center">
  <img src="docs/images/NYRO-ui-home-en.png" width="800">
</p>

---

## What is Nyro?

Nyro is a local AI gateway that sits between your AI tools and model providers. It translates protocol formats on the fly — so Claude Code, Codex CLI, Gemini CLI, OpenCode, and any client using OpenAI / Anthropic / Gemini SDKs can all route through any backend model you choose, without changing a single line of code.

Point your clients at `http://localhost:19530`. Nyro handles the rest.

```
Claude Code · Codex CLI · Gemini CLI · OpenCode
     OpenAI SDK · Anthropic SDK · Gemini SDK
              Any HTTP API Client
                      ↓
              Nyro AI Gateway
            (localhost:19530)
                      ↓
    OpenAI · Anthropic · Google · DeepSeek
    MiniMax · xAI · GLM · Xiaomi MiMo
       Kimi · Ollama · ...
```

Nyro ships as a **desktop app** (macOS / Windows / Linux) and a **standalone server binary** for headless and self-hosted deployments.

---

## Why Nyro?

**Use any model with any tool.** Claude Code expects Anthropic protocol. Codex CLI uses OpenAI Responses API. Gemini CLI speaks Gemini. Nyro translates between all three so one model can serve all your tools simultaneously.

**Switch providers without touching your tools.** Change the target model or provider from Nyro's UI. Your tools never need reconfiguring.

**Keep everything local.** API keys are stored locally. Requests stay on your machine. No cloud relay, no shared infrastructure.

**One UI for everything.** Manage providers, routes, API keys, logs, and usage stats from a single interface — desktop app or browser.

---

## Screenshots

<table>
  <tr>
    <td align="center" width="50%"><img src="docs/images/NYRO-ui-providers-add-en.png" height="260"><br><sub>Provider Management</sub></td>
    <td align="center" width="50%"><img src="docs/images/NYRO-ui-routes-en.png" height="260"><br><sub>Route Configuration</sub></td>
  </tr>
  <tr>
    <td align="center" width="50%"><img src="docs/images/NYRO-ui-apikeys-en.png" height="260"><br><sub>API Key Management</sub></td>
    <td align="center" width="50%"><img src="docs/images/NYRO-ui-connect-en.png" height="260"><br><sub>Connect — Code & CLI Integration</sub></td>
  </tr>
</table>

---

## Features

### Protocol Translation

- **Ingress**: OpenAI (Chat Completions + Responses API), Anthropic Messages, Gemini GenerateContent
- **Egress**: route to any OpenAI-compatible, Anthropic, or Gemini upstream
- **Streaming**: full SSE passthrough and cross-protocol format conversion
- **Reasoning**: `<think>` tag parsing and conversion across protocol boundaries
- **Tool calls**: cross-protocol tool call and result format normalization
- **Route types**: chat and embedding routes with type-aware handling

### Devin CLI

Select **Devin → Devin CLI subscription** when adding a provider. Open the sign-in page and paste the authorization code back into Nyro; an existing `devin-session-token$…` token can also be imported in the same masked field. This uses Devin CLI's PKCE login and native Codeium Connect/protobuf chat endpoint, **not** the public Devin sessions API or an OpenAI-compatible relay. No local Devin CLI process or tools are launched.

- The fixed upstream is `https://server.codeium.com` with protocol `devin-chat/get-chat-message/v1`; API keys, adaptive protocols and custom upstream origins are not supported.
- Create a model route targeting this provider (for example `swe-2`, mapped to `swe-2-high` by default). Clients keep using Nyro's OpenAI Chat/Responses, Anthropic or Gemini endpoints, in streaming or non-streaming mode.
- The built-in model list is a set of suggestions, **not a live entitlement catalog**. Edit **Devin model UIDs** to use exact account-supported model names. Explicit effort variants override request-body effort; known base names map to upstream variants.
- A known base name maps to the upstream variant matching the request's own reasoning effort, so one route can serve every tier. This covers `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-fable-5-1` and `gpt-6-astra` (`low`/`medium`/`high`/`xhigh`/`max`), `gemini-4-argon` (`low`/`medium`/`high`), plus the earlier `swe-2`, `gpt-5-2` and `gemini-3-8-flash`. For the five newer families, an effort the family does not offer is clamped to the nearest tier (thinking off and `minimal` → `low`; `xhigh`/`max` → `high` on `gemini-4-argon`), and a request with no reasoning directive uses the family default (`medium`; `high` for `gemini-4-argon`). An explicit variant such as `claude-opus-5-5-high` is sent as-is and never re-mapped.
- Text, function tool calls/results, reasoning/signatures, inline base64 images and reported token/cache usage are supported. Remote image URLs, embeddings, hosted tools, strict/forced tool constraints and structured output are rejected rather than silently dropped. Tool arguments and text following reasoning may be buffered until the verified terminal frame. The default completion budget is 64,000 tokens; an explicit budget is forwarded to the upstream.
- **Usage** reads the same authenticated `GetUserStatus` call: the plan name, the **daily** and **weekly** quota windows (reported as remaining, shown as used) and the **extra-usage balance**. The balance only applies once a window is spent, so it decides availability: a spent window pauses routing until its reset **only when the extra balance is $0**; with a positive balance the provider stays schedulable, shows an "on extra balance" notice, and under the `usage` balance strategy is ranked after every provider that still has included quota or unknown usage (extra usage is billed, so it is a last resort). Credit-billed plans report no quota windows. When no weekly window is reported, the daily window is scored in the `usage` strategy like any other single-window provider.
- **Connection test** calls authenticated `GetUserStatus` without generating chat. **Model test** sends a short chat and may consume quota; at most 16 models are tested per call. Session tokens have no refresh grant: use **Reauthorize** if access is revoked.

Devin is a supported Nyro provider, implemented using CLIProxyAPI's observed Devin wire protocol. Availability and model access depend on your Devin account, and upstream protocol changes may require adapter updates. Authentication and protocol behavior are covered by local fixtures; a live account is still needed to verify your subscription end to end.

### Kiro IDE / CLI

Select **Kiro → Kiro IDE / CLI subscription** when adding a provider, choose a login method and start authorization. **Google** (default), **GitHub**, **AWS Builder ID** and **IAM Identity Center** use browser device authorization: open the displayed URL, check/enter the one-time user code, and approve sign-in. Nyro polls automatically; after authorization, save the new provider. Reauthorization of an existing provider binds the new credential automatically. No local Kiro process, callback port or port forwarding is needed—even when Nyro runs on a remote server. If the browser does not open, use the displayed Open/Copy link.

- For **IAM Identity Center**, enter your administrator-provided HTTPS AWS access portal Start URL and **OIDC region**. The **generation region** is separate (default `us-east-1`); a discovered profile ARN determines its home region. A login attempt expires within five minutes (or sooner if the upstream code expires). Cancel or switch login methods to discard the old session; start again after expiry. Device codes and client-registration secrets stay on the server, never in public session responses. This supports AWS IdC, not arbitrary external identity-provider OAuth endpoints.
- **Import credential** remains available as a fallback. Import the contents of `kiro-auth-token.json`. For **AWS Builder ID / IAM Identity Center**, paste the token JSON followed by the client-registration JSON containing `clientId` and `clientSecret` (two consecutive JSON objects are accepted). A bare social refresh token or a `ksk_` static API key is also accepted. Tokens are secrets: do not put them in model names, logs or screenshots. Use **Re-import credential** to replace a revoked token or static key.
- The provider uses the fixed native `kiro-chat/generate-assistant-response/v1` protocol, with JSON requests and CRC-checked AWS EventStream responses—not an OpenAI-compatible upstream. The regional `https://q.<region>.amazonaws.com` origin follows the credential; the default is `us-east-1`. Builder ID without a profile uses the regional CodeWhisperer generation host. `idcRegion` can separately select the OIDC refresh region. Social and OIDC refresh grants rotate automatically; social token refresh stays on the documented US-East authentication service even when the model/profile is in Europe. Static `ksk_` keys have no refresh grant. Adaptive endpoints and API-key pools are not supported.
- Create a model route using the native model suggestions or an exact upstream ID. For example, `claude-sonnet-4-5` maps to `claude-sonnet-4.5`. Suggestions are **not an account-specific entitlement catalog**. Clients keep using Nyro's OpenAI Chat/Responses, Anthropic or Gemini endpoints, streaming or non-streaming. Function tool continuations, inline base64 images and reasoning are supported; tool arguments may be buffered until completion.
- Remote image URLs, embeddings, forced tool choices, structured output and unsupported stateful/generation options are rejected. Sampling knobs (`temperature`, `top_p`, positive `max_tokens`) are not forwarded because the native API has no corresponding controls. Token usage is estimated from the reported context percentage or local text size, not an authoritative billing counter.
- **Connection test** and **Usage** call `getUsageLimits` without generating chat. Usage shows the plan, pooled credits and base/bonus/trial detail. Active grants are additive within each resource; exhausted individual grants do not pause a provider with remaining pooled credits. Exhausted pools pause routing unless the upstream explicitly reports that **overage is already enabled**; then the `usage` routing strategy treats it as a paid last resort. Nyro never enables overage for you. Expired grants are excluded; unknown quota shapes do not overwrite a known scheduling observation. **Model test** generates a short reply and can consume credits (up to 16 models per call).

Kiro is a supported Nyro provider; its adapter follows observed Kiro wire behavior. Browser login uses Kiro's social device endpoints and the [AWS SSO OIDC APIs](https://docs.aws.amazon.com/singlesignon/latest/OIDCAPIReference/Welcome.html), consistent with [Kiro's remote authentication flow](https://kiro.dev/docs/getting-started/authentication/). Local fixtures cover device initialization/polling, denial, timeout, slowdown, cancellation, binding and credential isolation as well as chat/quota behavior. Anonymous initialization was checked against the live Google/GitHub and Builder ID endpoints; approving a real account and validating model entitlement, refresh and billing still require account-level acceptance testing.

### Routing

- Exact match routing on `virtual_model`
- Virtual model names decouple client requests from actual backend models
- Multi-target routing with four strategies: weighted load balancing, priority-based failover, latency-first ordering, or usage-priority dynamic weighting from upstream subscription quotas
- Optional per-model degraded fallback: one backend row flagged as last resort, called only after every regular target is unavailable
- Health-aware failover: 3 consecutive failures mark a target unhealthy, auto-recovery after 30s
- Per-route access control with API key authorization

### Caching

- **Exact cache**: identical requests return cached responses instantly
- **Semantic cache**: embedding-based similarity matching reuses results for similar queries
- Per-route TTL and threshold overrides
- Stream replay for cached streaming responses

### Model Capabilities

- Auto-detect model capabilities (tool call, reasoning, context window, modalities, costs)
- `ai://models.dev` built-in data source for offline capability lookup
- HTTP model list endpoints for dynamic model discovery
- Capability tags shown in route configuration UI

### Security

- Independent proxy and admin bearer token controls
- Default-deny route authorization — keys must be explicitly bound to routes
- Per-key quotas: RPM / RPD / TPM / TPD

### Management

- Full CRUD for providers, routes, and API keys
- Request logs with provider, model, token, and latency detail
- Usage charts by model and provider, with a remembered **Token / API value** switch for the UTC daily calendar, monthly trends, and model breakdown. API value uses current local reference prices per million tokens: uncached input + cache reads + output, without double-counting cache. USD and CNY estimates are merged at a fixed 1 USD = 7.0 CNY; the display currency switch converts the same merged total. Unknown prices show as unpriced, not free. Models without an exact price inherit the longest valid prefix match (e.g. `claude-haiku-5-5-medium` inherits `claude-haiku-5-5`). The compact cost list combines known execution variants: Claude effort suffixes (such as `claude-sonnet-5-5-max`), Grok `-build-fast` (such as `grok-4.7-build-fast` → `grok-4.7`), and Gemini effort/tier suffixes (such as `gemini-3.8-flash-high` → `gemini-3.8-flash`). Versions and products (`flash`, `pro`, `flash-lite`, `flash-image`) remain distinct. Usage across providers is summed without repricing any attempt. It shows the top five model totals by default; expand for provider/variant details and native rates, or show all models. This is an estimate over retained logs, not a bill or subscription savings; cache-write premiums, long-context tiers, discounts, and non-token fees are not separately modeled.
- **Manual API price overrides**: in Stats → API value, expand a model’s cost row and select **Set prices** on the exact provider/variant detail. Defaults follow the reference catalog; set USD/CNY input, cache-read and output rates per million tokens for an exact provider/model pair, or restore defaults with confirmation. A saved rule also prices longer variants of the same model prefix unless they have their own rates (e.g. `claude-haiku-5-5` covers `claude-haiku-5-5-medium`); the editor shows inherited rates and their source model. Blank cache rates use the input rate; explicit zero means free. Overrides are saved in gateway settings (persistent with database storage), take priority over automatic prices, and revalue retained historical and future usage without changing logs or upstream billing. Configuration export/import remaps prices to newly imported provider IDs; providers skipped due to name conflicts keep their existing prices.
- Provider connectivity testing with live feedback
- Configuration import / export

### Connect — Integration

**Code Integration** — select a route and copy ready-to-use examples for:

| Protocol | Languages |
|---------|---------|
| OpenAI | Python · TypeScript · cURL |
| Anthropic | Python · TypeScript · cURL |
| Gemini | Python · TypeScript · cURL |

**CLI Integration** — one-click config sync for AI coding tools:

| Tool | Protocol |
|------|---------|
| Claude Code | Anthropic |
| Codex CLI | OpenAI Responses API |
| Gemini CLI | Gemini |
| OpenCode | OpenAI |

Nyro detects installed tools, generates the correct configuration for the selected route, and writes it with one click. Original configs are backed up automatically.

### Deployment

**Desktop App**

| Platform | Architecture |
|---|---|
| macOS | Apple Silicon (aarch64) · Intel (x64) |
| Windows | x64 · ARM64 |
| Linux | x86\_64 · aarch64 |

**Server Binary** — three run modes for everything from single-host to distributed deployments

| Mode | Flag | Purpose |
|---|---|---|
| `all` (default) | `--mode all` | Proxy + Admin API + embedded WebUI in a single process |
| `proxy` | `--mode proxy` | Data-plane only — no Admin API, no WebUI |
| `admin` | `--mode admin` | Control-plane only — Admin API (+ optional WebUI), no proxy |
| Standalone | `--config config.yaml` | YAML config, no DB, no Admin — minimal / edge deployments |

| Platform | Architecture | Access |
|---|---|---|
| macOS | x86\_64 · aarch64 | Proxy `:19530` · WebUI `http://localhost:19531` |
| Linux | x86\_64 · aarch64 | Proxy `:19530` · WebUI `http://localhost:19531` |
| Windows | x64 · ARM64 | Proxy `:19530` · WebUI `http://localhost:19531` |

---

## Installation

### Desktop App

**Shell Script**

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/Link2Link/nyro/master/scripts/install/install.sh | bash

# Windows (PowerShell)
irm https://raw.githubusercontent.com/Link2Link/nyro/master/scripts/install/install.ps1 | iex
```

**Manual Download**

Download the latest installer for your platform from [GitHub Releases](https://github.com/Link2Link/nyro/releases/latest).

> **macOS**: After manual install run `sudo xattr -rd com.apple.quarantine /Applications/Nyro.app`, or use the install script which handles this automatically.
>
> **Windows**: SmartScreen may show "Unknown publisher" — click **More info → Run anyway**.

### Server Binary

```bash
# Download
curl -LO https://github.com/Link2Link/nyro/releases/latest/download/nyro-server-linux-x86_64
chmod +x nyro-server-linux-x86_64

# Start (localhost only, no auth required) — default all mode
./nyro-server-linux-x86_64

# Start (LAN access, auth enforced: proxy Bearer key + admin token)
./nyro-server-linux-x86_64 \
  --lan \
  --proxy-auth-key YOUR_PROXY_KEY \
  --admin-token YOUR_ADMIN_TOKEN

# Proxy-only mode — binds :19530 only, no Admin API, no WebUI
./nyro-server-linux-x86_64 --mode proxy

# Admin-only mode — binds :19531 only (for internal systems to consume the control plane)
./nyro-server-linux-x86_64 \
  --mode admin \
  --admin-host 0.0.0.0 \
  --admin-token YOUR_ADMIN_TOKEN

# Standalone mode (YAML config, no DB/Admin/WebUI; --mode is ignored)
./nyro-server-linux-x86_64 --config config.yaml
```

For LAN access, point other devices at `http://<host-ip>:19530` with `Authorization: Bearer YOUR_PROXY_KEY`. See the "LAN access" section in the [Server docs](docs/server/README.md).

Available server binaries: `linux-x86_64`, `linux-aarch64`, `macos-x86_64`, `macos-aarch64`, `windows-x86_64.exe`, `windows-arm64.exe`

Open `http://localhost:19531` for the management UI. See [Server docs](docs/server/README.md) and [Standalone docs](docs/standalone/README.md) for full configuration reference.

### Local Docker Image

On a Linux amd64 or arm64 host, build the server and package the resulting binary into a local Docker image and archive:

```bash
make server
./make-docker.sh
docker compose up -d
```

`make-docker.sh` builds the `nyro:mine` image, exports it as `nyro-mine.tar`, and creates a private `.env` with random `NYRO_ADMIN_TOKEN` and `NYRO_PROXY_AUTH_KEY` when one does not already exist (existing `.env` files get the missing key appended). The Docker ports are published on `0.0.0.0`, so the proxy data plane requires Bearer auth: clients must send `Authorization: Bearer <NYRO_PROXY_AUTH_KEY>`. Open `http://localhost:19531` and sign in with the token stored in `.env`.

To import the archive on another compatible Linux host, place `compose.yaml` beside it, create a `.env` containing strong `NYRO_ADMIN_TOKEN` and `NYRO_PROXY_AUTH_KEY` values (or run `./make-docker.sh` to generate them), and run `docker load -i nyro-mine.tar` before `docker compose up -d`. The archive contains a native binary and therefore must be loaded on the same CPU architecture on which it was built.

### SQL Storage Backends

Default behavior: local SQLite under `--data-dir`. To use PostgreSQL or MySQL:

```bash
# PostgreSQL
./nyro-server-linux-x86_64 \
  --storage-backend postgres \
  --postgres-dsn "postgres://user:pass@host:5432/db"

# MySQL
./nyro-server-linux-x86_64 \
  --storage-backend mysql \
  --mysql-dsn "mysql://user:pass@host:3306/db"
```

Or via environment variable:

```bash
export NYRO_POSTGRES_DSN="postgres://user:pass@host:5432/db"
./nyro-server-linux-x86_64 --storage-backend postgres

# or
export NYRO_MYSQL_DSN="mysql://user:pass@host:3306/db"
./nyro-server-linux-x86_64 --storage-backend mysql
```

### Multi-replica Production Deployment

When running multiple `nyro-server` replicas behind a load balancer, all replicas **must** share the same database and the same admin token:

| Requirement | Detail |
|---|---|
| Shared DB | Use `--storage-backend postgres` or `mysql` pointing to the same DSN across all replicas. SQLite cannot be shared. |
| Unified admin token | Set the same `--admin-token` / `NYRO_ADMIN_TOKEN` on every replica. |
| Config sync | Replicas poll the shared DB for config changes every `--config-poll-interval` seconds (default: 3 s). Route/model/provider changes propagate within one poll interval. |
| OAuth interactive flows | The OAuth authorization callback must reach the **same replica** that initiated the session. Configure sticky sessions (session affinity) on your load balancer for the admin port (`19531`). |

**Health probes:**

| Endpoint | Port | Use |
|---|---|---|
| `GET /healthz` | proxy + admin | Liveness — always `200` |
| `GET /readyz` | proxy + admin | Readiness — `200` if DB reachable, `503` otherwise |

**Key environment variables:**

```bash
NYRO_MODE=all                      # all (default) | proxy | admin
NYRO_ADMIN_TOKEN=<secret>          # Required when admin host is not loopback
NYRO_STORAGE_BACKEND=postgres      # postgres | mysql | sqlite (default)
NYRO_POSTGRES_DSN=postgres://...   # Required when backend=postgres
NYRO_MYSQL_DSN=mysql://...         # Required when backend=mysql
NYRO_CONFIG_POLL_INTERVAL=3        # Seconds between config epoch polls (default: 3, 0=disabled)
NYRO_WEBUI_DIR=/path/to/dist       # Serve WebUI from external directory instead of embedded assets (admin/all modes only)
```

## Quick Start

**1. Add a Provider**

Go to **Providers → New**. Enter your provider's Base URL and API key. Nyro auto-detects the protocol from the URL.

**2. Create a Route**

Go to **Routes → New**. Set a virtual model name (e.g. `gpt-4o`), select your provider and target model. Enable access control if needed.

**3. Point your client at Nyro**

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:19530/v1",
    api_key="your-proxy-key"  # or "no-auth" if access control is off
)

response = client.chat.completions.create(
    model="gpt-4o",  # matches your virtual model name
    messages=[{"role": "user", "content": "Hello"}]
)
```

**4. Sync your AI tools (optional)**

Go to **Connect**, select a route, and click **Sync** next to Claude Code, Codex, Gemini CLI, or OpenCode. Nyro writes the correct config automatically.

---

## License

```
Copyright 2026 The Nyro Authors

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
```

See the full license text in [LICENSE](LICENSE).
