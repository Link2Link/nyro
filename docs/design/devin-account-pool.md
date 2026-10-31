# Devin Provider Account Pool

Status: implemented in the working tree; deployment and real-account acceptance are separate steps. [中文说明](./devin-account-pool_CN.md) · [Domain context](../../CONTEXT.md)

## 1. Mixed accounts, automatic classification

One Provider is one externally visible routing destination. Its accounts may mix **Pro, Team Full Seat and Max**; creating the Provider or adding an account does not require selecting a plan.

After an authenticated `GetUserStatus` succeeds:

- Trim and case-normalize the plan name. `max` and `devin max` mean Max.
- Every other label, including Pro, Team Full Seat, blank or absent labels, uses the Pro-equivalent class.
- A failed/missing status observation is **not** a successful observation with a missing name: it remains unclassified until a successful query.
- Max has no daily quota. Its weekly capacity is **10 Pro-equivalent weekly units**.
- Non-Max accounts have **1 Pro-equivalent daily unit and 1 Pro-equivalent weekly unit**.

These conversion factors are the product rule for this pool, not absolute token/ACU limits returned by the upstream. Usage remains marked `estimated`; exact-token duplicates are rejected, but a new identity RPC does not independently audit whether distinct tokens share one subscription.

```text
Client -> existing model/backend routing -> Provider
       -> eligible account + cost class + priority + affinity/round robin
       -> immutable account credential snapshot -> Devin native transport
       -> account-specific outcomes and existing request diagnostics
```

Model bindings, Provider weight, official origin, protocol and proxy policy stay shared. Do not create hidden Providers, place OAuth session tokens into the API-key pool, or mutate a global "current token" for concurrent requests.

## 2. Capacity and quota aggregation

Use **Pro-equivalent units**, separately for each window:

| Account class | Daily capacity | Weekly capacity |
| --- | ---: | ---: |
| Pro / Team Full Seat / non-Max | 1 | 1 |
| Max | Not applicable; excluded | 10 |

For one window, `c_i` is the account's capacity and `r_i` its reported remaining percentage:

```text
total_units       = sum(c_i)
remaining_units   = sum(c_i * r_i / 100)
used_units        = total_units - remaining_units
remaining_percent = 100 * remaining_units / total_units
used_percent      = 100 - remaining_percent
```

Do not average mixed-plan percentages without weights. Daily excludes Max entirely; weekly includes both classes. Units are not calendar-day consumption or token counts, and daily units cannot be exchanged for weekly units.

### Example

| Account | Daily remaining | Weekly remaining | Weekly capacity |
| --- | ---: | ---: | ---: |
| Pro A | 60% | 20% | 1 |
| Team Full Seat B | 30% | 60% | 1 |
| Max C | — | 50% | 10 |

- Daily: total 2 units, remaining `0.6 + 0.3 = 0.9`, remaining **45%**, used **55%**.
- Weekly: total 12 units, remaining `0.2 + 0.6 + 10 × 0.5 = 5.8`, remaining **48.33%**, used **51.67%**.
- Weekly observation coverage is **3/3 accounts**, not 3/12. Capacity units and account count are different quantities.
- An all-Max pool omits the daily window. Three Max accounts total 30 Pro-equivalent weekly units.

### Membership, unknown data and resets

- Enabled, non-disconnected accounts participate in accounting, including exhausted, cooling and authentication-failed accounts. Temporary dispatch failure does not silently shrink the denominator.
- Disabled/removed/disconnected accounts are excluded and reported separately; changing membership changes both numerator and capacity, not upstream usage.
- A known account class with missing usage still contributes its capacity; its complete aggregate percentage is null until that window is known.
- An unclassified member prevents claiming complete capacity: `capacity_complete = false`, `total_units` is only the classified capacity subtotal, and full remaining/used totals and percentages are null. Do not infer Pro before a successful status.
- `total_accounts` is the applicable-account coverage/reset denominator; for daily it excludes known Max members but includes unclassified placeholders. `known_accounts` counts usable observations, not capacity units.
- Failed refresh preserves last-good data and marks it stale. A newer successful status can change an account's Pro/Max class and automatically recompute capacity; it does not quarantine an account because of a previous Provider plan choice.
- For each window retain individual resets. Expose `next_reset_at`, `next_reset_accounts`, and `reset_mode = aligned | staggered | unknown`. One account resetting does not mean the whole pool resets.
- Paid extra-usage balances remain separate from included quota units.

