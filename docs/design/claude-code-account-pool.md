# Claude Code OAuth account pool

[中文](<claude-code-account-pool_CN.md>)

## Scope and compatibility

A provider remains one routing target while its Claude OAuth accounts are independently managed and scheduled. This feature is enabled only for Anthropic identity, `channel = "claude-code"`, and `auth_mode = "oauth"`. The OAuth driver key is **`claude-code`**; the public account-pool kind is **`claude_code`**. Do not confuse the two spellings.

Ordinary Anthropic API-key providers, other OAuth channels, and provider creation keep their existing flows. An unconfigured Claude single account remains usable and can be re-authorized without enabling a pool. Save pool scheduling settings before adding another account. No new HTTP or desktop IPC command is required, and no new database table is needed for this UI.

## Management UI

The provider card shows backend account counts and readiness with a pool usage refresh button. **Manage accounts** and the provider edit panel expose:

- Add account through browser OAuth and authorization-code exchange.
- Re-authorize the exact account, with its stable account ID and status version captured before opening the browser.
- Edit a safe label and integer priority (lower first), enable/disable, or remove an account.
- Refresh an individual account or the entire pool.
- Round-robin / fill-first strategy, session affinity and TTL, and maximum account attempts.
- Per-account upstream quota windows and retained-log usage statistics.

Removing an account clears its credentials but keeps an internal disconnected record/version fence, preventing old OAuth callbacks from restoring it. Removed accounts are no longer listed in management (or its per-account statistics rows); retained request logs remain. If an older cached response contains a disconnected row, its mutations are disabled. Errors and conflicts trigger query invalidation so a subsequent action uses a fresh status version. Queries for account totals are shared by all rows and only mounted inside account management, not on collapsed cards.

Claude settings always submit `reserve_policy = "disabled"`; there is no paid-reserve switch. Upstream `extra_usage` data does not prove usable paid capacity, so it must not enable automatic paid fallback. This is **not a billing hard limit**: users who require no additional spending must also disable extra usage upstream.

## Usage contract and presentation

The shared public account DTO adds this optional field:

```ts
claude_usage?: {
  windows: Array<{
    name: string;
    used_percent: number;
    resets_at: string | null;
  }>;
} | null;
```

No plan or extra-usage field is required by this contract. Existing account status, scheduling, observation time, and stale flags remain authoritative.

Canonical windows are `five_hour`, `weekly_limit`, and `feature:<model-or-feature>:weekly_limit` (for example Sonnet, Opus, OAuth apps, or a model-scoped limit). The backend normalizes the actual upstream buckets; the UI renders every returned window independently, including model-specific windows. Missing windows are not invented. Missing usage is **unknown**, not zero. Missing/invalid reset times are unknown; stale snapshots remain explicitly marked. Reaching a reset timestamp does not locally restore quota.

There is deliberately **no aggregate Claude pool percentage**. Different subscription plans have different capacities, and equal percentages do not imply equal remaining capacity. Never apply Devin Pro-equivalent units, the Devin Max multiplier, Kiro credits, or arithmetic averages of account percentages. The card reports counts/readiness; expand management for actual account windows. Existing per-window steady-pace markers are only visual time estimates, never scheduling inputs.

