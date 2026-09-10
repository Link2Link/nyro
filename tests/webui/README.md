# WebUI browser smoke tests

These dependency-free Node scripts drive real Chromium through CDP using native
`fetch` and `WebSocket`. They exercise a disposable admin server, never a running
Nyro instance. They do not install packages, commit, or deploy.

## Prerequisites and commands

- Node 22+ with native `WebSocket`.
- Chromium: set `CHROME_BIN` if none of the cached/system paths exists.
- Performance and log-outcomes smokes additionally need Python 3.9+ with
  standard-library `sqlite3`.
- Build the current backend and frontend before running; coordinate shared artifacts.

```bash
cargo build -p nyro-server --no-default-features
(cd webui && npm run build)
node tests/webui/model-ratings-smoke.mjs
node tests/webui/performance-smoke.mjs
node tests/webui/log-outcomes-smoke.mjs
```

Optional absolute-path overrides: `NYRO_SMOKE_BINARY`, `NYRO_SMOKE_WEBUI`,
`CHROME_BIN`. Missing artifacts fail immediately; no existing server is reused.

Lightweight helper tests need no extra framework:

```bash
cd webui
test_dir=$(mktemp -d /tmp/nyro-ui-tests.XXXXXX)
./node_modules/.bin/tsc src/lib/model-ratings.test.ts src/lib/model-performance.test.ts \
  --outDir "$test_dir" --module commonjs --moduleResolution node --target es2020 \
  --esModuleInterop --skipLibCheck
node --test "$test_dir/model-ratings.test.js" "$test_dir/model-performance.test.js"
```

## Isolation and evidence

Each run creates a new `nyro-*-smoke-*` directory under the OS temporary directory,
starts Nyro in admin-only mode on an ephemeral loopback port with fresh SQLite,
and uses a new Chrome profile. `NYRO_*` child settings are stripped; performance
also strips proxy environment settings.

Providers and single comprehensive scores are saved through the real Admin API.
The ratings fixture supplies local model catalogs. The performance fixture is an
HTTP trap: any catalog, benchmark, or upstream request fails the test.

Performance's Python seed discovers exactly one existing `.db` inside this run's
fresh data directory, verifies real paths, opens with `mode=rw`, and inserts logs
only into an empty request-log table. The input and DB remain scratch-local.
Seeded metadata represents completion and final-wire effort evidence, not execution
of live lifecycle capture or historical recovery. Effort metadata never splits
performance results.

All spawned children and fixture servers are stopped in `finally`, including failed
assertions. Scratch directories remain for review: `report.json` records checks,
errors, child logs and screenshots; performance also saves seed logs and the real
backend snapshot. Printed `REPORT` and `RESULT` identify evidence and cleanup.
Unexpected browser console/runtime/network errors fail; injected failures are
recorded separately. Screenshots are evidence, not pixel-golden assertions.

## Model ratings

`model-ratings-smoke.mjs` covers:

- Single-score API validation, integer 0–100, explicit unrated and unknown states.
- One-score editing, confirmed clearing, persistence and independent provider scores.
- Global catalog/route/saved-score union, retained disabled/stale models, sorting
  with unrated last, score/state/provider/text filters.
- Available Models sharing the same exact provider/model score across route mappings.
- Save failure retaining drafts and persisted values; list failures remaining unknown
  rather than unrated, with retry recovery and unsafe filters/editing gated.
- Strict catalog HTTP 502 retaining scores and editability, not marking missing.
- EN/ZH desktop/mobile screenshots, no page overflow, usable mobile editor.

## Performance: same TPS as logs and model usage

Rebuild the current backend with normal features and the frontend before this smoke;
when sharing a checkout, wait for the build owner to confirm both artifacts are ready:

```bash
cargo build -p nyro-server
(cd webui && npm run build)
node --check tests/webui/performance-smoke.mjs
node tests/webui/performance-smoke.mjs
```

The browser uses one `/api/v1/model-performance` snapshot, not per-model usage calls.
Ratings are prefix-shared (`PUT /api/v1/model-ratings?prefix=...`), and one page row is
one matched **prefix × provider group**. Each group carries `model_prefix`,
`provider_id`, `score`, `score_updated_at`, a merged `mixed`, and one `variants[]` entry
per distinct upstream model name; `profile` and `tiers` remain absent. The Node fixture
independently compares **every logged stream's variant** `mixed.average_tps` with
`/api/v1/providers/:id/model-usage?model=...` using exact numeric equality, saves both
responses in `report.json`, and asserts the group's `mixed` is the plain per-variant sum
with sample-weighted TPS. `window_start` is `null`: there is no seven-day cutoff beyond
whatever request logs are still retained.

Coverage:

- One comprehensive score per matched prefix and one merged mixed-TPS point per
  prefix × provider group; no effort selector, tier points, score overrides, or
  fallback scores. Each variant samples its own latest ten retained calls, so a group
  served by two upstream variants legitimately reports twenty selected/valid samples.
