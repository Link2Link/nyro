# Provider-local model ratings

[简体中文](model-ratings_CN.md)

## Purpose

Nyro stores a manually assigned **comprehensive capability score** for each exact
`(provider_id, upstream_model)` pair. Scores are integers from **0 to 100**. They
are subjective administrative labels, not IQ, benchmark results, probabilities,
or a ratio scale. They exclude latency, throughput, price, and availability.

Rating management provides individual editing, persistence, global comparison,
provider-copy snapshots, and configuration backup. Each pair has an effort profile:
one nullable **common** score and five optional **low / medium / high / xhigh / max**
overrides. The **Performance** page compares effective scores with completion-aware,
observed TPS. **Scores do not change routing.**
Chart/router consumers must explicitly handle unrated models rather than defaulting
them to a numerical score.

## States and identity

| State | Representation | Meaning |
|---|---|---|
| Rated | `status: "rated"`, integer score, timestamp | A saved manual evaluation; 0 is valid |
| Unrated | `status: "unrated"`, `score: null`, `updated_at: null` | A successful lookup found no saved row |
| Unknown/error | Failed request, explicit UI error | The score could not be determined; never substitute unrated |

The database stores only explicitly rated scopes. Clearing deletes that scope's
row; `null` is absence, never zero. A tier resolves **override → common → unrated**.
An explicit override remains an override even when equal to common, including zero.
Clearing common preserves overrides; clearing an override resumes inheritance.
State is derived, not a redundant persisted flag. Provider IDs, not names or vendor
presets, identify the supplier. Routing mappings sharing a pair share its profile;
deleting or recreating a mapping does not affect that profile.

Upstream model identity preserves case, whitespace, Unicode, and namespace/slash
characters. Rating APIs do not lowercase, trim, normalize Unicode, or merge aliases.
Names must be nonblank, contain no NUL, and be at most 1024 UTF-8 bytes; invalid
names are rejected rather than truncated. Existing provider protocol parsers remain
the boundary at which upstream catalogs become Nyro model identifiers.

Each `(provider_id, upstream_model, effort)` row stores only `score` and
`updated_at`; `effort` is `common` or one of the five tiers. There are no notes,
capability dimensions, confidence fields, or history. New/changed profile values
use server UTC RFC3339 millisecond timestamps; unchanged scopes retain their times.
Legacy individual saves reconfirm the common score with a fresh timestamp.
Concurrent edits are last-successful-write-wins. Legacy
pair-only ratings migrate to `common` without changing their scores or timestamps.

The **low** performance tier includes outgoing `minimal` and `low`; `medium`,
`high`, `xhigh`, and `max` remain distinct. Budget-based, disabled, other, unknown,
and unspecified effort are not guessed into a tier. They can contribute only to
mixed statistics when completion and TPS are otherwise credible.

## Administration UI

- **Available Models** displays the score or **Unrated** and opens a single-model editor.
- **Model Ratings** (`/model-ratings`) provides a flat cross-provider table, search,
  supplier/state/range filters, score/name/update-time sorting, and pagination.
- Rows are the union of successful catalogs, known routing targets, and saved
  scores. Disabled suppliers and no-longer-listed saved models remain manageable.
- Score sort defaults to descending; unrated rows are last in either direction.
  A score range includes only rated rows. Ties have a stable identity ordering.
- A single model popup edits common and all five overrides, showing explicit,
  inherited, and unrated states. One **Save** submits the complete profile in one
  atomic operation; validation or storage failure leaves every scope unchanged.
- Scores must be integers without rounding or clamping. Empty/unset is not zero;
  clear/unset controls make removal explicit. Equal-to-common overrides are not
  silently collapsed. Cancel discards the entire unsaved draft.
- Rating failures show a retry/error state, not fictitious unrated counts. Catalog
  errors show an incomplete-directory notice without preventing saved-score editing.
- Only a successful catalog response can establish **Not in catalog**; failures
  or disabled suppliers yield **Catalog unknown**.

The rating page requests catalogs with `require_catalog=true`. This opt-in mode
reports upstream transport/HTTP/JSON/schema failures rather than the historical
best-effort static fallback. Other catalog consumers retain existing behavior.
The catalog is never consulted when saving or clearing a score.

## Performance chart

**Performance** (`/performance`) appears between **Stats** and **Extensions**.
It uses the dedicated `/api/v1/model-performance` snapshot, with no upstream
discovery or active benchmark calls. **Only this page uses the new aggregation**;
`get_model_usage_stats` and other legacy usage APIs keep their existing semantics.

### Profile and sample groups

- With **no explicit override**, a pair appears only in the mixed chart, using its
  common score and mixed TPS. With **any explicit override**, it switches to the
  five tier charts. Each tier uses its effective score and **same-tier TPS only**;
  never borrow mixed TPS or another tier's samples. An equal-to-common override
  still triggers this switch. Unrated tiers or tiers without valid TPS have no point.