Retained usage statistics use the existing account usage-stats endpoint: all retained records plus the current calendar week (Monday start in the browser's local time). Tokens and equivalent API prices are log-derived estimates, not upstream quota or a permanent bill. Mixed currencies remain separate; deleted logs reduce totals; failed/retried attempts and missing reference prices follow the existing statistics semantics.

## API and session lifecycle

The WebUI calls the existing commands mapped in [backend.ts](<../../webui/src/lib/backend.ts>):

| Operation | HTTP surface |
|---|---|
| Read pool | `GET /api/v1/providers/:id/accounts` |
| Configure scheduling | `PUT /api/v1/providers/:id/account-pool` |
| Start account authorization | `POST /api/v1/providers/:id/accounts/auth-sessions` |
| Bind ready account session | `POST /api/v1/providers/:id/accounts/bind` |
| Update account | `PATCH /api/v1/providers/:id/accounts/:account_id` |
| Remove account | `DELETE /api/v1/providers/:id/accounts/:account_id` |
| Refresh account usage | `POST /api/v1/providers/:id/accounts/:account_id/usage/refresh` |
| Refresh pool usage | `POST /api/v1/providers/:id/account-pool/usage/refresh` |
| Retained account statistics | `GET /api/v1/providers/:id/accounts/usage-stats?period_start=...` |

Account initialization fixes either add-account intent (`account_id` / `expected_version` null), or targeted reauthorization intent (both populated). It returns the Claude browser authorization URL and `completion_mode = "manual_exchange"`. The user opens the page, signs into the intended account, and pastes a code (including `code#state`) or full callback URL. The UI calls `complete_oauth_session`, then consumes the ready session with `bind_provider_account`. **Binding sends only the session ID and optional label, not a newly selected account ID.** The backend owns target and CAS validation.

Unconfigured single-account reauthorization instead uses `init_oauth_session` with vendor `claude-code` and the provider proxy setting, then `bind_provider_oauth`. The UI checks that the provider is still unconfigured and unambiguous both before authorization and before binding; the backend must enforce this atomically as well.

The shared [OAuth session hook](<../../webui/src/lib/use-oauth-session.ts>) handles cancellation, expiry, generation ownership, abort signals, and stale callback rejection. Codes are cleared on submission/cancellation and never persisted in account DTOs or labels. Duplicate tokens are rejected by the backend; when a subject is known, reauthorizing a different identity is rejected rather than silently replacing the account identity.

## Backend scheduling and recovery boundaries

Like the existing account pools, an untargeted model-generation probe is not supported once account selection is ambiguous: it returns `AMBIGUOUS_ACCOUNT` rather than silently selecting an account. Non-generating connectivity checks and model discovery remain available.

The UI never derives eligibility or retries from percentage bars. The backend considers each account's five-hour, global weekly, and applicable model window. An exhausted Opus-only window must not block Sonnet.

Credentials are refreshed before requests within the five-minute expiry margin, with per-account version/CAS isolation; background refresh follows the same isolation. Structured upstream rejection permits safe account switching for `authentication_error` (401), `rate_limit_error` (429), `permission_error` (403), and `overloaded_error` (529 or 503). Bare HTTP status codes, transport timeouts, or streams already accepted must not cause replay. A credits-required rejection may try another account but must not rewrite account health or quota. Usage-query 401 requires reauthorization; usage-query 403 only marks the observation stale conservatively.

## Implementation and verification

- [Claude pool components](<../../webui/src/components/claude-account-pool.tsx>): card, management, actual-window presentation and retained stats.
- [Claude login component](<../../webui/src/components/claude-account-login.tsx>) and [session helpers](<../../webui/src/lib/claude-account-login.ts>): browser/code exchange and immutable session intent.
- [Shared scheduling form](<../../webui/src/components/devin-pool-settings.tsx>): Claude-specific reserve restrictions, with existing Devin/Kiro behavior preserved.
- [UI tests](<../../webui/src/components/claude-account-pool.test.tsx>) and [contract tests](<../../webui/src/lib/claude-account-login.test.ts>): identity gating, quota rendering, no aggregation, unknown/stale cases, bilingual labels, retained stats, legacy compatibility, target/CAS wiring and cancellation guards.

Run the related bundled Node tests using the repository's installed esbuild. The Vite icon glob is stubbed only in test output; application behavior is unchanged. Also run the existing Devin/Kiro account-pool, HTTP mapping, provider-usage, and usage-pace tests. Production type-check/build must not touch generated tracked output:

```sh
# From webui/ (esbuild path reflects the installed lockfile version)
node_modules/.pnpm/esbuild@0.27.3/node_modules/esbuild/bin/esbuild \
  src/lib/claude-account-login.test.ts src/components/claude-account-pool.test.tsx \
  --bundle --platform=node --format=esm --packages=external --alias:@=./src \
  --jsx=automatic --loader:.png=dataurl \
  --banner:js='import.meta.glob = () => ({});' \
  --outdir=node_modules/.tmp/claude-pool-tests --out-extension:.js=.mjs
for suite in node_modules/.tmp/claude-pool-tests/lib/*.test.mjs \
  node_modules/.tmp/claude-pool-tests/components/*.test.mjs; do
  node "$suite" || exit $?
done
npm run build -- --outDir node_modules/.tmp/claude-pool-build
```

These automated checks do not substitute for a real-account acceptance test. Browser OAuth, multiple real subscription plans, desktop IPC, and deployed streaming/retry behavior still require environment-specific acceptance; no live-account authorization is claimed by these UI tests.