A pool can have positive aggregate daily and weekly remaining while all Pro accounts are cross-window blocked: A `(daily=100%, weekly=0%)`, B `(daily=0%, weekly=100%)`. Keep the truthful bars but show no included-quota account available. A healthy Max account can still serve this pool because it has no daily constraint.

## 3. Selection and replay safety

For each request:

1. Filter disabled/disconnected/unauthorized accounts, unclassified accounts, current model cooldowns/circuits and visited `(Provider, account, model)` tuples.
2. Prefer included quota, then unknown usage, then permitted paid reserve.
3. Honor configured account priority; within the preferred class/priority, reuse an eligible session binding.
4. Otherwise round-robin; `fill_first` is also available.
5. Revalidate policy and credential generation before submission. Re-select retry candidates from current state rather than reusing stale cost order.

Pro uses daily and weekly limits; Max uses weekly only. Missing required observations remain unknown rather than being labeled fully available. An explicit hard quota rejection remains blocked until a newer complete authoritative observation confirms recovery; partial responses and elapsed reset time are insufficient.

The 10× weekly capacity affects **quota aggregation**, not automatic 10× traffic weight. Existing outer Provider routing and inner round-robin remain unchanged. The outer usage strategy uses a real eligible representative account rather than synthetic averaged windows with a fabricated reset.

Default policy:

```json
{
  "account_pool": {
    "strategy": "round_robin",
    "session_affinity": true,
    "affinity_ttl_seconds": 3600,
    "max_account_attempts": 3,
    "reserve_policy": "disabled"
  }
}
```

The pool has no effective `plan` setting. HTTP/Tauri creation can omit policy to use these defaults. Existing `plan: pro|max` input is accepted only for backward compatibility and is ignored/omitted from canonical output.

### Affinity and safety

- Affinity is scoped by caller identity, Provider and explicit logical session. Prompt cache keys are not conversation IDs. Upstream session IDs include account identity.
- Binding/cooldown/health are generation-aware; old-token failures cannot poison a new authorization. No-session requests use ordinary selection.
- Explicit structured pre-execution refusal can fail over before downstream commitment. Bare 403/429, ambiguous post-submit timeout, emitted text/tool output or opaque account-bound continuation do not justify replay.
- Every Connect Data frame is execution evidence, including buffered tools/usage and data in the same chunk as a later error trailer.
- Maximum distinct accounts defaults to 3; the shared request budget caps upstream generations at 8 and keeps one deadline. Open circuits are excluded without consuming an outbound attempt or hiding later healthy accounts.
- Quota recovery never clears independent model failures; authentication failure requires reauthorization. Repeated hard rejections advance a revision so stale probes cannot erase newer evidence.

## 4. Account lifecycle and persistence

[Core administration](../../crates/nyro-core/src/admin/account_pool.rs) owns create/add/reauthorize/enable/disable/disconnect/usage operations. Server HTTP and desktop commands are thin adapters; frontend DTOs never contain tokens.

The upgraded `provider_oauth_credentials` table is the credential source of truth: immutable local `account_id`, indexed `provider_id`, label, enablement, priority, credential version and account state. [Schema reference](../database/schema.md) documents the four storage contracts and migration details.