- For each exact pair and each group independently, select the **latest 10 completed
  requests within the last 7 days**, then average the valid per-request TPS values.
  Ordering is completion time descending with request ID as a deterministic tie-break.
  Invalid TPS inside those ten does not cause an older eleventh request to be fetched.
- Eligible requests have credible completion metadata and successful upstream and
  client HTTP status. Failed, cancelled, timed-out, truncated, token-limit, and
  unknown-completion attempts are excluded before the latest-ten selection.
- TPS is output tokens divided by measured upstream generation time, not a
  token-weighted aggregate. Buffered responses use full upstream duration; streams
  subtract time to first chunk unless generation is under 50 ms or first-chunk
  latency is at least 80% of upstream duration, in which case full duration is used.
  Missing, nonfinite, nonpositive, or inconsistent timings/tokens are not zero samples.
- One valid sample is enough to draw a point; **fewer than 3 valid samples are hollow**.
  Counts distinguish selected completed requests from valid TPS samples. Sample
  times describe valid samples, not the latest arbitrary call. Unclassified completed
  requests remain mixed-only; untrusted historical counts are shown separately.

### Completion and effort provenance

Effort comes from the **final outgoing upstream request after all rewrites**,
including route max-effort overrides and vendor dialect mapping, per attempt.
The client-request `reasoning_effort` log field is not a fallback for missing
outgoing evidence. Scalar metadata is recorded even when payload logging is off.

Completion is observed per attempt at the response Body boundary **before the log
is persisted**. A successful protocol terminal outcome alone is insufficient if
body delivery is cancelled or fails. “Completed” means gateway-observed Body EOS
with a credible successful terminal outcome, **not client acknowledgement** or
proof that the client application consumed the response.

All pre-feature logs retain `request_completion = 'unknown'`: existing status,
usage, and stored payloads cannot prove body delivery. Bounded batches may recover
**outgoing effort only** from retained upstream request bodies within the last
7 days, never client fallback or historical completion. A newly upgraded chart
therefore needs new credible samples; old successful-looking logs do not fill it.

Evidence parsing is conservative: a buffered response or individual SSE event over
1 MiB, undecodable original compressed bytes, unknown terminal reasons, or missing
required protocol termination markers are not proven complete. Proxy response
handling is unchanged, but these attempts do not become new performance samples.
Some otherwise successful requests may therefore lack plotted data; this is never
interpreted as a model speed of zero.

### Chart interaction

- X is fixed at **0–100**, including an explicit score of zero. Y starts at zero
  with a **200 TPS** ceiling, expanding upward in **50 TPS** steps when necessary.
- Points use stable provider colors and compact numeric IDs. A full-identity side
  panel maps each ID to provider, model, tier, score source, TPS, counts, and times.
  IDs are assigned for the full snapshot and remain stable while filters change.
- Exact overlaps are grouped without moving their true coordinates; nearby candidate
  lists make dense points selectable. There is **no jitter**. Pointer, keyboard,
  and touch selection pin a highlight shared by the point and side-panel entry.
- Supplier/text filters narrow the chart without renumbering it. Disabled providers
  with saved profiles remain visible. Small screens can scroll the chart horizontally.
- Refresh replaces the snapshot (profiles, shared `as_of`/window, and all groups).
  Missing TPS, loading, and failures are explicit, never fabricated zero points;
  valid peers remain visible when the response marks an individual model unavailable.

Performance is observational, affected by request size, upstream load, and reasoning
settings. It pairs current manual scores with recent observations, not controlled
benchmark results or a claim that effort tiers have equal workloads.

## Management API

All endpoints use existing Admin API authentication; none are public proxy model
metadata. Desktop IPC invokes the same core service.

| HTTP | Operation |
|---|---|
| `GET /api/v1/provider-model-rating-profiles` | Saved profiles, including disabled/unlisted pairs; optional `provider_id` filter |
| `GET /api/v1/providers/:id/model-rating-profile?model=…` | Complete profile, effective tier scores/sources, and display mode |
| `PUT /api/v1/providers/:id/model-rating-profile?model=…` | Atomically replace common and all five overrides |
| `GET /api/v1/model-performance` | One `as_of`, `window_start`, and `models` snapshot; optional `provider_id` filter |
| `GET /api/v1/provider-model-ratings` | Legacy **common-only** saved rows; optional `provider_id` filter |
| `GET /api/v1/providers/:id/model-rating?model=…` | Legacy common rated/unrated state |
| `PUT /api/v1/providers/:id/model-rating?model=…` | Legacy common-only `{ "score": 85 }` update |
| `DELETE /api/v1/providers/:id/model-rating?model=…` | Clear common only, preserving every override; return `{ "ok": true }` |

Profile PUT requires exactly one nullable common score and **all five** nullable
override scores, including unchanged/unset tiers (not a partial patch):

```json
{
  "common": 80,
  "overrides": { "low": null, "medium": 0, "high": 80, "xhigh": null, "max": 95 }
}
```

