# WebUI browser smoke tests

These dependency-free Node scripts drive real Chromium through CDP using native
`fetch` and `WebSocket`. They exercise a disposable admin server, never a running
Nyro instance. They do not install packages, commit, or deploy.

## Prerequisites and commands

- Node 22+ with native `WebSocket`.
- Chromium: set `CHROME_BIN` if none of the cached/system paths exists.
- Performance smoke additionally needs Python 3.9+ with standard-library `sqlite3`.
- Build the current backend and frontend before running; coordinate shared artifacts.

```bash
cargo build -p nyro-server --no-default-features
(cd webui && npm run build)
node tests/webui/model-ratings-smoke.mjs
node tests/webui/performance-smoke.mjs
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
The Node fixture independently compares **every rated pair's** `mixed.average_tps`
with `/api/v1/providers/:id/model-usage?model=...` using exact numeric equality, and
saves both responses in `report.json`. Each snapshot item has one `rating` and
`mixed` statistic; `profile` and `tiers` remain absent. `window_start` is `null`:
there is no seven-day cutoff beyond whatever request logs are still retained.

Coverage:

- One comprehensive score and one mixed-TPS point per exact provider/model pair;
  no effort selector, tier points, score overrides, or fallback scores.
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
- Only no history or invalid tokens/timing produce missing TPS in the real fixture.
  Invalid latest samples are not refilled from older valid calls. Zero score is
  valid; missing TPS is not zero. Fewer than three valid samples are hollow points.
- Exact SVG coordinates retain full backend precision while user-visible TPS uses
  one decimal. X rounds the lowest visible plotted score down and the highest up to
  ten-point ticks (within 0–100, at least a 10-point single-score span; no points uses
  the full range). Search filtering to scores 50–51 gives 50–60, provider filtering
  to 73 gives 70–80, and clearing filters restores the full-data 0–100 range.
  Y defaults to 0–100 with 50-unit expansion only above 100; fixture maximum 225
  deliberately keeps the expanded ceiling at 250.
  Model names appear directly; exact overlaps list every model without jitter.
- No permanent numbered index or visible Pxx IDs. Hover/focus/tap tooltips expose
  full supplier/model identity, score, one-decimal TPS and samples; pointer transfer
  into the tooltip keeps it readable, leaving or Escape dismisses it. Enter/Space
  and zoom work.
- Provider/model search, EN/ZH desktop/mobile, bounded mobile tooltips,
  direct loading and no whole-page overflow.
- Partial model errors preserve peers. Invalid numeric payloads, null TPS, warm/cold
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
  TPS, and missing/error models excluded from the boundary. One/two-sample hollow
  models remain eligible; sample-count warnings and actual scores/TPS do not change.
- Actual SVG vertices must map through the **shared visible X minimum/maximum and Y
  scale**, lie at real boundary coordinates, and cover all supporting corners. The
  line is one dashed, unfilled, pointer-transparent **open polyline**, with no closing
  polygon, axis connections, or horizontal/vertical tails. Collinear members may be
  rendered as vertices or lie on the same straight segment, but all retain membership.
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
