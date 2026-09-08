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

## Trusted performance

`performance-smoke.mjs` uses `/api/v1/model-performance`, not legacy per-model usage.
Each response item contains `rating`, `mixed`, `status`, optional `error`, and
unclassified/untrusted diagnostic counts; `profile` and `tiers` are absent.

Coverage:

- One comprehensive score and one mixed-TPS point per exact provider/model pair;
  no effort selector, tier points, score overrides, or fallback scores.
- Seven-day window, latest ten eligible completed calls per pair, arithmetic mean
  of valid TPS, streaming first-chunk timing, invalid samples not refilled.
- Mixed effort metadata contributes to the same sample pool. Failed/incomplete/
  output-limit/cancelled, non-2xx, old and untrusted legacy rows are excluded.
- A legacy average never fills missing trusted TPS. Zero score is valid; missing
  TPS is not zero. Fewer than three valid samples are hollow points.
- Exact SVG coordinates: X starts at 0 and rounds the highest visible plotted score
  up to the next multiple of 10 (minimum ceiling 10, maximum 100; no points uses 100).
  Search filtering to max 51 gives 60, provider filtering to 73 gives 80, and clearing
  filters restores the full-data ceiling 100. Exact multiples stay unchanged.
  Y defaults to 0–100 with 50-unit expansion only above 100.
  Model names appear directly; exact overlaps list every model without jitter.
- No permanent numbered index or visible Pxx IDs. Hover/focus/tap tooltips expose
  full supplier/model identity, score, TPS and samples; pointer transfer into the
  tooltip keeps it readable, leaving or Escape dismisses it. Enter/Space and zoom work.
- Provider/model search, EN/ZH desktop/mobile, bounded mobile tooltips,
  direct loading and no whole-page overflow.
- Partial model errors preserve peers. Invalid numeric payloads, null TPS, warm/cold
  HTTP 500 remain unknown/unavailable, never invented zero; refresh recovers.
- No browser path requests legacy usage, model catalogs or benchmarks.

These scripts do not replace Rust lifecycle fault tests, SQL backend conformance,
import/export tests, or Tauri IPC execution tests.
