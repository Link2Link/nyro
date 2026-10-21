# Kiro Provider Account Pool

[中文说明](./kiro-account-pool_CN.md) · [Devin account pool](./devin-account-pool.md)

## Scope and setup

Kiro OAuth Providers can manage several independently authorized accounts behind one externally visible Provider. Existing single-account Providers remain usable; creating a new Provider still uses the existing Kiro login/import flow.

In **Providers → Kiro → Manage accounts** (or the expanded Provider settings), save **Pool scheduling settings**, then select **Add account**. Available authentication methods remain Google, GitHub, AWS Builder ID, IAM Identity Center, and credential import. Import supports the existing Kiro IDE/CLI credential formats and static API keys. Browser/device login options and proxy settings are retained.

Accounts have their own immutable ID, alias, priority, enabled flag, credential generation, usage observation and scheduling state. Smaller priority values are preferred. Reauthorization/import targets one account and expected version; removing an account clears only that account's secrets and retains a disconnected tombstone to fence delayed callbacks. Disconnected rows are not reusable: add a new account instead. A known profile identity must match during reauthorization.

## Scheduling and safe failover

The implementation reuses Devin's bounded account registry, shared management endpoints and existing credential storage, not its plan classification:

- Default policy: round-robin, session affinity enabled, affinity TTL 3,600 seconds, at most three distinct accounts per request, paid fallback disabled.
- `fill_first` is also available. Within the best cost class and priority group, eligible session bindings take precedence over round-robin.
- Included allowance is preferred, then unknown usage, then explicitly permitted paid overage. Disabled, disconnected, auth-rejected and hard-quota-blocked accounts are excluded. Model cooldowns and circuits are account/generation scoped.
- Paid fallback requires **both** `reserve_policy = fallback` and the upstream account's `overage_enabled` flag. This controls selection, not upstream billing: a request can exhaust its remaining included allowance mid-generation.
- Each account's cost class is captured at admission and revalidated before submission. If a concurrent usage observation moves the selected account to a worse class (for example included allowance to paid overage), it is not submitted; selection ranks again so an included sibling goes first.
- Account-specific credentials, region, profile ARN and OIDC registration travel together as an immutable request snapshot. No global current token is mutated.
- Explicit authentication/billing refusal or a recognized structured AWS refusal can fail over before execution. A bare proxy 403/429/503, ambiguous timeout after submission, or an exception after HTTP acceptance is not replay authorization. Stream failures still update the affected account's health without replaying delivered output or buffered tools.
- Opaque account-bound continuation is not moved between accounts. The shared request deadline/cancellation and eight-generation request budget still apply.

Authentication rejection requires reauthorization. Quota rejection remains blocked until a newer complete observation confirms usable included allowance or explicitly permitted overage. A stale generation's failure cannot invalidate a replacement credential.

## Token refresh

The existing social and AWS OIDC refresh protocols remain unchanged. Background refresh and request-time refresh operate on the exact `(provider_id, account_id, status_version)` rather than an ambiguous Provider credential. Tokens approaching expiry (five-minute margin) are refreshed before request admission; static API keys do not need refresh.

A per-account singleflight lock and storage compare-and-swap coordinate refresh publication. Refresh runs in a detached task, so a client cancellation or request deadline stops only the wait; it cannot abandon a claimed `refreshing` lease or lose a refresh token already rotated by the upstream. An account mid-refresh remains a pool member: requests join its lease instead of reporting an empty pool. Account aliases, priorities, enablement and latest quota state are preserved. Concurrent reauthorization, disablement or disconnect fences delayed refresh completion. One account's refresh failure does not overwrite or prevent selection of healthy siblings. Explicit upstream auth rejection is not silently cleared by a read-only usage query or routine refresh.

## Actual Kiro usage, not Devin units

Each account exposes Kiro's detected plan and resource entries with actual `used`, `limit`, and reset/expiry timestamps. Base allowance, active bonuses and active trial grants are additive **within the same resource**. Different resources are independent constraints. An explicit zero limit is exhausted; missing usage is unknown, not zero-used.

Bonus/trial expiry never counts as a replenishing reset. Persisted grants are excluded from scheduling after their expiry even without another successful query. Failed or incomplete refreshes preserve last-good observations and mark them stale; a partial response missing a previously observed resource cannot erase a quota block.

The UI shows pool account counts, a **pool total per resource**, and per-account resource usage. The pool total sums each enabled, non-disconnected account's actual pooled `used` (capped at its limit) and `limit` for the same resource, and shows the resulting used percentage, remaining percentage, coverage (accounts with observations / members), stale contributors, and the earliest account reset (marked aligned when every contributor shares it). Different resources are never merged with each other. Its steady-pace marker is display-only: each contributor's monthly window progress toward its own reset, weighted by that account's allowance; it is hidden if any contributor lacks a reset. The UI does not invent daily/weekly windows, Pro-equivalent units, or a shared reset. This total is computed in the WebUI only; scheduling stays per-account, and multi-account provider usage from the API still includes account details rather than a synthetic aggregate quota tier. Connection tests check enabled accounts independently, are non-generating, and succeed if any account passes; model suggestions are not a guarantee of account entitlement.

## APIs and persistence

Existing account APIs support both Devin and Kiro:

- Read/configure the Provider account pool.
- Initialize an account-bound login session (optional typed `kiro` login options).
- Bind the completed session, update label/priority/enablement using expected version, disconnect, and refresh one/all account usage.

Public account DTOs add optional `kiro_usage`; Kiro pool views have `kind = "kiro"`, no Devin daily/weekly windows, and never include tokens, refresh tokens or OIDC client secrets. HTTP and desktop adapters delegate to the core admin service.

No database schema migration is introduced. Existing `provider_oauth_credentials` multi-account rows and settings store are reused across memory, SQLite, PostgreSQL and MySQL. As with Devin, use one active gateway instance per account-pool database; process-local session/affinity coordination is not distributed scheduling.

## Verification and operational limits

Focused regressions cover account selection, actual quotas and expiring grants, account-scoped failures, refresh isolation and stale completion fences, login/import targeting, management DTO redaction, and frontend controls. Real-account browser login, live upstream generation, and deployment are separate acceptance steps. Tests must not modify production databases; optional PostgreSQL/MySQL suites require dedicated disposable databases.
