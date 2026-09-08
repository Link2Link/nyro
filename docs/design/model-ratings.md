# Provider-local model ratings

[简体中文](model-ratings_CN.md)

## Purpose

Nyro stores a manually assigned **comprehensive capability score** for each exact
`(provider_id, upstream_model)` pair. Scores are integers from **0 to 100**. They
are subjective administrative labels, not IQ, benchmark results, probabilities,
or a ratio scale. They exclude latency, throughput, price, and availability.

The UI provides individual editing, persistence, global comparison, provider-copy
snapshots, configuration backup, and a capability-versus-speed performance chart.
**Scores do not change routing.** Unrated models are never assigned an invented score.

## States and identity

| State | Representation | Meaning |
|---|---|---|
| Rated | `status: "rated"`, integer score, timestamp | A saved manual evaluation; 0 is valid |
| Unrated | `status: "unrated"`, `score: null`, `updated_at: null` | A successful lookup found no saved row |
| Unknown/error | Failed request, explicit UI error | The score could not be determined; never substitute unrated |

The database stores only rated rows. Clearing deletes the row. State is derived,
not a redundant persisted flag. Provider IDs, not names or vendor presets, identify
the supplier. A pair shared by multiple routing mappings has one score; deleting
or recreating a mapping does not affect that score.

Upstream model identity preserves case, whitespace, Unicode, and namespace/slash
characters. Rating APIs do not lowercase, trim, normalize Unicode, or merge aliases.
Names must be nonblank, contain no NUL, and be at most 1024 UTF-8 bytes; invalid
names are rejected rather than truncated. Existing provider protocol parsers remain
the boundary at which upstream catalogs become Nyro model identifiers.

Only `score` and `updated_at` accompany the identity. There are no notes, dimensions,
reasoning-effort variants, confidence fields, or history. Normal saves, including
reconfirmation of the same score, update the server UTC RFC3339 millisecond time.
Concurrent edits are last-successful-write-wins.

## Administration UI

- **Available Models** displays the score or **Unrated** and opens a single-model editor.
- **Model Ratings** (`/model-ratings`) provides a flat cross-provider table, search,
  supplier/state/range filters, score/name/update-time sorting, and pagination.
- Rows are the union of successful catalogs, known routing targets, and saved
  scores. Disabled suppliers and no-longer-listed saved models remain manageable.
- Score sort defaults to descending; unrated rows are last in either direction.
  A score range includes only rated rows. Ties have a stable identity ordering.
- Save validates an integer without rounding or clamping. Empty input does not
  clear a score. Clearing uses a separate confirmation.
- Rating failures show a retry/error state, not fictitious unrated counts. Catalog
  errors show an incomplete-directory notice without preventing saved-score editing.
- Only a successful catalog response can establish **Not in catalog**; failures
  or disabled suppliers yield **Catalog unknown**.

The rating page requests catalogs with `require_catalog=true`. This opt-in mode
reports upstream transport/HTTP/JSON/schema failures rather than the historical
best-effort static fallback. Other catalog consumers retain existing behavior.
The catalog is never consulted when saving or clearing a score.

## Management API

All endpoints use existing Admin API authentication; none are public proxy model
metadata. Desktop IPC invokes the same core service.

| HTTP | Operation |
|---|---|
| `GET /api/v1/provider-model-ratings` | All saved rows, including disabled/unlisted pairs |
| `GET /api/v1/provider-model-ratings?provider_id=…` | Saved rows for one supplier |
| `GET /api/v1/providers/:id/model-rating?model=…` | Explicit rated/unrated state for a pair |
| `PUT /api/v1/providers/:id/model-rating?model=…` | Save `{ "score": 85 }`, return the saved row |
| `DELETE /api/v1/providers/:id/model-rating?model=…` | Clear one pair, return `{ "ok": true }` |

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

A rated lookup substitutes `status: "rated"`, a numeric score (including zero),
and a timestamp. PUT rejects missing/null/string/fractional/out-of-range scores
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
| Clear score | Delete only that pair |
| Delete provider | Transactionally delete all its ratings |
| Copy provider | Copy all score/time snapshots into the new provider ID; later edits are independent |
| Export config | Nest `model_ratings` under each exported provider; include stale-catalog/disabled records |
| Import new provider | Restore scores with the new ID; preserve the original instant in UTC millisecond form |
| Import existing provider name | Skip both provider and its nested scores; do not overwrite |

