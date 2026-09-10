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
Scores remain 0–100 values. The X axis rounds the lowest visible plotted score down
to a multiple of 10 and the highest up to a multiple of 10, always within 0–100.
A single point retains at least a 10-point span; no points uses the full 0–100 range.
For example, 53–78 → 50–80, only 73 → 70–80, and only 100 → 90–100. Tick spacing
is 10, and search/provider filters recalculate the scale without changing scores or
point identities. Extreme scores map one marker margin inside the axis frame (half a
marker plus air), so the leftmost and rightmost markers never straddle an axis; points,
their tick labels and the envelope all share that one mapping.
Y defaults to 0–100; values above 100 expand its ceiling in 50-TPS steps with headroom.
Reasoning effort is not a score dimension, filter, or point identity. Only points on
the visible upper-right convex envelope label model names directly, adding supplier names
when needed to distinguish identical models; coincident boundary groups list every model
at that spot. Interior and dominated points never label directly — their names appear on
hover, keyboard focus, or mobile tap. Labels wrap and avoid collisions without moving
actual point coordinates; envelope positions too dense to fit are omitted with a notice.
Hover, keyboard focus, or mobile tap reveals score/TPS and sample details in a dismissible
tooltip; no permanent side index or visible point IDs remain. Search and provider filters
are retained.

### Upper-right capability–speed envelope

An open dashed line connects adjacent models on the **upper-right convex boundary**
of the currently visible valid points. This is not the complete Pareto frontier:
for A(40,200), B(60,120), C(90,100), the line joins A–C and skips B's inward dent.
B remains visible as a normal model point. Search/provider filtering and refresh
recompute the envelope; viewport scaling or display rounding never decides membership.

- Include low-sample points, preserving their sample warning and dashed icon border.
- For equal scores, retain the fastest position; for equal TPS, retain the strongest.
  All models at exactly the same boundary coordinates share membership.
- Preserve collinear points along the downward-sloping boundary. A model below a
  boundary segment is not a member merely because no single model dominates it.
- With zero points draw nothing; with one optimal position draw no line segment,
  but show envelope membership in every corresponding model's details.
- Uniform slate dashed strokes render below points and labels with no fill, closure,
  axis extension, or pointer capture. Provider colors and the low-sample marker meaning
  stay unchanged. Details and accessible point descriptions identify envelope members.
- Only boundary members are labeled directly; an interior or dominated point gets no
  direct label, and a coincident boundary position keeps every model in one label.
  Hover, focus, or tap surfaces any non-boundary name.
- Horizontal/vertical gridlines are removed. Solid left/bottom axes, short tick marks,
  tick values, axis titles, and model-name leader lines remain.

The line bounds current observations, not guaranteed performance or recommendations.
Segment interiors do not correspond to measured models, and low-sample uncertainty
still applies. Computation uses raw score/TPS values with machine-precision-only
orientation tolerance, never one-decimal display values.

`GET /api/v1/model-performance` (optional `provider_id`) and desktop
`get_model_performance` return `{ as_of, window_start, models }`. Each model has
`rating: ProviderModelRating`, `mixed`, `status`, optional `error`,
`unclassified_count`, and `untrusted_count`; there is no `profile` or `tiers` field.
`mixed` contains selected/valid counts, average TPS, and first/last sample times.

A group is one rated prefix at one provider. `variants` lists the distinct upstream
model names that matched the prefix, each with its own `mixed` sampled from that
variant's latest ten retained calls. The group's `mixed` is the sample-weighted merge
of its variants: counts are sums that legitimately exceed ten (up to ten per variant),
timestamps span the merged window, and a group with no usable TPS keeps a null average.

Performance uses the **same latest-ten retained-call sampling and TPS calculation as
model usage statistics**, not a separate completion-qualified metric. There is no
additional seven-day, HTTP-status, completion-state, reasoning-effort, or metadata-version
filter. `window_start` is null; `as_of` describes when the response was fetched.
Unknown completion (including MiniMax responses whose terminal is not recognized)
does not invalidate usable output-token and timing data. Invalid TPS samples among
the selected ten are not replaced with older requests.

The backend shares its per-request TPS helper with model usage: streaming is detected
by the stream flag or observed chunks; generation time normally subtracts the first
chunk wait, with the existing 50 ms / 80% non-incremental fallback. Otherwise it uses
upstream duration, falling back to total duration when upstream timing is absent.
Mean TPS is the arithmetic mean of valid per-call values, not total tokens divided
by total time. Points retain full numeric precision; displayed TPS uses one decimal.
Each point is drawn as its provider's vendor icon, never as a plain dot, and always
inside its own marker box: an icon file that declares another intrinsic size is
normalized to fill that box instead of painting over the plot. Identity comes from the
provider's canonical preset/vendor key, the same value the admin surface reports as
`provider_icon` (aliased to the shipped icon, e.g. `ark-coding` → doubao), then from
its display name and API host, then from the provider initial. The wire protocol never
participates: `openai-compatible` and `openai-responses` describe a request format, so
matching them branded every relay as OpenAI. The `custom` preset ships no usable vector
mark, so such a provider keeps whatever its name and host identify. Fewer than three valid samples
switch that marker to a dashed border at reduced opacity; missing data and read failures are
explicit; no active upstream benchmark is performed. Existing completion metadata
and historical recovery remain diagnostic only, not an eligibility gate. Legacy
`unclassified_count` and `untrusted_count` response fields are retained as zeros for
compatibility and no longer shown as invalid-call counts.

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
