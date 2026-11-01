# System monitor

[简体中文](<system-monitor_CN.md>)

The **System Monitor** navigation entry opens `/system-monitor`. It is a read-only
view of the environment running Nyro, not the browser's device. It uses the same
rounded glass panels and automatic light/dark theme as the rest of the console,
with English and Simplified Chinese labels.

## Panels and controls

- CPU usage, logical/physical core counts, model, available frequency, and an
  expandable per-logical-core view.
- Memory used/total/available, swap usage, and Nyro process memory/CPU/PID.
- The most-full storage volume and individual mount capacity/available-space
  meters. Capacities are not added across mounts because mounts can share storage.
- Receive/send throughput trends and per-interface cumulative byte counters.
  Summary figures exclude loopback (`lo`, `lo0`, `Loopback…`); virtual interfaces
  can still count the same traffic more than once.
- Client-to-admin round-trip time (HTTP or desktop IPC), including collection and
  serialization time. This is **not** upstream-model latency or an ICMP ping.
- Host, OS, architecture, kernel, and OS uptime.

The default polling interval is 3 seconds; 5 and 10 seconds are also available.
Pause stops automatic polling; manual refresh still works. Polling is not kept
running in background tabs or after leaving the page. Failed refreshes keep the
last successful snapshot, visibly marked disconnected/stale rather than live.
Trends retain at most 60 samples and 10 minutes in the query cache in memory,
not in the database or persistent storage. Long gaps are not joined by chart lines.

## Collection and transport

`AdminService::get_system_status()` in
[system_status.rs](<../../crates/nyro-core/src/admin/system_status.rs>) is shared by:

- Authenticated `GET /api/v1/system/status`, using the existing admin `{ "data": … }`
  envelope and authentication middleware.
- Desktop IPC command `get_system_status`.

A process-wide collector owns `sysinfo` system/disk/network state and a 2-second
snapshot cache. Refreshes and locking run on Tokio's blocking pool; no background
thread or timer is started for collection. CPU and network differences use prior
samples, with monotonic elapsed time for byte rates. Initial CPU/network rates,
new interface rates, and reset counters are unknown (`null`), not zero. An actual
idle sample is zero. First-frame per-core values are an empty array. Process CPU
is divided by logical core count to report a whole-machine 0–100% percentage.

The response includes `sampled_at` (epoch milliseconds),
`sample_interval_seconds`, host identity, `cpu`, `memory`, `disks`, `networks`, and
`process`. Byte quantities use bytes; rates use bytes/second; frequency uses MHz.
The UI formats byte quantities with IEC units. Unsupported/unavailable rate
values display a dash. No upstream endpoints are contacted, no speed-test payload
is transferred, and no database schema change is required.

## Interpretation limits

Container CPU/memory visibility is platform/runtime-dependent and must not be
assumed to equal cgroup quotas. Disk and network visibility also follows the
process environment and its permissions. Network throughput is observed usage,
**not** internet speed, provisioned bandwidth, or the physical link capacity.
Cumulative counters may reset when an interface or the OS restarts. Monitoring is
not a health certification and does not infer gateway/provider availability.

## Verification

```bash
cargo test -p nyro-core system_status
cargo test -p nyro-server system_status
cargo check -p nyro-core -p nyro-server
(cd webui && npm run build)
node tests/webui/system-monitor-smoke.mjs
```

The browser smoke uses Chromium/CDP to fulfill all page, asset, and API requests
from the built UI and deterministic fixtures, without starting an HTTP server or
using an existing instance. See [browser test instructions](<../../tests/webui/README.md>)
for Chromium prerequisites.