Export remains a compatible version-2 extension. Old backups without ratings load
as empty metadata. Older Nyro versions may ignore the new field and cannot be relied
upon to preserve it. All nested scores, identities, timestamps, and duplicate keys
are validated before import changes begin. A failed rating restore rolls back its
newly created supplier; earlier completed suppliers are not globally rolled back,
and failure feedback reports partial progress. Copy failure similarly reports and
rolls back the new supplier rather than succeeding without scores.

## Performance chart

The **Performance** page (`/performance`) displays one point per rated exact
provider/model pair: its comprehensive score on X and mixed average TPS on Y.
Scores remain 0–100 values, but the X axis starts at zero and adapts its ceiling to
only the visible plotted points: round the highest score up to a multiple of 10,
with a minimum of 10 and maximum of 100; no points uses 100. For example, 53 → 60,
78 → 80, and 60 → 60. Tick spacing is 10, and search/provider filters recalculate
the scale without changing scores or point identities.
Y defaults to 0–100; values above 100 expand its ceiling in 50-TPS steps with headroom.
Reasoning effort is not a score dimension, filter, or point identity. The chart directly labels model names, adding supplier names when
needed to distinguish identical models. Coincident groups list every model; labels
wrap and avoid collisions without moving actual point coordinates. Dense labels that
cannot fit are omitted with a notice. Hover, keyboard focus, or mobile tap reveals
score/TPS and sample details in a dismissible tooltip; no permanent side index or
visible point IDs remain. Search and provider filters are retained.

`GET /api/v1/model-performance` (optional `provider_id`) and desktop
`get_model_performance` return `{ as_of, window_start, models }`. Each model has
`rating: ProviderModelRating`, `mixed`, `status`, optional `error`,
`unclassified_count`, and `untrusted_count`; there is no `profile` or `tiers` field.
`mixed` contains selected/valid counts, average TPS, and first/last sample times.

The server selects the latest 10 successful, complete requests per pair within
seven days, then averages valid per-request TPS. Output-limit truncations, failed,
cancelled, incomplete, and untrusted historical requests are excluded. Invalid TPS
samples are not replaced with older requests. Streaming TPS normally excludes time
to first chunk; if the remaining generation interval is under 50 ms or first-chunk
latency is at least 80% of total upstream duration, it uses the full upstream duration
to avoid TPS spikes. Buffered TPS uses observed upstream duration. Fewer than three valid samples
are hollow points. Missing TPS is not zero; partial or whole-query errors remain
explicit. No upstream benchmark or model-usage fallback is requested by this page.
Lifecycle observation and historical metadata recovery remain enabled; effort
metadata may be retained for diagnostics but never splits displayed statistics.

### Compatibility after removing effort ratings

Existing physical `effort` columns and historical non-`common` rows are retained
without destructive migration. Only the `common` row is exposed as the single
rating; non-common rows are not displayed, copied, or exported. A model with only
old effort-specific scores is unrated until an administrator assigns a comprehensive
score; no averaging or preferred-tier fallback is performed. Backups without
`effort`, or with `effort: "common"`, are accepted; non-common effort entries are
rejected rather than silently converted.

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
cargo test -p nyro-server --no-default-features rating_
cargo check -p nyro-core
cargo check -p nyro-desktop
```

Run WebUI lint/build from `webui/`. The lightweight UI helper tests require no extra framework:

```bash
# From webui/
test_dir=$(mktemp -d /tmp/nyro-rating-tests.XXXXXX)
./node_modules/.bin/tsc src/lib/model-ratings.test.ts src/lib/model-performance.test.ts --outDir "$test_dir" \
  --module commonjs --moduleResolution node --target es2020 \
  --esModuleInterop --skipLibCheck
node --test "$test_dir/model-ratings.test.js" "$test_dir/model-performance.test.js"
```

Storage tests document opt-in disposable
PostgreSQL/MySQL conformance guards; a skipped backend is not externally verified.
The Node/CDP smoke test in `tests/webui/` uses only isolated temporary services and
checks real browser interactions without touching an existing deployment.
