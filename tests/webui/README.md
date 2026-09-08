# WebUI browser smoke tests

## Model ratings

`model-ratings-smoke.mjs` uses Node's built-in `fetch` and `WebSocket` to drive a
real headless Chromium instance through the Chrome DevTools Protocol. It does
not need npm packages, Playwright, a browser extension, or a test framework.

### Prerequisites and command

- Node 22+ with built-in `WebSocket`.
- A Chromium executable (set `CHROME_BIN` if it is not at one of the cached or
  system paths listed in the script).
- A current server binary and WebUI build:

```bash
cargo build -p nyro-server --no-default-features
(cd webui && npm run build)
node tests/webui/model-ratings-smoke.mjs
```

Optional absolute-path overrides: `NYRO_SMOKE_BINARY`, `NYRO_SMOKE_WEBUI`, and
`CHROME_BIN`. Missing artifacts fail immediately; the script does not install
packages, build artifacts, or silently reuse a running Nyro instance.

### Isolation and evidence

The test creates a new `nyro-model-ratings-smoke-*` directory under the operating
system temporary directory, starts Nyro in admin-only mode with a fresh SQLite
DB and ephemeral loopback port, and seeds data through its actual Admin API.
A local in-process HTTP fixture supplies upstream model catalogs. No existing
Nyro database or server is used. `NYRO_*` environment variables are removed from
the child server environment so local deployment settings cannot redirect it.

Chrome uses a fresh temporary profile. All spawned Chrome/Nyro children and the
fixture HTTP server are stopped in `finally`, including assertion failures.
The scratch directory is retained for review; it contains screenshots,
`report.json` (assertions, error events, child logs), and the disposable SQLite
DB. A printed `REPORT` line gives its exact path. No deployment or commit occurs.

### Coverage

- Actual API rejects out-of-range, fractional, string, and null scores; unscored
  GET is explicit `status: unrated` with `score: null`.
- Single editor input validation; save `0`, reload persistence, and confirmed
  clear back to unscored rather than zero.
- Flat global management across providers, retained missing-catalog/disabled
  rows, both score directions with unscored last, and range/state/provider/text
  filters.
- Existing Available Models editing shares a pair's rating across route mappings
  while different providers stay independent.
- English and Chinese desktop/mobile screenshots, page-overflow and minimum
  mobile content-width checks, and an interactable mobile editor.
- Injected save failure preserves the draft and old persisted score; rating-list
  failure displays unknown, disables unsafe actions/filters, and recovers on retry.
- A real local upstream catalog outage produces HTTP 502 through the strict
  `require_catalog=true` API; the page preserves ratings/editability and labels
  catalog status unknown rather than incorrectly claiming missing models.
- Browser console/runtime/unexpected network errors fail the run. Deliberate
  injected 503 and strict-catalog 502 network errors are recorded separately as
  expected evidence.

This is an HTTP/browser integration test, not a Tauri IPC execution test or a
replacement for SQL backend conformance and import/export unit tests.