Here medium is explicitly zero and high remains an explicit override equal to
common. Missing keys, unknown fields/tiers, and noninteger/out-of-range scores
are rejected before any write. All-null clears the profile atomically. Returned
profiles contain nullable `{score, updated_at}` values, `effective` tier states
with `source` (`override`, `common`, `unrated`) and `score_updated_at`, and
`display_mode` (`common` or `per_effort`).

Each performance model contains `profile`, `mixed`, `tiers`, `unclassified_count`,
`untrusted_count`, and availability `status` (plus `error` when unavailable).
Every mixed/tier group contains `selected_request_count`, `valid_tps_count`,
nullable `average_tps`, `first_sample_at`, and `last_sample_at`. Snapshot/sample
times are Unix milliseconds. Empty statistics are valid empty groups, not errors.

URL-encode model query values, especially `/`, `+`, `#`, and spaces. Successful
list/get/put responses use the existing `{ "data": ... }` wrapper. Example lookup:

```json
{
  "data": {
    "provider_id": "provider-id",
    "upstream_model": "vendor/model-x",
    "status": "unrated",
    "score": null,
    "updated_at": null
  }
}
```

A rated legacy lookup substitutes `status: "rated"`, a numeric score (including zero),
and a timestamp. Legacy PUT rejects missing/null/string/fractional/out-of-range scores
and client-supplied fields such as `updated_at`. DELETE is idempotent for a valid
supplier. Errors use JSON `{ "error": "..." }` with 400 invalid input, 404 unknown
supplier, 501 unsupported storage, or 500 internal failure. Storage failures are
never translated into empty successful lists.

The list enumerates **saved ratings**, not the universe of unrated upstream models.
A future consumer must join it against its known candidate pairs only after a
successful read, or query individual explicit states. If a directory is unavailable,
its undiscovered unrated models cannot be counted.

## Lifecycle and backups

| Event | Rating behavior |
|---|---|
| Catalog refresh, temporary disappearance, probe failure | Preserve score and time |
| Provider disable, URL/account/channel change | Preserve score and time; manually re-evaluate if needed |
| Mapping deletion | Preserve score |
| Same exact model returns under the same provider | Reuse saved score |
| Different model ID/alias appears | No automatic inheritance |
| Clear scope / entire profile | Delete that scope / all six scopes only for the exact pair |
| Delete provider | Transactionally delete all its ratings in every scope |
| Copy provider | Copy all common/override score/time snapshots into the new provider ID; later edits are independent |
| Export config | Nest `model_ratings` with explicit `effort` under each provider; include every scope and stale-catalog/disabled records |
| Import new provider | Restore scores with the new ID; preserve the original instant in UTC millisecond form |
| Import existing provider name | Skip both provider and its nested scores; do not overwrite |

Export remains a compatible version-2 extension. Old backups without ratings load
as empty metadata; old rating entries without `effort` default to `common`.
Older Nyro versions cannot be relied upon to preserve the new scopes. All nested
scores, identities, timestamps, effort scopes, and duplicate exact
`(upstream_model, effort)` keys are validated before import changes begin. A failed rating restore rolls back its
newly created supplier; earlier completed suppliers are not globally rolled back,
and failure feedback reports partial progress. Copy failure similarly reports and
rolls back the new supplier rather than succeeding without scores.

## Architecture and validation

`ProviderModelRatingStore` is separate from routing snapshots, backend weights, and
third-party `ModelCapabilities`. SQLite, PostgreSQL, and MySQL persist the table.
YAML/MemoryStorage preserves its existing read-only/unsupported administration mode;
no transient write is reported as persistent success.

Pure rating writes do not change `config_epoch`, reload routing caches, or refresh
quota state. UI pages share a validated list query with explicit mutation
invalidation and independent rating refresh. See [database schema](../database/schema.md)
for DDL and safe generation of reference SQL from disposable migrated databases.

Focused verification:

```bash
cargo test -p nyro-core --test provider_model_ratings
cargo test -p nyro-core --test storage_provider_model_ratings
cargo test -p nyro-core --test storage_effort_performance
cargo test -p nyro-server --no-default-features rating_
cargo check -p nyro-core
cargo check -p nyro-desktop
```

Run WebUI lint/build from `webui/`. The lightweight UI helper tests require no extra framework:

```bash
# From webui/
test_dir=$(mktemp -d /tmp/nyro-rating-tests.XXXXXX)
./node_modules/.bin/tsc src/lib/model-ratings.test.ts --outDir "$test_dir" \
  --module commonjs --moduleResolution node --target es2020 \
  --esModuleInterop --skipLibCheck
node --test "$test_dir/model-ratings.test.js"
```

Storage tests document opt-in disposable
PostgreSQL/MySQL conformance guards; a skipped backend is not externally verified.
The Node/CDP smoke test in `tests/webui/` uses only isolated temporary services and
checks real browser interactions without touching an existing deployment.
