# Model-aware quota buckets

Status: implementation contract, 2026-11-05. Implemented on the feature branch with offline verification; release status is tracked separately. No SQL schema migration is required. Chinese companion: [quota-buckets_CN.md](quota-buckets_CN.md).

## Implementation and evidence boundaries

The implementation supplies scoped quota state/admission, immutable per-attempt attribution, retained-log bucket pricing and filters, internal structured cards/configuration, and flattened external publication for DSH. Official generic Google labels do not prove Flash/Argon membership: exact official associations or an explicit validated operator mapping of discovered resources are required. Discovery-only/catalog observations are not published as healthy independent allowance suppliers. Until such mapping exists, users may see unresolved diagnostics rather than the prototype's two fabricated fixed-window buckets.

Google raw structured denial-to-summary constraint identifiers have not been established by a sanitized real response fixture. Unknown 429s therefore use generation-fenced narrow model cooldowns and refresh. The internal trusted-constraint rejection API only accepts existing enforced raw window/shared resources and fences older refreshes; no invented raw-error parser claims it can identify a Google allowance. Subday cycle statistics are available only with positive canonical fixed-window evidence. Qualified catalog or rolling/unknown window names cannot create cycle prices/capacity.

Live owner/fence/resource mappings (`quota_bucket_owner.*`, `quota_bucket_fence.*`, `quota_buckets.*`) are intentionally excluded from configuration export and ignored on import. Restored providers must rediscover and explicitly rebind live resources; copied identifiers cannot transfer quota or owner state. The existing permanent daily usage ledger is unchanged. Filtered detail aggregation uses one scalar SQL cursor; filtered paginated logs remain a metadata postfilter compatibility path and may need snapshot/keyset optimization for high-traffic consistency.


## Goal and non-goals

One Provider can serve models backed by independent upstream allowances. The motivating case is the operator-reported separation between Gemini 3.8 Flash and Gemini 4 Argon. Show and account for each allowance independently, admit requests against the allowance they actually consume, and retain one Provider configuration and its credentials.

Do not split a Provider merely to obtain two quota bars. Do not equate quota usage with recorded tokens, API-equivalent prices with actual subscription spend, or two equal percentages with evidence of a shared allowance. This design does not automatically substitute Argon for Flash, turn on paid overage, introduce Google account pools, or change other vendors' quota semantics.

## Existing behavior and the information it loses

