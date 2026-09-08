# Provider-local model ratings

[简体中文](model-ratings_CN.md)

## Purpose

Nyro stores a manually assigned **comprehensive capability score** for each exact
`(provider_id, upstream_model)` pair. Scores are integers from **0 to 100**. They
are subjective administrative labels, not IQ, benchmark results, probabilities,
or a ratio scale. They exclude latency, throughput, price, and availability.

The first release provides individual editing, persistence, global comparison,
provider-copy snapshots, and configuration backup. **Scores do not change routing
and this release adds no charts.** Future chart/router consumers must explicitly
handle unrated models rather than defaulting them to a numerical score.

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
./node_modules/.bin/tsc src/lib/model-ratings.test.ts --outDir "$test_dir" \
  --module commonjs --moduleResolution node --target es2020 \
  --esModuleInterop --skipLibCheck
node --test "$test_dir/model-ratings.test.js"
```

Storage tests document opt-in disposable
PostgreSQL/MySQL conformance guards; a skipped backend is not externally verified.
The Node/CDP smoke test in `tests/webui/` uses only isolated temporary services and
checks real browser interactions without touching an existing deployment.
