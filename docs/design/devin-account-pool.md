# Devin Provider Account Pool

Status: initial implementation is present in the working tree; not deployed. Chinese companion: [中文方案](./devin-account-pool_CN.md). Domain terms: [Domain Context](../../CONTEXT.md).

## Implemented release boundary

- Core account storage/migrations, Pro/Max validation, account management, usage aggregation, round-robin/fill-first with priority and session affinity, account-scoped feedback, safe failover, HTTP/Tauri commands and WebUI are implemented. Remaining sections retain the architectural rationale; their pre-change code references describe the baseline, not the current state of those lines.
- **Run one Gateway instance against a database containing managed Devin pools.** Session claims, plan-change/logout coordination and scheduling are process-local. Multiple active writers sharing PostgreSQL/MySQL are not supported for pool administration; account CAS alone does not make plan/fence publication atomic across processes. This is separate from the requirement not to mix old/new binary versions.
- Usage is explicitly marked estimated: plan/window compatibility and exact credential duplicates are checked, but independent subscriber identity and absolute entitlement capacity are not audited by a new identity RPC. No cross-Provider duplicate warning has been added.
- Per-account concurrency limits, distributed leases and weighted account selection remain future options; the initial release has no configurable per-account concurrency cap. Existing model health permits provide circuit half-open control.
- Copy/probe/test through ambiguous legacy Provider-only operations is rejected rather than choosing a random account. Static model discovery works for configured pools. A dedicated config-only pool-copy action is not included.
- Configuration export/import preserves the pool policy and selected plan, not OAuth credentials or account descriptor lists. Database backup is required to preserve logins. Legacy single-account reauthorization remains available before pool configuration.
- Verification uses in-process/mock upstreams and disposable databases. No real Devin login/chat was performed, no production database was migrated, and no running application was replaced.

## 1. Decision and scope

Keep one Provider as one model-routing destination. Add multiple independently authorized Provider Accounts beneath it. Do not create hidden Providers, put OAuth session tokens into `Provider.keys`, or change a Provider's active credential globally for each request.

```text
Client -> existing model/backend selection -> Provider
       -> account eligibility + affinity + selection
       -> immutable account credential snapshot -> Devin native transport
       -> account-scoped feedback + existing request diagnostics
```

Provider identity, model bindings, routing weight, protocol, official origin and proxy policy remain shared. Authorization, enablement, quota observations, authentication failure and account/model cooldown belong to individual accounts. The first implementation enables multiple accounts only for Devin; other OAuth drivers remain limited to one account.

**Revised requirement: homogeneous plan pools.** Every Devin Provider explicitly selects `account_pool.plan = pro | max`. Pro pools expose one aggregate daily quota and one aggregate weekly quota; Max pools expose only one aggregate weekly quota. Pool members must have the selected plan and equal included allowances for each applicable window. Under this constraint, account quota units can be added and normalized into a real pool percentage; this supersedes the earlier counts-only display proposal. These are the supported product assumptions for this feature, not a claim that the current parser hardcodes those plan names.

Non-goals: automatic account creation, bypassing upstream restrictions, a cross-Provider account marketplace, distributed global concurrency enforcement, account-specific proxy overrides, and changing Devin's protocol conversion contract.

## 2. Current implementation constraints

