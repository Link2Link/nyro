# WebUI browser smoke tests

These dependency-free Node scripts drive real Chromium through CDP using native
`fetch` and `WebSocket`. They exercise a disposable admin server, not a running
Nyro instance. They never install packages, commit, or deploy.

## Prerequisites and commands

- Node 22+ with native `WebSocket`.
- Chromium: set `CHROME_BIN` if none of the cached/system paths in the scripts exists.
- Performance smoke additionally needs Python 3.9+ with standard-library `sqlite3`.
- Build the current backend and frontend before running (coordinate with any other
  agents editing/building those artifacts):

```bash
cargo build -p nyro-server --no-default-features
(cd webui && npm run build)
node tests/webui/model-ratings-smoke.mjs
node tests/webui/performance-smoke.mjs
```

Optional absolute-path overrides: `NYRO_SMOKE_BINARY`, `NYRO_SMOKE_WEBUI`,
`CHROME_BIN`. Missing artifacts fail immediately; neither script builds artifacts
or silently reuses an existing server.

## Isolation and evidence

Each run creates a new `nyro-*-smoke-*` directory under the OS temporary directory,
starts Nyro in admin-only mode on an ephemeral loopback port with fresh SQLite,
and uses a new Chrome profile. `NYRO_*` child environment settings are stripped;
the performance test also strips proxy environment settings.

Providers and rating profiles are saved through the real Admin API. The ratings
fixture supplies local model catalogs. The performance fixture is an HTTP trap:
any catalog, benchmark, or upstream request fails the test. No external upstream
or existing application DB is used.

Performance's Python seed discovers exactly one existing `.db` **inside that
run's fresh data directory**, verifies real paths, opens SQLite with `mode=rw`,
and inserts logs only into an empty request-log table. Its input and DB paths
must stay inside scratch. It writes only that disposable DB, never a DB path
from the environment. Seeded scalar metadata represents final-wire effort and
completion evidence; this test does not claim to execute live request lifecycle
capture or historical migration recovery.

All spawned Chrome/Nyro/Python children and fixture HTTP servers are stopped in
`finally`, including failed assertions. Scratch directories remain for review:
`report.json` contains checks, errors, child logs and screenshot paths; performance
also saves deterministic seed logs and the real backend snapshot. Printed `REPORT`
and `RESULT` lines identify the evidence and cleanup status. Unexpected browser
console/runtime/network errors fail; deliberately injected HTTP failures are
recorded separately. Screenshots are evidence, not pixel-golden assertions.

## Model ratings

`model-ratings-smoke.mjs` covers:

- Legacy single-score API validation and explicit common-only unrated behavior.
- The new popup's optional common score and five explicit inherit/override modes;
  atomic profile saves, zero, equal-to-common overrides, fallback and override-only
  profiles. Disabling common preserves overrides. All-null Save requests confirmation
  before clearing; a separate confirmed whole-profile clear removes common and all overrides.
- Global catalog/route/saved-profile union, retained disabled/stale models,
  sorting with unscored last, score/state/provider/text filters, and persistence.
- Available Models shares the exact provider/model profile across route mappings;
  providers remain independent.
- Save failure retains drafts/persisted values. Profile-list failure is unknown,
  not unrated; unsafe filters/editing are gated and retry recovers. Real catalog
  outages return strict HTTP 502, preserving ratings and editability while marking
  catalog state unknown, not missing.
- EN/ZH desktop/mobile screenshots, no page overflow, usable mobile editor.

## Trusted performance

`performance-smoke.mjs` uses the batch `/api/v1/model-performance` contract, not
legacy per-model usage responses. It covers:

- Real profiles plus completion-aware statistics: seven-day window, latest ten
  eligible completed calls **per group**, arithmetic mean of valid TPS, stream
  first-chunk timing, invalid samples not refilled, older high-TPS outliers excluded.
- Final-wire `minimal` seeds map to `low`; five tier groups use their own samples.
  Failed/incomplete/output-limit/cancelled, non-2xx, old and unknown legacy rows
  do not become trusted samples. Unclassified/untrusted diagnostics are checked.
  A legacy average remains independently available but never fills trusted unknown.
- No overrides gives common × mixed; any override (even equal common) gives tiers
  with backend effective scores and no mixed fallback. Score zero is valid; missing
  TPS is not zero. One/two valid samples are hollow, at least three are solid.
- Exact SVG coordinates with fixed X 0–100, default Y ceiling 200, and expansion in
  50-unit steps above 200. Grouped exact overlaps use ×N without jitter/centroids;
  near-point selection exposes each real candidate.
- Pxx labels map to complete provider ID/name, exact model, tier, source, score,
  TPS and sample counts in the numbered index. IDs remain stable under filters and
  zoom. Focus links chart/index; Enter/Space pins; Escape clears; members of exact
  overlaps can be selected independently.
- Provider/tier/search filtering, EN/ZH desktop/mobile screenshots, mobile index
  below chart, usable direct-load layout and no whole-page overflow.
- Partial profile statistics errors preserve others. Invalid numeric payloads,
  null TPS, and warm/cold batch HTTP 500 remain unknown/unavailable, never invented
  zero; refresh recovers. Browser routes must not request legacy usage, catalogs,
  or benchmarks.

These HTTP/browser integration scripts do not replace Rust lifecycle fault tests,
SQL backend conformance, import/export tests, or Tauri IPC execution tests.