- A rated prefix with no retained call produces **no row at all** — never a zero-TPS
  row — while `/model-usage` still reports its empty window.
- Select the latest ten raw retained logs by request time before validating TPS.
  The shared logs/model-usage formula uses `output_tokens`, `latency_upstream_ms`
  (or total latency fallback), `is_stream`/chunk count and `stream_first_chunk_ms`.
  Streaming generation timing preserves the legacy non-incremental-response fallback.
  The seeded 100-token 2000ms/500ms-TTFT and 50-token 1000ms calls average to 175/3 TPS.
  New `performance_*` timings intentionally disagree to prove they are not used.
- Failed/incomplete/output-limit/cancelled/unknown completion, non-2xx statuses and
  metadata versions 0, 1 and 99 do not disqualify valid token/timing samples. Mixed
  reasoning-effort metadata stays in the same sample pool. Legacy-valid logs are
  plotted without an untrusted-history warning.
- MiniMax-M3 with version 1, unknown completion, 2007 output tokens, 20617ms upstream
  latency and 1798ms TTFT yields `2007 / ((20617 - 1798) / 1000)` TPS, shown as
  **106.6 tok/s**. A retained log older than seven days with no metadata fields and
  total-latency fallback also contributes. No upstream request is made.
- Two upstream variants (`model/dual`, `model/dual-0813`) share one prefix and one row:
  the group must keep both variants, report `10 + 10 = 20` selected and valid samples,
  merge TPS sample-weighted (100 and 60 → **80.0 tok/s**), inherit the shared score, and
  still be plotted with `20 / 20` in the diagnostics table. The same client contract is
  enforced negatively by CDP injection: a group claiming more samples than its declared
  variants can hold is rejected outright rather than plotted.
- Only invalid tokens/timing produces missing TPS in the real fixture.
  Invalid latest samples are not refilled from older valid calls. Zero score is
  valid; missing TPS is not zero. Fewer than three valid samples are hollow points.
- Exact SVG coordinates retain full backend precision while user-visible TPS uses
  one decimal. X rounds the lowest visible plotted score down and the highest up to
  ten-point ticks (within 0–100, at least a 10-point single-score span; no points uses
  the full range). Search filtering to scores 50–51 gives 50–60, provider filtering
  to 73 gives 70–80, and clearing filters restores the full-data 0–100 range.
  Y defaults to 0–100 with 50-unit expansion only above 100; fixture maximum 225
  deliberately keeps the expanded ceiling at 250.
  Only envelope boundary points label model names directly; exact coincident boundary
  positions list every model without jitter. Interior and dominated points never label
  directly — their names surface only in hover/focus/tap details.
- No permanent numbered index or visible Pxx IDs. Hover/focus/tap tooltips expose
  full supplier/model identity, score, one-decimal TPS and samples; pointer transfer
  into the tooltip keeps it readable, leaving or Escape dismisses it. Enter/Space
  and zoom work.
- Provider/model search, EN/ZH desktop/mobile, bounded mobile tooltips,
  direct loading and no whole-page overflow.
- Invalid numeric payloads, fabricated merged over-counts, null TPS, warm/cold
  HTTP 500 remain unknown/unavailable, never invented zero; refresh recovers.
- No browser path requests per-model usage, model catalogs or benchmarks. All seed,
  API comparison, fault injection and browser work stays inside fresh local fixtures.

### Visible-model convex capability–speed envelope

The performance smoke also verifies the **upper-right convex envelope**, not every
Pareto-optimal model. Its independent oracle enumerates negative-slope supporting
lines over deterministic coordinates; it neither imports the frontend hull helper
nor trusts the SVG membership metadata as its expected result.

- The unchanged real SQLite/API fixture has a single dominating model at `(100,225)`:
  it has a tooltip membership badge but **no envelope line**. All original latest-ten,
  exact `/model-usage` parity, one-decimal TPS, and auto-X-domain checks still run.
- After those baseline and failure/recovery checks, CDP substitutes geometry-only
  `/model-performance` snapshots using the real response shape and local providers.
  Explicit fixtures cover A `(40,200)`, B `(60,120)`, C `(90,100)` (only A/C are on the
  envelope, even though B is nondominated), a genuine convex bend, same-score fastest
  and same-TPS strongest ties, identical coordinates across every exact model key,
  negative-slope collinear members, singleton/empty data, score zero, full-precision
  TPS, and missing models excluded from the boundary. One/two-sample hollow
  models remain eligible; sample-count warnings and actual scores/TPS do not change.
- Actual SVG vertices must map through the **shared visible X minimum/maximum and Y
  scale**, lie at real boundary coordinates, and cover all supporting corners. The
  line is one dashed, unfilled, pointer-transparent **open polyline**, with no closing
  polygon, axis connections, or horizontal/vertical tails. Collinear members may be
  rendered as vertices or lie on the same straight segment, but all retain membership.