- Add appends; reauthorize targets an account ID + expected version; existing siblings remain unchanged.
- Server-owned login sessions bind Provider/account/version/fence, not a user-editable destination at callback completion. Result consumption, cancellation and expiry are enforced.
- A request holds `(Provider, account, version, token)` throughout execution.
- Disconnect scrubs secrets but retains a tombstone/version fence. Legacy bind/logout and concurrent empty-slot binds cannot resurrect an old authorization.
- Usage updates compare credential generation and exact prior state; repeated rejections change the state revision even when the error reason is unchanged.
- Successful classification normalizes old stored usage: historical Provider plan mismatches no longer block accounts. Authentication and hard quota evidence are preserved.
- Non-Devin OAuth drivers remain limited to a single account. Legacy Provider-only mutation and model-generation probe operations reject ambiguity instead of selecting or deleting random accounts. Provider connectivity is explicitly pool-aware, as described below.

## 5. UI and APIs

Creation shows automatic classification rather than a required plan dropdown. Settings expose strategy, affinity, attempt limit and paid-reserve policy only. Existing single-account Providers can enable a mixed pool without choosing Pro/Max; their legacy reauthorization path remains available before configuration.

```text
Devin · Mixed                 2 Pro-equivalent · 1 Max · 3 available
Daily   Used 55%     Remaining 45%       0.9 / 2 Pro-equivalent units
        Non-Max accounts only
Weekly  Used 51.67%  Remaining 48.33%    5.8 / 12 Pro-equivalent units
Next account reset: ... (1/3 accounts)  [Manage accounts]
```

Daily rows are absent for all-Max pools; Max account rows show daily as not applicable. Show each account's detected class and last successful observation. Incomplete capacity, stale data, actual readiness and paid-reserve use remain explicit; the frontend does not repeat quota aggregation logic.

Existing account endpoints and CAS payloads remain compatible. `PoolQuotaWindow` adds `total_accounts` and `capacity_complete`; `total_units` now means weighted Pro-equivalent units. Counts add `pro`, `max`, and `unclassified`; deprecated `plan_mismatch` stays zero for compatibility. New writes/exports omit the old Provider plan restriction.

### Consistent progress bars and steady-pace estimates

Account daily/weekly cells use the same used-percentage track, utilization colors and reference needle as other providers: green below 70%, orange from 70%, red from 90%. They explicitly label used and remaining percentages; Max daily is not applicable, and missing observations stay unknown rather than becoming a zero-used bar.

For each account/window, the visual steady-pace estimate is `clamp(100 * (1 - (reset - now) / window_duration), 0, 100)`. Daily uses 24 hours and weekly uses 7 days. The pool reference is `sum(capacity_i * account_pace_i) / sum(capacity_i)`, using daily Pro=1/Max excluded and weekly Pro=1/Max=10, with the same enabled/accounting members as the backend. It uses each account's reset, never the pool's earliest reset as a fictitious shared timestamp.

The marker refreshes on a local 30-second clock without querying upstream. Hover text explains the estimate and its capacity weighting. Missing/invalid resets, unknown account class, incomplete usage or inconsistent capacity suppress the pool needle; actual bars and backend percentages are not recalculated. Exhausted/auth-invalid accounts remain in the denominator, and elapsed reset time does not restore quota or imply readiness.

### Per-account retained gateway usage

Below each account's actions, a compact summary shows **All records** and **Current cycle**, with total tokens and estimated API-equivalent value for each. The cycle is anchored to each account's own upstream reset: Devin and Claude use the weekly reset minus 7 days; Kiro uses the monthly reset minus one calendar month (UTC, preserving time of day and clamping month-end dates). These are not browser-local calendar weeks/months or a shared pool reset. Kiro base-resource resets take precedence over the global fallback and must agree across resources; bonus/trial expiry is never used as a recurring reset. Totals include retained upstream attempts (including retries and failures) attributed by `route_decision.account.account_id` under the exact Provider. Account renames, priority changes, disablement and credential refresh do not reset attribution; old records without an account ID are not guessed or redistributed. Gross input already includes cache reads, so total tokens are input plus output only.