- [`QuotaSummaryBucket`](../../crates/nyro-core/src/provider/google/antigravity.rs#L434-L453) already preserves group/display names, optional bucket ID, window, remaining fraction and reset. It does not currently expose an explicit model-to-bucket association; `AvailableModel` only preserves model ID and a fraction/reset.
- `admin/usage.rs::quota_summary_tiers` flattens model-family × window observations into the most-used row per window. Its catalog fallback first collapses thinking variants, then selects the modal Gemini family percentage as one `gemini` row.
- `google_scheduling_observation` selects the least-used flattened row and publishes a synthetic `subscription_pool`. It confuses alternative model allowances with simultaneous window constraints. The synthetic name also cannot be scored by the existing canonical-window usage strategy.
- `ProviderQuotaRegistry` is keyed by provider ID. `UsageStrategy` assigns one provider score to all its targets. The dispatcher checks this provider gate before resolving wildcard targets and the actual backend model.
- `admin/provider_api_price.rs` selects one provider-wide cycle, sums all models' retained usage, then divides that cost by the selected percentage. The denominator can belong to a different model allowance than part of the numerator.
- `ProviderUsageFooter` renders flat tiers and one price summary. Nyro forwarding exposes providers/routes, not model-aware quota admission.

These are common-model limitations, not just missing UI rows. Preserve identity before aggregation, and have UI, statistics and admission consume the same binding.

## Domain and invariants

### Provider, owner, bucket, window

```text
Provider
  └─ Quota owner / authority scope
       ├─ Flash bucket ── five-hour constraint + weekly constraint
       ├─ Argon bucket ── five-hour constraint + weekly constraint
       └─ Optional explicitly shared constraint ── applies to both
```

A quota owner is an entitlement identity, not an access token. Internally scope it by provider ID, stable account/legacy-credential identity, quota authority and project/entitlement namespace. Token refresh does not create fresh quota. Credential generation fences late results but is not part of the permanent bucket identity. A different subscriber/project must not inherit the old subscriber's state.

A **bucket** groups requests sharing an allowance; a **window** is one time-bounded constraint on it. Upstream `bucketId` may identify a window-level constraint rather than a family-level bucket: the adapter must preserve both identities instead of assuming it is always the parent ID. Shared global constraints are represented separately and referenced, not duplicated into independent capacity.

Core invariants:

1. Every request is evaluated against its actual upstream model and entitlement owner.
2. All applicable windows/constraints must allow a request (AND). Alternative configured targets/accounts are alternatives (OR). Display summaries do not decide admission.
3. Flash exhaustion cannot pause Argon unless an explicitly shared constraint is exhausted. Authentication/account unavailability can block both.
4. Every recorded attempt has at most one primary accounting bucket or an explicit unassigned result. Additional constraint references must not duplicate its tokens or cost in provider totals.
5. Observed upstream percentages are not additive across buckets or windows. Unknown values are not zero usage or unlimited allowance.
6. API-equivalent prices are admin-only estimates. Neither price lookup failure nor a missing price may pause/free quota or alter quota-based ranking.

## Proposed internal contracts

Use typed structures; names below are illustrative, not existing public API guarantees:

```text
QuotaScopeKey(provider_id, owner_id, authority_namespace)
QuotaBucket(id, scope, label, model_binding, window_refs, shared_constraint_refs)
QuotaWindow(id, kind, used_percent?, resets_at?, duration?, source, observed_at, quality)
QuotaBinding(primary_bucket?, applicable_constraint_refs[], provenance, version)
PreparedAdmission(logical_model, backend_model, wire_model, owner, credential_generation,
                  quota_binding, snapshot_revision)
QuotaEvaluation(eligible | exhausted | unknown, blockers[], reserve, freshness, score?)
```

`model_binding` uses exact discovered wire IDs and explicitly supported aliases. Quota identity, public model identity and API-price identity are different mappings. For example, Flash effort variants may be mapped to one verified allowance while pricing still uses their correct reference model. Argon's bare/tier IDs must not be collapsed merely because a suffix resembles a reasoning level.

Bucket IDs are opaque, stable within their scoped lifetime, and never derived from utilization/reset timestamps or mutable display labels. Raw account/project IDs and email addresses are not exposed as public bucket IDs. Known incompatible entitlement changes create a new scoped identity; renamed labels do not.

### Binding sources and unknowns

Resolve bindings in this order:

1. Explicit official associations between models, entitlement resources and enforced constraints, where the actual response schema provides them.
2. Versioned vendor-adapter rules supported by evidence, including recognized aliases and channel/entitlement scope.
3. Explicit operator bindings to discovered opaque resource IDs and exact model selectors, with source, validity interval and overlap validation visible in the admin UI.
4. Unresolved: preserve separate discovered observations, mark association unknown, and do not pool or fabricate a family-wide percentage.

Do not infer grouping from matching percentages/reset times, loose `gemini-*` prefixes, display-name substrings, or the API-price matcher. The user's report establishes that Flash and Argon should be treated separately, but this design does not claim to have fetched their current official bucket IDs or tier memberships. Obtain a sanitized discovery/summary fixture in implementation before enabling an automatic adapter rule. Unbound buckets remain visible. Conflicting primary accounting bindings are a configuration error, not first-match-wins.

## Observation and recovery

Fetch enforced summary and model discovery as independently fallible tasks. A discovery outage must not prevent publishing a valid enforced summary; a summary outage must not replace enforced exhaustion with optimistic catalog data.

Store quality, freshness and last-good state per scoped constraint. One refresh may succeed for Flash and fail for Argon. Missing/partial data is not deletion or recovery. Publish atomically with owner generation, mapping/snapshot revision and rejection-revision fences. A refresh started before a newer quota rejection cannot clear that rejection; a late result for replaced credentials cannot update the new entitlement.

Authority is purpose-specific: enforced quota evidence governs its constraints; a catalog entry may establish model discovery or an explicitly identified catalog-only scope, but does not clear a stronger enforcement fence. Catalog-only fallback remains distinguishable and cannot invent window duration. Preserve an exhausted fence until a fresh same-scope authoritative recovery proves every applicable blocking constraint recovered. Reset time triggers recheck, not automatic replenishment. Unknown generic 429 throttles expire as throttles, not as confirmed quota recovery.

Reuse per-owner refresh coalescing/backoff, normal five-minute refresh and prompt exhausted/throttled rechecks. Cold/unknown mappings allow only bounded controlled probing under existing health/concurrency rules and receive no fabricated usage advantage; known blocked constraints remain blocked even if other mappings are unknown. Persist/reload compact owner-scoped enforcement fences when required to avoid restart erasing a known exhaustion; use existing settings storage initially and expire scope only with authoritative identity/policy changes.

## Request admission and scheduling

### One prepared model identity

Introduce one shared model/admission preparation result used by the selector, encoder, log builder and feedback handler. Materialize wildcard backend model, selected account, reasoning overrides, channel aliases and final wire model before final quota admission. Do not implement a second guessed quota-only model resolver.

The outer selector can score cached model bindings without network I/O; owner/account binding and generation are validated again after credential selection. Immediately before network dispatch, verify that hooks/encoding did not change the owner or wire model outside the admitted binding; otherwise recompute admission or reject locally. Revalidate between retries and queued account/key siblings.

Every balance strategy (`priority`, `weighted`, `latency`, `usage`) honors the same scoped admission gate. Their ordering policies do not grant permission to use an exhausted bucket. An unavailable Flash bucket skips the Flash target; another configured Flash provider can serve it. Argon is tried only if the operator has explicitly included an Argon target in that virtual model's route.

### Usage weighting

Keep the current pacing formula inside each applicable allowance:

```text
r = remaining quota fraction / remaining time fraction
weight = clamp(r)^3 × existing window perishability boost
```

Preserve current largest identified main-window scoring (monthly, weekly, daily, five-hour) within each bucket; ALL applicable windows still gate admission even when only the largest scores pacing. Multiple simultaneously required allowance groups/shared constraints cap the candidate's effective pacing by the least permissive applicable group. Unknown-duration catalog percentages must not be promoted to canonical weekly/five-hour windows.

Avoid duplicate-model probability inflation:

1. For this virtual route, group eligible targets by provider, then by the same resolved quota-choice/constraint signature.
2. Aliases/duplicate rows drawing from the same bucket get one inner quota-group mass. Static configured target weights choose rows within that group.
3. Rank/select distinct bucket groups inside a provider by their own effective pacing weight.
4. Preserve ONE outer mass per provider, represented by the maximum eligible inner group weight, not the sum. A healthy unused bucket can improve its provider's representative score, but adding aliases/bucket rows cannot multiply its cross-provider lottery tickets.
5. Included quota ranks before unknown fallback, which ranks before explicitly authorized paid reserve. Blocked groups get no mass. A provider with only unknown eligible groups keeps the existing one-mass unknown behavior. Weights for known groups are not boosted by unrelated unknown groups.

This is an explicit v1 compatibility policy, not a capacity-addition claim. Adding truly additive capacity-based routing later requires comparable capacity units and an explicit policy, not summed percentages. Priority/latency/static-weight strategies retain their own ordering after the scoped gate.

### Response feedback

- Structured quota denial with a trustworthy constraint reference updates only that owner/constraint; if one budget is shared by several variants, all its bound variants are gated.
- Unknown 429: narrow short-lived owner/model/endpoint cooldown plus coalesced refresh. Do not label the whole provider or all buckets exhausted.
- RPM/capacity throttle and included-allowance exhaustion are distinct causes. Auth failure fences the affected credential/account, not just one bucket. Connectivity and model-forbidden errors retain correctly scoped health rules.
- Retry only safe configured alternatives before output/continuation commitments, respecting existing total deadlines, retry limits and account/session replay restrictions.
- Route decision snapshots record bucket, constraints, source/quality, scoring window, snapshot revision and exact blocker/cooldown scope. Provider-wide trace annotations must not mark healthy sibling-model rows skipped.

## Accounting and API-equivalent pricing

### Stable attempt attribution

Add server-owned typed, versioned `quota_attribution` metadata to the existing `route_decision` JSON for the actual attempt (not every hypothetical candidate): provider/owner scoped opaque ID, primary accounting bucket, additional constraint refs, backend and wire model, mapping revision/source, dispatched-at timestamp and attribution status. Write it independent of ordinary payload recording; never trust an incoming client JSON field. Do not store tokens, email, project identifiers or credentials in this metadata.

An attempt routed through several windows is counted once in provider/bucket totals, never once per window. Failed/retried attempts remain included in attempt counts and any recorded token cost; do not deduplicate by client_request_id or invent free zero-token usage from missing evidence. Locally skipped/rejected attempts are diagnostic events, not evidence that upstream quota was consumed.

Historical logs without this metadata may be classified only with an unambiguous time-valid mapping and labeled inferred. Never apply today's reassigned bucket mapping retroactively as fact. Missing or conflicting ownership/model evidence goes into `unassigned`; retired buckets retain historical totals. Existing `upstream_model` may be the pre-wire backend alias, not the final tier ID.

### Scan and period rules

Extend the provider retained-usage accumulator to scan each provider once per statistics refresh, grouping by owner + immutable accounting bucket + exact priced model. Accept independent `[start,end)` windows per bucket/window, and cap every result at one captured observation cutoff. Share the same priced rows for provider, bucket and model breakdowns rather than rescanning logs or doing rate lookups for every bucket.

For new attributed attempts, dispatch time anchors cycle membership; require recorded/visible completion metadata by the snapshot cutoff. For older rows the existing emitted-log timestamp is only a legacy estimate and must be labeled. A request spanning a reset is attributed by dispatch-time policy, not presented as measured Google budget allocation. Buffered logs, dropped writes and out-of-gateway traffic mean local totals are not the upstream quota ledger.

Keep v1 totals aligned with existing retained-log pricing: clearing logs reduces provider and bucket API-equivalent totals. The permanent UTC-day token ledger is separate; it has provider/model but no historical owner/bucket or sub-day boundaries. Do not use it to reconstruct precise bucket reset cycles after log deletion. A future durable bucket ledger needs an explicit schema/retention design, not a hidden change of existing totals.

Select an identified weekly cycle per bucket; use an identified five-hour cycle only when that bucket has no weekly constraint. A present but invalid/expired weekly observation does not silently fall back. For other vendors keep existing per-vendor cycle policy. Different bucket cycle ends may differ. Unknown duration/reset yields no current-cycle price or capacity projection, not a fabricated calendar cycle. A reset timestamp alone does not prove a fixed-duration cycle: distinguish rolling limits from fixed resets, and withhold exact cycle/capacity claims until upstream semantics or a validated adapter establishes the interval.

For bucket B and selected cycle W:

```text
cost(B,W) = sum(current reference/manual price of recorded tokens
                for attempts attributed to B within W)
estimated_total_api_capacity(B,W) = cost(B,W) / used_fraction(B,W)
```

Only use a fresh matching scope/window/source with utilization > 0, priced observations, valid bounds and adequate attribution evidence. Mark estimates partial/low-confidence when usage/attribution coverage is incomplete; suppress when evidence cannot support a matched estimate. Used=0 may show recorded cycle cost but cannot project capacity. A measured utilization shared across all buckets cannot divide any individual bucket's cost. Shared global-constraint estimates, if shown, are a separate overlapping view and must not be added to bucket capacities.

Reuse the current pricing engine per exact applicable reference model; never assign Flash's price to unknown Argon pricing. Keep currencies separate, unknown prices unpriced, missing tokens distinct from observed zero, and report retained attempt counts, input/output/cache-read tokens and missing-usage/attribution/pricing coverage. Cache reads already belong to gross input and are not added twice.

Provider all-retained totals equal the partition of primary buckets plus unassigned attempts (per currency for cost). Provider total current-cycle/capacity should not aggregate incompatible bucket cycles. A bucket without a meaningful primary accounting choice can still participate as an admission constraint while its historical attribution remains unknown.

## Admin API and UI

Add optional `quota_buckets` and model-aware scheduling detail to the shared core Admin DTO. The exact-provider endpoint and explicit provider-oriented view retain one real Provider with nested buckets for Nyro's own web/desktop management UI. Bulk external usage views instead publish one separately identified read-only provider entry per bucket (see below); unchanged single-bucket vendors retain their existing shape. Suggested internal bucket fields:

```json
{
  "id": "opaque-local-bucket-id",
  "label": "Gemini Flash",
  "models": ["gemini-3.8-flash"],
  "binding": {"source": "configured", "status": "resolved", "revision": 1},
  "windows": [{"id": "opaque-window-id", "kind": "weekly_limit",
    "used_percent": 65, "resets_at": "<upstream RFC3339>",
    "source": "enforced_summary", "freshness": "fresh"}],
  "scheduling": {"status": "eligible", "blocking_constraint_ids": []},
  "api_price": "<admin-only per-bucket stats and coverage>"
}
```

Example is illustrative and not a live observation. Include source observations/quality and independently timestamped windows in the actual contract. `models` denotes a resolved binding, not a loosely inferred family membership. Backend labels/defaults are English with explicit Chinese UI translations.

Keep one Provider card, with two stable subcards (Flash / Argon), each showing:

- bound models, source/binding status and individual scheduling state;
- its own five-hour/weekly bars, reset timestamps, stale/unknown warnings;
- retained requests/tokens, total API-equivalent price, its own current-cycle price and approximate total cycle capacity;
- details for exact models, missing usage/unpriced/unassigned counts and shared blockers.

Provider header shows available/partially available/exhausted/unknown summary without a bogus combined percentage. A healthy Argon card cannot hide a Flash pause. Provider footer may show all-retained grand total and unassigned subtotal only; it must not sum different periods or overlapping capacity estimates. Reuse API calculations in web/desktop; UI never discovers grouping or computes scheduling.

### External usage projection: one bucket, one provider entry

Confirmed operator requirement: external consumers must see Flash and Argon as two separate provider cards, never several indistinguishable `five_hour`/`weekly_limit` rows inside one card. This is a **Published Usage Entry**, not an extra configured Provider.

Source audit of the active local DSH plugin confirms both paths matter:

- [Admin client](../../dsh-nyro-usage/src/nyro.ts#L297-L303) calls `GET /api/v1/providers/usage` without parameters and checks the `data` array. Its connection test separately counts real configurations through `/api/v1/providers`.
- [Card expansion](../../dsh-nyro-usage/src/client/panel/upstream.ts#L41-L81) uses direct `provider_id`, or the composite of relay-provider ID and remote `provider.id`, for card identity/drag order. It replaces a relay's summary card with its forwarded rows.
- [Card rendering](../../dsh-nyro-usage/src/client/panel/ProviderCard.tsx#L171-L175) renders every flat tier with `key={tier.name}`. Merely appending another pair of `five_hour`/`weekly_limit` fields causes ambiguous labeling and duplicate keys.

#### Endpoint views

| Surface | Contract |
| --- | --- |
| `GET /api/v1/providers/usage` (default, or `?view=buckets`) | Existing `{data:[...]}` envelope, one flat read-only usage-provider row per primary bucket. This supports the current DSH client without requiring it to discover nested buckets. |
| `GET /api/v1/providers/usage?view=providers` | Existing real-provider row semantics with optional nested `quota_buckets`. Nyro's web management client explicitly uses this view. |
| `GET /api/v1/providers/:id/usage` | Real provider ID only; one structured provider with nested buckets. |
| Core `list_provider_usage` / desktop command | Remain real-provider oriented; HTTP publication projection is a separate shared service/helper, not a mutation of routing state. |
| `GET /api/v1/providers` and provider CRUD | Unchanged real configurations. Published usage IDs are not configuration IDs or valid new routing targets. |
| `GET /v1/nyro/usage` | Existing `providers[]` envelope, expanded into anonymous flat bucket entries with precise allowed-route associations. Version/capability metadata describes the projection and route-specific admission support. |

The default bulk change is a deliberate enumeration-contract change, not merely an optional-field addition: some clients assume one row per real provider or perform CRUD with `provider_id`. Document `view=providers` as their migration path and deploy the explicit web query in the same release; desktop stays structured. Reject unrecognized `view` values rather than silently returning another representation. Do not silently claim universal client compatibility. Existing DSH quota rendering can consume the flat rows, but it currently ignores `api_price`: displaying those estimates in DSH requires a separate plugin UI enhancement. Its provider-configuration count and usage-entry count also have different meanings and should be labeled separately in a follow-up plugin improvement.

#### Identity and field rules

- Example names: `Google AI Pro Year · Flash`, `Google AI Pro Year · Argon`. Put the model distinction in the title, not hidden in repeated tier labels. Keep `kind = google_subscription`, site/level and standard tier names unchanged.
- Every expanded entry has a stable unique opaque published ID scoped to source instance + real provider + owner/bucket identity. Never use array position, utilization, reset time or display name as identity. IDs remain stable across label/reset/token refresh; changed entitlement scope gets a new ID. For the admin bulk row, `provider_id` and `usage.provider_id` are the same published ID; the nested proxy row's `id` and `usage.provider_id` likewise agree. IDs are display/read-only, not accepted for provider mutations.
- Add typed provenance, e.g. `usage_entry: {kind: quota_bucket, source_provider_ref, bucket_ref}`. Admin-only provenance can link to a real provider; proxy provenance is namespaced/opaque and contains no account/project identity. Clients must not regroup rows by the parent reference or display name. Plugin keys already include instance/relay context; preserve that rather than colliding same names across instances.
- For a multi-bucket provider, replace its aggregate quota row with the bucket rows in that usage view; do **not** emit the aggregate parent plus both children. Do not split each thinking variant or each time window into another provider. Unchanged single-bucket vendors retain the legacy ID/name/row; once a provider uses bucket projection, an existing bucket keeps its published ID even if only one remains visible later.
- Each row's `usage.tiers` contains only that primary bucket's windows, canonical sorted `five_hour → daily → weekly_limit → monthly`, at most one row per name. Do not rename to `flash_weekly_limit` or `argon_five_hour`: that breaks existing translations and steady-pace logic. Contradictory duplicate source constraints are unresolved, not overwritten or represented as duplicate keys.
- Shared/global constraints are not independent capacity or a healthy routing alternative. Keep them in separately typed sidecar constraints/blocker details, never a standalone ordinary provider row and never a second same-name tier. Every bucket row's effective scheduling includes its owner/auth and ALL applicable shared constraints. If a compatibility display cannot represent that evidence safely, withhold the unsupported detail rather than copy unrelated percentages.
- `status` remains the query discriminator (`ok`, `error`, `unsupported`); do not add `quota_exhausted` there. Per-bucket exhaustion belongs in `usage.scheduling`, computed from effective admission. `is_enabled` inherits real operator enablement, not temporary quota state.
- Preserve independently timestamped successful/last-good bucket rows on partial refresh failures; expose error/stale quality without inventing zero usage or `eligible`. If no binding/data exists, emit only an explicit unresolved/error diagnostic entry with no made-up window or fabricated extra capacity; never duplicate one error per guessed bucket.
- Admin published rows carry **only the bucket's** optional API-equivalent stats. Do not copy provider grand totals, account-pool payloads, shared balances or actual spends onto every child. Unassigned retained statistics remain on the structured admin view, not a fake routable quota supplier. Proxy publication strips every nested admin price and all identity fields.

Illustrative admin bulk payload (IDs, percentages and times are examples, not live data):

```json
{
  "data": [
    {
      "provider_id": "usage:demo-provider:flash-01",
      "provider_name": "Google AI Pro Year · Flash",
      "is_enabled": true,
      "status": "ok",
      "usage_entry": {"kind": "quota_bucket", "source_provider_ref": "demo-provider", "bucket_ref": "flash-01"},
      "usage": {
        "provider_id": "usage:demo-provider:flash-01",
        "kind": "google_subscription", "site": "global",
        "tiers": [
          {"name": "five_hour", "used_percent": 100, "resets_at": "2026-11-05T10:00:00Z"},
          {"name": "weekly_limit", "used_percent": 60, "resets_at": "2026-11-09T02:00:00Z"}
        ],
        "balances": [], "spends": [],
        "scheduling": {"status": "quota_exhausted", "reason": "usage_limit", "blocking_tiers": ["five_hour"]},
        "queried_at": "2026-11-05T08:20:00Z"
      }
    },
    {
      "provider_id": "usage:demo-provider:argon-01",
      "provider_name": "Google AI Pro Year · Argon",
      "is_enabled": true,
      "status": "ok",
      "usage_entry": {"kind": "quota_bucket", "source_provider_ref": "demo-provider", "bucket_ref": "argon-01"},
      "usage": {
        "provider_id": "usage:demo-provider:argon-01",
        "kind": "google_subscription", "site": "global",
        "tiers": [
          {"name": "five_hour", "used_percent": 18, "resets_at": "2026-11-05T12:00:00Z"},
          {"name": "weekly_limit", "used_percent": 30, "resets_at": "2026-11-11T00:00:00Z"}
        ],
        "balances": [], "spends": [],
        "scheduling": {"status": "eligible", "blocking_tiers": []},
        "queried_at": "2026-11-05T08:20:00Z"
      }
    }
  ]
}
```

On the proxy endpoint the corresponding rows use `id`/`name`/`vendor`/`routes` and the same per-bucket `usage` shape; provenance uses only safe references. Cache the real-provider snapshot once, then filter by caller's route permissions and expand locally; do not repeat upstream quota calls per published row. Keep default and `view=providers` cache keys/representation validators distinct, and never cache an admin-enriched projection in a public forwarding cache.

### Nyro-to-Nyro

This is required for the operator's Aliyun → VPS topology, not optional UI polish. In addition to the flat bucket publication, extend forwarding with capabilities/versioning plus per-visible-remote-route admission/usage bindings. The Aliyun provider target refers to a remote virtual model/route, not the final Google wire name. Evaluate only the providers/buckets that can actually serve THAT authorized remote route; unrelated Argon or another vendor cannot make a Flash route appear usable. Each row's `routes` is restricted to visible routes that can actually use its bucket; the same bucket behind several routes is emitted once with a deduplicated route list. Preserve concrete backend model identities during route collection. Unknown/ambiguous associations must remain unresolved and must not copy every parent route onto every bucket or disclose hidden sibling buckets.

Forward an explicitly allowlisted anonymous DTO containing visible route IDs/aliases, opaque public bucket/constraint references, windows, freshness and route-specific eligibility. Do not forward admin API prices, owner/account/project identity, hidden model membership or raw new bucket objects. Recursively redact inside new bucket fields and caches, not only top-level `api_price`. Local Aliyun usage prices describe that hop's retained attempts and must never be added to the VPS's prices as if they were distinct Google spend.

A report version bump alone cannot protect older Nyro: its current reader checks the object/providers shape, and its scheduler asks whether ALL rows are exhausted. Therefore publish effective blocked state on every affected primary bucket row (including account/global blockers), and never introduce a healthy standalone shared-constraint row as a false alternative. Upgraded consumers use the explicit route-admission capability. Older peers remain provider-summary-only/unknown for fine-grained admission, with authoritative final enforcement at the upgraded serving hop; they do not gain guessed model mappings. Keep recursion guards and per-owner refresh coalescing.

## Implementation plan and compatibility

1. **Bindings and observations:** add core typed bucket/window/binding model, preserve Google raw identities, capture sanitized fixtures, configure/validate Flash and Argon bindings; test shared and unknown cases. Remove modal/max/min folds from the new path, not from all vendors indiscriminately.
2. **Runtime correctness:** shared prepared identity, scoped admission/registry/feedback/fencing, target-specific trace, usage weighting, additive legacy adapters. Existing GLM/MiniMax/Devin/Kiro/Claude behavior must remain green; do not average account percentages.
3. **Attribution/statistics:** immutable attempt metadata, single-scan multidimensional aggregation and per-bucket periods/pricing/coverage. Existing JSON/settings storage allows v1 without SQL schema changes; no handcrafted SQL artifact edits. New columns/indexes/rollups, if actually needed after measurement, require all storage backends, schema docs and generated Postgres/MySQL references per repository policy.
4. **Admin/UI and remote integration:** optional DTO fields, bilingual subcards/details, typed anonymous per-route relay, old/new peer compatibility. Do not stop after local Google cards: validate actual two-hop admission.
5. **Verification/release separately:** unit/API/SQL-storage/web tests and offline end-to-end two-hop fixtures, then explicitly authorized minimal live quota-discovery/retest. Do not deploy, publish or promise a version from this design alone.

### Acceptance matrix

| Scenario | Required result |
| --- | --- |
| Flash 5h=100%, weekly=20%; Argon healthy | Flash blocked, Argon eligible; provider partially available |
| Flash 5h=10%, weekly=100% | Flash blocked; never choose the least-used window |
| Flash and Argon both healthy but different resets | Independent bars, periods, costs and pacing; no combined cycle |
| Both models additionally require exhausted global budget | Both blocked with shared constraint shown |
| Low/high aliases share one proven bucket | One quota group/mass; combined bucket usage, exact-model pricing retained |
| Equal percentages without binding evidence | Remain unresolved/separate observations, never merge automatically |
| Summary fails after enforced exhaustion; catalog says healthy | Preserve enforced block and stale/source warning |
| Discovery fails but summary succeeds | Update enforced windows; retain/qualify last-good mappings |
| Partial refresh or old credential/rejection result arrives late | No sibling clearing, scope transfer or stale recovery |
| Unknown 429 on Flash | Flash-scoped throttle/recheck, not provider-wide Argon exhaustion |
| Repeated target rows / several buckets on same provider | No additional provider lottery mass; own bucket controls admission/inner ranking |
| Rebinding, model rename, new project/subscriber | Historical attribution stable; ambiguous legacy rows unassigned |
| Missing Argon reference price / missing usage / zero used fraction | No borrowed price, fake free usage, or divide-by-zero estimate |
| Different bucket currencies | No implicit FX conversion or mixed-currency sum |
| Logs cleared; permanent daily rollups remain | Retained bucket prices decrease consistently; no reconstructed sub-day cycles |
| Aliyun Flash route, VPS has healthy Argon but exhausted Flash | Flash-specific report/gate; no unrelated provider/bucket masks exhaustion |
| Ordinary proxy key requests remote usage | Only allowed route/model scopes; no account IDs or nested admin prices |
| Direct DSH Admin bulk query for Flash + Argon | Exactly two bucket cards; each has one `five_hour` and one `weekly_limit`; no parent quota duplicate |
| DSH reads a relay provider's `upstream.providers[]` | Two disambiguated remote bucket cards replace the relay summary, each with stable unique key |
| Bucket rename/reset/token refresh, or only one bucket remains visible | Existing published ID/drag order remains stable; no key collisions across instances |
| Web/desktop management and provider CRUD | Structured real-provider view unchanged; synthetic usage IDs are read-only and not routing destinations |
| Admin bulk default versus `view=providers` | Representation explicitly chosen, unknown views rejected, independent representation caches |
| Shared/auth blocker while old peer reads flat rows | Every affected primary bucket effectively blocked; no healthy standalone shared row masks the pause |
| Partial bucket refresh / hidden bucket / same bucket behind several routes | No zero fabrication or sibling data leakage; one published bucket row with exact deduplicated authorized routes |
| Bucket prices with unassigned history/shared balances | Only attributed bucket prices, no cloned parent money; proxy recursively strips admin prices |
| Priority/weighted/latency route | Same scoped quota gate as usage strategy; no unconfigured model substitution |

Before implementation acceptance, validate the concrete Google model-to-constraint associations and whether generic upstream group labels require an operator binding. That is the main discovery dependency; it does not justify guessing relationships from percentages.