- Only boundary members are labeled directly, checked against the independent oracle
  in every scenario and the real fixture: no direct label may list an interior or
  dominated point, and every boundary key gets its label (coincident boundary models
  share one position label) unless the too-dense notice appears. The real fixture's
  interior coincident "overlap" pair is verified to have **no** direct label while its
  hover details still name every member.
- Every plotted model's tooltip is opened by its stable point ID and checked for the
  exact `data-point-key` badge only when it belongs to the independent expected
  boundary, including coincident models. EN/ZH badge text, low-sample warnings and
  one-decimal TPS are verified. Hover/focus never moves actual points.
- Text/provider filters recompute the boundary locally: removing C promotes B,
  single-result/no-result filters remove the line, and clearing filters restores it.
  Raw scores, TPS and IDs stay unchanged while X/Y rescale. Zoom and input-order
  reversal cannot alter underlying SVG geometry or membership; no extra snapshot
  requests are made merely for filtering.
- Long horizontal/vertical lines are classified rather than banning SVG `line`
  elements: only the two plot-edge **axis lines** may span the plot; short ticks,
  numeric labels, localized axis titles and point-anchored model-label leaders remain.
  Leaders are explicitly exempt from gridline detection.

`report.json` adds `envelopeScenarios` with the injected snapshots, explicit boundary
keys, actual SVG geometry and axes. Screenshots include convex-vs-Pareto, true bend,
tie/duplicate, collinear, filter-recomputed and Chinese badge evidence. The smoke
restores the real snapshot and compares all backend model statistics with the
original response: geometry injection never changes stored scores/logs or API TPS
semantics. All work remains inside the disposable server/browser lifecycle above.

These scripts do not replace Rust lifecycle fault tests, SQL backend conformance,
import/export tests, or Tauri IPC execution tests.

## Log outcomes and failure observability

`log-outcomes-smoke.mjs` reuses the disposable server + Python SQLite seed +
CDP browser pattern for failure observability. The seed inserts the versioned
outcome columns (`client_request_id`, `attempt_index`, `outcome_version`,
`attempt_outcome`, `failure_kind/stage`, `error_message`, `error_causes_json`,
`payload_metadata_json`, `payload_cleared_at`) plus correlated rows in
`request_results`, so every UI assertion runs against real API responses —
an HTTP trap fails the run on any upstream/catalog call.

Node-side oracle (real admin API) and browser coverage:

- A MiniMax-like HTTP 200/200 upstream read failure with `outcome_version=1`,
  `attempt_outcome=failed` is `is_error=true` / effective outcome `error`,
  while its response headers carry `x-nyro-request-id` and the SSE error body
  carries the same `request_id`. A retry chain (attempt 0 → 502 failure on
  provider A, attempt 1 → completed on provider B) yields two attempt rows
  with one final `request_results` summary and an in-dialog cross-attempt
  navigation. Legacy version-0 `failed` markers and future outcome versions
  stay `unknown`, never errors.
- The Logs page shows one outcome badge (`error|completed|cancelled|
  output_limited|unknown`) beside the raw client HTTP number plus the upstream
  HTTP line. The attempt-result filter `error` includes the HTTP200 read
  failure and excludes cancelled/output-limited/unknown; the raw client-HTTP
  filter is independent and combines with AND. Every filter combination is
  first asserted through `/api/v1/logs`.
- The detail dialog exposes failure kind/stage/message and the parsed cause
  chain, correlated attempts, the final client result summary, and bounded
  payload evidence: a >1 MiB truncated body rendered as exact 512 KiB head +
  512 KiB tail segments with the computed missing-middle byte count,
  base64-encoded binary bodies shown verbatim, `not_retained` (recording
  disabled), `absent`, legacy unknown-capture rows with raw text, separate
  header accounting (observed vs retained bytes and omitted-header counts),
  and the `Manually cleared` marker after destructive payload clearing.
  Cancelled and output-limited attempts retain payloads without entering any
  error filter or count.
- Provider / model / API-key usage dialogs count attempts (not client
  requests): success is confirmed-completed only, error counts include the
  HTTP200 read failure, unknown/cancelled/output-limited counts are shown,
  and the five exclusive buckets sum to the attempt total.
- The logging health snapshot from `/api/v1/logging/status` renders queue-full,
  channel-closed and database-write drop counters.
- Destructive confirmations spell out their exact scope (payload clearing
  covers ALL logs including errors and keeps classifications; error deletion
  removes only confirmed errors — HTTP 4xx/5xx or versioned failure/timeout —
  and excludes cancelled/output-limited/unknown), verified in EN and ZH copy.
  Confirming error deletion from the Logs page invalidates the logs, stats and
  usage query families so the stats page updates after SPA navigation without
  a reload.
- EN/ZH desktop and mobile screenshots with no whole-page overflow, no
  unexpected console/runtime/network errors, and zero trap hits.