`GET /api/v1/providers/:id/accounts/usage-stats` (desktop command `get_provider_account_usage_stats`, with only `id`) returns one entry per current account: `account_id`, `all_time`, nullable `period`, and nullable epoch-ms `period_start`/`period_end`. Each total contains `total_tokens`, `request_count`, `equivalent_costs` (native currency buckets), and `unpriced_request_count`. The backend derives the half-open cycle `[period_start, period_end)` from saved quota observations and caps log reads at the captured current time. Missing, invalid, ambiguous or expired resets leave the cycle fields null; the UI shows dashes and asks for a usage refresh, including when a cached cycle expires. It never rolls an old reset forward or falls back to a calendar week/month. The expanded management panel queries all accounts together and refreshes every minute; this endpoint does not query upstream or alter scheduling. The three SQL backends accumulate all-time and per-account cycle totals in one log scan before applying the shared pricing engine.

Prices reuse current manual overrides/catalogue pricing, not subscription charges or historical invoices. Unknown prices stay unavailable (partial estimates are marked); currencies are not silently added. These are **retained-log totals**, not a permanent ledger: log deletion/retention affects both windows, and absent upstream usage cannot be reconstructed. SQLite, PostgreSQL and MySQL use the existing logs without a schema migration; memory-only mode has no retained log usage.

### Provider connectivity testing

`test_provider` checks all enabled Devin accounts using the non-generating authenticated status RPC, with at most four concurrent checks and a 15-second timeout per account. A failed account does not prevent checking its siblings; any passing account makes Provider connectivity succeed. Disabled accounts are skipped, and no enabled accounts or all failed accounts produce a failed aggregate.

The result includes optional `accounts` rows containing only account ID, safe alias, success, latency, sanitized error, skip reason and observation timestamp. Rows preserve pool order. Credentials changed/disabled/disconnected during a check are reported as skipped, not as a current pass. Existing clients without this field remain compatible.

The WebUI logs each account's pass/failure/skip and a pool summary, retaining these details even if the subsequent catalog step fails. Connectivity testing does not alter account quota/auth failure state and is not proof of available quota or model entitlement. Devin model discovery is a static suggestion list, independent of account selection; explicit model-generation probes remain separate.

## 6. Compatibility, deployment and limits

- This mixed-plan revision changes policy/JSON semantics, not the SQL schema. Existing account IDs, credentials, model bindings and policy preferences are retained; old `plan` fields are ignored on read, and old snapshots are normalized without requiring a new login.
- The original multi-account schema upgrade still requires a real database backup and stopping old binaries. Configuration JSON does not back up OAuth secrets.
- **Use one Gateway instance per managed account-pool database.** Session/fence coordination and scheduling are process-local; multiple active account-management writers are not supported.
- Automatic paid reserve remains opt-in; this is not an upstream billing hard limit. A request can exhaust included quota and trigger upstream automatic overage.
- Per-account concurrency caps, distributed leases, weighted account selection, audited subscriber identity and a dedicated config-only pool copy are not part of this revision.
- Real Devin login/chat and deployment require separate acceptance. No production data is modified by local tests.

## 7. Verification targets

- Creation succeeds without a plan; old Pro/Max JSON loads canonically without restricting mixed membership.
- Pro, Team Full Seat and Max coexist; 2 Pro-equivalent + 1 Max produces weekly capacity 12 and the weighted example above.
- Max daily artifacts are removed; all-Max hides daily; unknown class/usage never produces false complete capacity.
- Refresh upgrades/downgrades automatically reweight without losing auth/quota fences; failed/partial observations preserve known hard blocks.
- Existing routing, priority, affinity, paid-reserve order, generation fencing, safe replay, cross-model fallback and legacy login tests remain valid.
- Core/server/desktop checks plus frontend type checking, focused tests and production build validate the public contract.