| Evidence | Consequence |
| --- | --- |
| [OAuthCredential](../../crates/nyro-core/src/db/models.rs#L368-L399) and [OAuthCredentialStore](../../crates/nyro-core/src/storage/traits.rs#L309-L373) are keyed by `provider_id`. | A second login currently replaces the first credential. |
| [OAuth binding](../../crates/nyro-core/src/admin/oauth.rs#L991-L1079) publishes a credential and uses versioned compensation. | Account binding must retain generation fences and improve concurrent replacement semantics. |
| [API-key selection](../../crates/nyro-core/src/provider/key_pool.rs#L29-L54) rejects native OAuth protocol pins. | Reuse selection concepts, not API-key rows or token-in-key workarounds. |
| [Devin auth](../../crates/nyro-core/src/auth/drivers/devin.rs#L50-L57) has no identity/refresh/expiry fields; [refresh](../../crates/nyro-core/src/auth/drivers/devin.rs#L254-L261) is unsupported. | Local account IDs are required. Refreshing usage is not refreshing authorization. Tokens may still be revoked. |
| [Quota registry](../../crates/nyro-core/src/router/quota.rs#L54-L69) is Provider-scoped and [usage refresh](../../crates/nyro-core/src/admin/usage.rs#L2332-L2391) observes one credential. | Quota, refresh locks and cached usage need account keys; Provider state becomes an aggregate. |
| [Devin quota parsing](../../crates/nyro-core/src/provider/devin/usage.rs#L11-L22) understands applicable windows, omitted zero values and paid reserve. | Preserve those semantics; do not require a nonexistent daily or weekly window. |
| [Native retry](../../crates/nyro-core/src/proxy/dispatcher/mod.rs#L1118-L1179) tracks submission/rejection, while [IR response handling](../../crates/nyro-core/src/proxy/dispatcher/ir_response.rs#L126-L166) normally forbids replay. | No automatic replay merely because the client has not received output. |
| [Connect errors](../../crates/nyro-core/src/provider/devin/stream.rs#L421-L465) redact diagnostics into a generic error. | Extract a safe classification before redaction, without persisting raw messages. |
| [Usage routing](../../crates/nyro-core/src/router/selector.rs#L275-L329) gives each Provider one group. | Account count must not silently multiply the Provider's external weight. |

## 3. Persistence and ownership

### One source of truth

Upgrade `provider_oauth_credentials` in place to account-addressed records, rather than introduce a Devin-only credential table alongside it:

```text
provider_oauth_credentials
  account_id                 primary key, immutable local ID
  provider_id                foreign key, non-unique indexed membership
  label                      operator-facing alias
  is_enabled                 operator intent; not a health flag
  priority                   lower value first, default 0
  identity_key?              authenticated upstream identity when available
  credential_fingerprint     internal digest; never returned to clients
  ... existing driver, token, metadata, status, status_version fields
```

`status/status_version` continue to describe authorization lifecycle. Do not overload them with quota exhaustion, network failure or operator disablement. Keep account/model cooldown and quota observations as separate scheduling state.

Account IDs, not e-mail, username or token fingerprints, address runtime operations and logs. A reauthorization keeps the ID when it is the same account. When verified upstream identity is available, deduplicate on driver + subject + relevant organization/tenant scope. Current Devin parsing deliberately omits personal fields; identity enrichment is a bounded, authenticated addition, not an assumption about existing `subject_id` data. Token fingerprinting catches exact token duplicates only. Do not claim it detects all logins to the same subscriber.

Reject exact duplicate additions with a link to the existing account. For a verified identity match with a new token, offer explicit reauthorization of that account rather than adding another copy. If identity is unavailable, display that fact and require an operator label. Warn about detected duplicates across Providers without silently merging ownership.

### Plan configuration and validation

- New Devin Providers require an explicit `pro` or `max` selection. Do not silently default an unknown subscription to Pro. A populated pool cannot change plan with an ordinary edit; remove/move its members or use an explicit atomic reclassification flow with revalidation.
- Verify the authenticated status response's plan name and applicable window shape on add/reauthorization, and recheck during usage refresh. Pro expects daily + weekly; Max expects weekly and no applicable daily window. Max's missing daily window is `not_applicable`, never 0% or 100%.
- The current parser reads `plan_name`, hide flags and applicable windows rather than matching Pro/Max strings. Add an explicit allowlisted normalization/validation layer; unknown labels or incompatible entitlements are `plan_unverified`/`plan_mismatch`, not silently coerced. Preserve the existing upstream wire semantics: operator selection must not erase a real unexpected daily restriction or fabricate a missing window.
- Reject a newly added wrong-plan account. If an existing member upgrades/downgrades, quarantine it from dispatch and accounting with a visible warning; do not automatically switch the Provider's plan. A failed query alone does not establish a plan change and preserves the last confirmed membership.
- Count independent upstream entitlements, not login tokens. Known duplicate identities/tokens cannot multiply capacity. When independent identity or equal-capacity verification is unavailable, label the aggregate as estimated rather than claiming audited capacity; supported data should be used to strengthen identity checks. Different entitlements within a nominal plan require a separate pool until a weighted-capacity model is explicitly supported.

### Store contract

Add account-aware `list(provider_id)`, `get(provider_id, account_id)`, `create`, version-checked replacement, enable/disable, disconnect and refresh operations. All public account operations verify parent Provider membership.

- Account replacement and its previous snapshot must be obtained transactionally, or require `expected_version`; do not read an old snapshot and later overwrite an intervening bind during compensation.
- Preserve scrubbed disconnected tombstones and monotonically advancing generations. A stale usage refresh, request result or login callback cannot revive a disconnected/replaced credential.
- A request holds a snapshot of `{provider_id, account_id, credential_version, token}`. Updating or disabling the account affects new attempts; an already submitted request is allowed to finish.
- Delayed observations must match credential generation and have an observation sequence/time newer than the state they intend to supersede. A pre-exhaustion status request completing late must not clear a newer quota rejection.
- Reauthorization clears authentication failure, not automatically quota exhaustion. Query authoritative usage before releasing a known quota block for the same upstream identity.

Retain a strict singleton compatibility facade for existing drivers. Provider-only read/write/logout operations may act only on an unambiguous single account; disabled/error accounts still count when determining ambiguity. Return `AMBIGUOUS_ACCOUNT` (HTTP 409) before consuming a login session. Never silently pick the first account of a pool. Enforce the non-Devin one-account limit transactionally, including concurrent creates.

### Scheduling state across restart

Persist known account authentication failures and hard quota blocks, their reason/window/reset hint, last authoritative observation and credential generation. Keep selection cursors, in-flight counters and short transient cooldowns in memory. Write durable state on meaningful transitions, not on every request.

On restart, known exhausted accounts stay blocked until confirmed recovery. Stale quota observations are refreshed; an expired reset hint alone does not mark an account healthy. This avoids a restart sending one failed request through every exhausted account.

## 4. Login and management lifecycle

Expose distinct actions:

1. **Add account**: create a login session with intent `add_account` and a fixed target Provider.
2. **Reauthorize account**: create a session bound to Provider + account + expected credential generation.
3. **Refresh usage**: call `GetUserStatus`; never invoke the unsupported Devin token-refresh driver.
4. **Disable / enable**: change operator intent without clearing credentials or quota history.
5. **Disconnect / remove**: scrub credentials and fence pending callbacks; no effect on sibling accounts.

Reuse manual PKCE, callback-state checks, token paste and `prompt=select_account`. Bind session intent server-side, consume each successful login result once, and reject callback replay or an account deleted/replaced during login. A bulk disconnect also advances a Provider-level fence/cancels pending sessions, so an old unfinished add cannot repopulate the pool afterward.

After authorization, probe status, verify the Provider's selected plan/window contract, and obtain identity when supported. Invalid authorization or a mismatched/unverified plan must not enter routing. A transient probe failure is displayed as unverified, not quota-exhausted; retry verification without silently reporting success. Migrated, previously connected accounts retain their authorization state, with stale/unknown usage refreshed in the background.

Reauthorization that demonstrably returns a different subscriber must be rejected as an account mismatch or explicitly completed as a new account. It must not inherit another subscriber's quota history under the same account ID.

## 5. Two-level selection

### Outer level: existing Provider routing

Model/backend selection, priorities, explicit fallback backends and Provider weights remain intact. Pool membership does not duplicate model bindings.

Provider readiness is an aggregate of account candidates, not the status of the last refreshed account. Distinguish `no_account`, `disabled`, `auth_required`, `quota_exhausted`, `temporarily_unavailable` and `eligible`; do not label every empty pool as a quota failure. Per-model circuit/capability checks remain model-sensitive.

For the existing `usage` strategy:

- At least one eligible included-quota account => Provider can compete in the normal pool.
- No such account but an eligible unknown-usage account => unknown-usage fallback tier.
- Only eligible paid-reserve accounts => reserve tier.
- No eligible accounts => unavailable, with aggregated reasons and earliest next check.
- Represent a scored Provider by the maximum eligible account routing score, including the existing `dynamic_weight(rate) * window_boost(window)`, not a sum of account percentages or a multiplication by account count.

This representative score is a routing hint, distinct from the homogeneous-pool quota percentage defined in section 8. Do not feed aggregate daily/weekly percentages with a fabricated common reset into the existing single-account scorer or use them as the sole availability gate. Preserve existing explicit fallback backend ordering. Reserve-last currently belongs to `usage` ordering; do not silently reorder every other routing strategy globally.

### Inner level: account selection

Recommended first-release policy:

```text
eligible authorization + operator enabled + verified matching plan + model allowed
  -> exclude known quota blocks, live cooldowns, open circuits and attempted accounts
  -> prefer included quota; then unknown usage; then permitted reserve
  -> lowest configured priority in that class
  -> valid session binding within that preferred class
  -> round-robin among equal-priority candidates
  -> atomically reserve a slot and revalidate account generation
```

Default strategy: `round_robin`. Optional `fill_first` uses stable priority/insertion ordering. A quota-score-weighted strategy can be added later using existing scoring functions; it is not required to deliver multi-account support.

An optional per-account concurrent-request cap can be left unset for compatibility. If enabled, slot acquisition must be atomic; exhaustion is a neutral local-capacity condition, not an upstream failure. A lease survives until full stream completion/cancellation, not just receipt of response headers.

Selection configuration belongs to the Provider, for example:

```json
{
  "account_pool": {
    "plan": "pro",
    "strategy": "round_robin",
    "session_affinity": true,
    "affinity_ttl_seconds": 3600,
    "max_account_attempts": 3,
    "reserve_policy": "disabled"
  }
}
```

These are proposed fields, not existing API options. New pools default to no intentional paid-reserve fallback; migrated Providers explicitly retain their existing reserve fallback behavior. The operator may choose `fallback` instead of `disabled`.

**Billing limitation:** this is a scheduling policy, not a hard upstream spending cap. A cached included balance can run out during a request and the upstream may automatically charge reserve. A strict no-spend guarantee requires upstream overage to be disabled; the UI must not promise otherwise.

## 6. Session affinity

Use a bounded TTL/LRU binding keyed by `(client principal/API-key ID, provider_id, logical_session_id)`. Resolve session identity once before account selection and reuse it for Devin request encoding. Do not include raw prompts or tokens in the cache key, and do not bind all unidentified traffic to a single pseudo-session.

The current [Devin session encoder](../../crates/nyro-core/src/provider/devin/request.rs#L638-L675) derives IDs independently from body/header fields. Extract that resolution boundary; qualify upstream session/cascade IDs by account identity, so switching accounts does not reuse another account's upstream session identifier.

- A healthy bound account wins only within the permitted cost/priority class.
- A known exhausted, disabled or unauthorized account loses the binding.
- Temporary slot contention may choose another account for the request without permanently rebinding the conversation.
- Publish a new binding only after a successful selection/accepted attempt, with concurrency-safe first-bind semantics; concurrent first turns should not accidentally create competing sticky bindings.
- No explicit session => ordinary account selection; no claim of guaranteed multi-turn affinity.
- Follow-ups that depend on opaque upstream continuation state must not switch accounts unless the request is reconstructible from full supported history. Do not strip signatures or invent missing history to force failover.

The first release's binding/counters are process-local. Shared database storage does not imply cluster-wide affinity or concurrency limits; cross-instance coordination needs a separate design if multi-replica deployment requires it.

## 7. Errors, cooldowns and replay safety

Introduce a safe typed account outcome before rendering client errors:

```text
scope: request | account | account_model | provider_model | transport
reason: auth_invalid | daily_quota | weekly_quota | rate_limited |
        model_forbidden | capacity | request_invalid | transport_unknown
retry_after?
rejection_evidence: not_submitted | explicitly_rejected | execution_unknown
```

Inspect bounded HTTP errors and Connect trailer diagnostics to derive these allowlisted fields; retain existing secret/request-text redaction. A bare 429 or 403 is insufficient to infer account-wide quota exhaustion or safe replay.

| Observation | State update | Next-account behavior |
| --- | --- | --- |
| Explicit invalid/revoked authorization | Account requires reauthorization | Allowed only for a definite rejection |
| Explicit daily/weekly quota exhaustion | Block whole account and request status refresh | Allowed only for a definite rejection |
| Generic rate limit or model-capacity refusal | Short bounded cooldown in the narrow known scope | Only when explicit pre-execution rejection is established |
| Model entitlement denial | Block account/model, not all models | Try another eligible account when safe |
| Request validation error | Do not penalize any account | Return error; no pool churn |
| Unknown network failure after submit | Transport diagnostic, not account invalidation | No transparent replay |
| Client cancellation or local slot exhaustion | Neutral | No account-health penalty |
| Failure after downstream output/tool delta | Record accurate failure and future scheduling state | Never splice another account's output into the response |

Preserve independent cooldown dimensions. Quota recovery must not erase an unrelated model circuit, and reauthorization must not silently forgive known quota exhaustion. Authentication failure is durable until reauthorization; transient rate limits use bounded exponential backoff with jitter and a single half-open probe.

Key account-model health by `(provider_id, account_id, protocol, actual_model)`. Common endpoint/transport failures may retain shared transport-level handling, rather than trying every account against an outage. Update health summaries too: one account's open circuit must not mark the whole Provider/model unavailable while another can serve it.

### Native-stream retry boundary

Do not weaken native safety globally. Account switching is allowed only when all are true:

1. No downstream response commitment/output prevents replacement.
2. No model/tool execution evidence has been observed.
3. The request was not submitted, or a provider-specific, tested rejection classification proves refusal rather than ambiguous execution.
4. The request can be reconstructed without account-bound upstream continuation state.
5. Attempts and the shared request deadline remain available.

An HTTP 200 followed by an error trailer is not automatically retryable. A strict, tested pre-payload auth/quota refusal can become explicit rejection evidence; otherwise retain `Never`. In particular, replace the current native branch's status-only 401/403 override with evidence-aware handling rather than copying it into account failover.

Maximum distinct accounts per selected Provider request: 3 by default. Add a request-wide outbound-generation budget shared across accounts, API keys, Provider targets and any same-target retry; proposed default 8 total upstream generations. Reuse one end-to-end deadline and visited `(provider, account, model)` set. These are new controls: the current attempt counter is diagnostic, not an enforced budget. Do not retry N accounts independently on each of M outer retries.

If the account pool is exhausted and replay remains safe, proceed to the next existing Provider target. If replay is unsafe, return the failure regardless of remaining healthy accounts.

## 8. Usage observation and recovery

Adapt the existing quota rules to account keys; do not build a second Devin-only quota parser or poller.

- Healthy accounts: refresh roughly every five minutes, with jitter.
- Accounts using paid reserve: refresh roughly every minute, retaining the existing fast-depletion intent.
- Known exhausted accounts: revalidate at the earlier of a plausible reset plus grace or a bounded early-recovery check (proposed five minutes), then reschedule on authoritative evidence.
- Missing reset hint: short bounded recheck/backoff; no permanent block and no automatic assumption of recovery.
- Auth-required or disabled accounts: no chat retries; background checks may stop until operator action.
- Query failures preserve the last authoritative observation. A timeout does not mean zero quota or recovered quota.

Use per-account singleflight plus a global bounded status-query concurrency limit. Manual refresh and background refresh share those locks. Native 429 handling must explicitly schedule an account usage refresh: it currently returns before the ordinary dispatch branch's refresh hook.

Only applicable windows participate in exhaustion/recovery. A quota-billed omitted zero is exhaustion; hidden/non-applicable/unlimited windows are not. Keep paid reserve separate from included quota. A live explicit upstream quota rejection overrides an older positive-balance snapshot until a newer authoritative result resolves it.

### Homogeneous-pool quota aggregation

For each applicable window, let `N` be the pool's accounting-member count and `r_i` each member's remaining percentage in `[0, 100]`. One full per-account allowance for that plan/window is one **Account Quota Unit**:

```text
total_units        = N
remaining_units    = sum(r_i / 100)
used_units         = N - remaining_units
remaining_percent  = 100 * remaining_units / N
used_percent       = 100 - remaining_percent
```

The unnormalized sum of percentage points has denominator `100 * N`; it is not a pool percentage on a 100% scale. Equal-capacity members make the arithmetic mean of their percentages mathematically equivalent to the normalized capacity sum. No token/ACU quantity is invented when upstream only reports percentages. This is a snapshot of the members' current quota windows, not a cumulative calendar-day/week usage counter.

Example: three Pro members have daily remaining `60%, 30%, 90%` and weekly remaining `20%, 50%, 80%`. The Provider exposes:

- Daily: used `40%`, remaining `60%`, remaining capacity `1.8 / 3` Pro daily units.
- Weekly: used `50%`, remaining `50%`, remaining capacity `1.5 / 3` Pro weekly units.
- For three Max members with the same weekly percentages: weekly only, with no daily bar/DTO window (`daily = null`, not `0%`).

**Accounting membership and completeness:**

- Count enabled, non-removed/non-disconnected members whose plan has been confirmed to match. Include exhausted members, short cooldowns, model circuits and members that now need reauthorization: a transient inability to route does not shrink the quota denominator. Keep authorization/scheduling counts separate.
- Disabled/removed/disconnected or positively mismatched members do not contribute to capacity; display the exclusion count/reason. Their removal changes both numerator and denominator, which is a pool-capacity change, not newly consumed/recovered quota.
- Failed refresh preserves a valid last-known observation for the same entitlement generation and marks it stale/estimated. Do not import snapshots across a plan or identity change. Never fill unknown remaining with zero or 100.
- With no snapshot for one of the `N` members, publish `known_accounts / N`, keep `total_units = N`, and leave complete aggregate percentages and sums null. Show `Updating (2/3 accounts)` and optionally a clearly labeled known subtotal. Do not divide by the known count and present it as the entire pool. With `N = 0`, show no active pool capacity, not 0% used/100% remaining.
- Return window `total_units`, `used_units`, `remaining_units`, `used_percent`, `remaining_percent`, observation coverage/staleness, and separate scheduling counts. Do not overload the old Provider quota registry with these display-only synthetic tiers; the account registry remains authoritative for routing.

**Quota display is not dispatch eligibility.** Two Pro accounts A `(daily=100%, weekly=0%)` and B `(daily=0%, weekly=100%)` yield aggregate daily/weekly remaining of 50%, but neither can serve an included-quota request. Show both honest bars plus `No included-quota account available` (and reserve-only status if applicable). Never combine A's daily allowance with B's weekly allowance or compute a fake schedulable percentage using `min(daily, weekly)`; the units have different denominators. Eligibility is still evaluated per account before Provider aggregation.

**Reset semantics:** accounts need not reset together even within one plan. Keep individual reset times and expose `next_reset_at` with the affected member count, plus `reset_mode = staggered | aligned | unknown`. A common reset is valid only when all included members report the same time; otherwise label it `Next account reset`, not `Pool resets`. Arrival of one reset increases only the corresponding member's quota after authoritative confirmation. Paid reserve stays separate from included units.

## 9. Admin API, UI and diagnostics

Add transport-agnostic core service methods, then thin server HTTP/Tauri IPC wrappers. Proposed route shapes:

```text
GET    /api/providers/{p}/accounts
POST   /api/providers/{p}/accounts/auth-sessions
POST   /api/providers/{p}/accounts/bind
PATCH  /api/providers/{p}/accounts/{a}
POST   /api/providers/{p}/accounts/{a}/reauthorize
POST   /api/providers/{p}/accounts/{a}/usage/refresh
DELETE /api/providers/{p}/accounts/{a}
```

The exact HTTP prefix should follow the existing server router. Add expected generations/idempotency where mutations need them. Reject cross-Provider account IDs. Summary/list responses expose only safe aliases, status, plan, quota and timestamps, never session tokens or raw metadata.

Provider configuration requires a `Plan: Pro | Max` selector. The Provider card uses the same aggregate progress-bar style as a single subscription, with a plan badge, capacity count, readiness and an Accounts action:

```text
Devin · Pro                                      3 accounts · 3 available
Daily quota   [used 40%]   Remaining 60%           1.8 / 3 account units
Weekly quota  [used 50%]   Remaining 50%           1.5 / 3 account units
Next account reset: 2h (1/3 accounts)             [Manage accounts]
```

For Max omit the daily row entirely. Keep quota bar direction consistent with existing used/remaining labeling. Show incomplete/stale-data and plan-mismatch warnings; readiness is not inferred from either aggregate bar. The expandable per-account table remains for diagnostics (daily column omitted for Max):

```text
Alias       Authorization   Scheduling       Daily   Weekly   Reserve    Actions
Personal A  Connected       Available        62%     31%      $0         Usage / Reauthorize / Disable
Personal B  Connected       Quota exhausted  0%      15%      $0         Usage / Disable
Team C      Needs login     Unavailable      —       —        —          Reauthorize / Remove
```

Show remaining percentages, actual reset times, last successful observation and staleness. Distinguish authorization from scheduling so a connected but exhausted account is understandable. All canonical UI labels/defaults are English with Chinese localization.

Each outbound attempt records safe `provider_id`, `account_id`, alias snapshot, selection reason, failure scope, account failover index and cooldown reason in existing diagnostics. Keep one final client result; multiple attempts must not be counted as additional client requests. Do not use e-mail/token as a metric label.

## 10. Compatibility and migration

1. Stop old server/desktop writers and back up the actual database. A configuration JSON export is not a credential backup.
2. Add account-aware schema and indexes transactionally; migrate each current OAuth row to one deterministic legacy account ID, including disconnected tombstones and their generations.
3. Keep Provider IDs, model/backend bindings and credentials unchanged. Give existing rows an English default alias such as `Default account`. Preserve existing reserve behavior through explicit policy migration. Infer the selected plan only from a confirmed compatible status; otherwise retain the legacy single-account path with a visible `Plan selection required` state and block pool expansion/unified capacity claims until the operator selects and validates Pro or Max. Do not guess Pro from missing data. Export/import the selected plan together with the pool policy.
4. Update every startup backfill from legacy Provider OAuth fields. Removing uniqueness on `provider_id` invalidates the old `INSERT OR IGNORE`/conflict-based idempotency. Use deterministic legacy IDs and a completed migration/existence check that includes tombstones; never resurrect an old secret after logout.
5. Switch runtime credential resolution, probe/model discovery, usage, copy and management to explicit account IDs. Account selection must not occur implicitly inside a management status query.
6. Existing single-account routes remain compatible. Multi-account calls without an account target return a clear ambiguity error; a distinct explicit bulk-disconnect action requires confirmation.
7. Configuration export/import carries pool policy and non-secret account descriptors, not OAuth login tokens. Existing exports may contain ordinary API-key secrets; do not describe the entire export as secret-free. Reimported accounts require reauthorization; no claim of credential restoration. Validate supported format versions rather than merely increasing a version field that older readers may ignore. Provider copy preserves current single-account behavior where required; copying a pool must explicitly choose configuration-only or credential cloning (configuration-only recommended) and warn about sharing the same upstream quota.
8. Update SQLite, PostgreSQL, MySQL and memory-store contracts. Update authoritative schema documentation and regenerate PostgreSQL/MySQL reference schema using dedicated disposable databases, per repository instructions.

This is a schema-changing release, not mixed-version rolling deployment. Old binaries must not write the new schema. Downgrading requires restoring a compatible backup; do not promise lossless old-version operation after multiple accounts are created.

## 11. Delivery plan and acceptance criteria

### Milestone A — account persistence and management

Migrate storage, enforce driver cardinality, add account-specific login/reauthorize/disconnect and the account panel. Keep existing single-account behavior operational. Include backup/import/copy semantics and redaction tests.

### Milestone B — runtime pool

Add explicit credential candidates, account-scoped quota/health and usage refresh, aggregate Provider readiness, round-robin/priority/affinity, generation snapshots, leases and budgets. Add safe Devin error classification and evidence-aware failover. Keep protocol encoding/decoding otherwise unchanged.

### Milestone C — observability and release hardening

Cover server and desktop, configure reserve policy, verify restart behavior and migrations, and expose account attempt diagnostics. Optional weighted account selection and distributed coordination are later extensions, not prerequisites.

Required scenarios:

- Adding B does not overwrite A or duplicate Provider/model records; exact duplicate binding is idempotently rejected.
- Concurrent adds respect non-Devin singleton limits; concurrent reauthorizations/logout cannot restore obsolete credentials.
- A exhausted/B healthy => B serves; all accounts exhausted => outer Provider fallback, not a busy loop.
- A unauthorized => only A needs login; independent B/model circuits survive unrelated recoveries.
- An early reset or top-up is recognized by fresh status; elapsed time, refresh failure or stale observations do not revive a blocked account.
- Included quota outranks reserve; disabled reserve is never deliberately selected; the UI explains upstream automatic-overage limitations.
- Same logical session stays on an eligible account; account switch changes upstream session namespace; missing history prevents unsafe failover.
- Proven HTTP/Connect refusals can switch before commitment; ambiguous submitted timeouts and post-output failures never replay.
- Pool-wide attempts/deadlines/concurrency remain bounded and cancelled streams release leases.
- Restart preserves hard blocks/tombstones; repeated migration does not add accounts or resurrect legacy secrets.
- Same-plan equal-capacity aggregation yields correct summed units and normalized percentages; Pro has daily + weekly and Max has weekly only.
- Mixed-plan addition is rejected; plan drift quarantines the member, and failed queries alone do not alter its confirmed plan.
- Exhaustion/auth failure does not silently shrink the denominator; disabled/removed membership changes are visible; incomplete observations never become a false complete percentage.
- Cross-window blocking can make an account pool unavailable while both aggregate bars remain positive; staggered resets are not presented as one pool reset.
- Duplicate upstream entitlements do not knowingly multiply capacity, and pool display aggregation does not by itself multiply outer Provider routing weight.

Validation for this initial implementation includes the core library suite, focused account/usage/native tests, real disposable PostgreSQL/MySQL and SQLite/memory storage contracts, server contract tests, core/server/desktop compilation, WebUI type checking/focused tests/targeted lint and a build to a temporary directory. The repository-wide WebUI lint still reports unrelated pre-existing failures. Release deployment and live-account acceptance are separate steps.
