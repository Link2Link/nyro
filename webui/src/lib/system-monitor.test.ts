import { deepEqual, equal, ok } from "node:assert/strict";
import { test } from "node:test";
import { backend } from "./backend";
import {
  appendMonitorHistory, busiestDisk, formatBytes, formatPercent, formatUptime, isLoopback,
  networkSummary, percent, type MonitorSnapshot, type SystemStatus,
} from "./system-monitor";

const network = (name: string, receive: number | null, transmit: number | null) => ({
  name, received_bytes: 8192, transmitted_bytes: 4096,
  receive_bytes_per_second: receive, transmit_bytes_per_second: transmit,
});
const status = (sampledAt: number): SystemStatus => ({
  sampled_at: sampledAt, sample_interval_seconds: null, hostname: "test", os: "Linux",
  kernel_version: null, architecture: "x86_64", uptime_seconds: 3600,
  cpu: { brand: "Test CPU", logical_cores: 4, physical_cores: 2, usage_percent: null, frequency_mhz: null, cores: [] },
  memory: { total_bytes: 1024, used_bytes: 512, available_bytes: 512, swap_total_bytes: 0, swap_used_bytes: 0 },
  disks: [], networks: [network("eth0", null, null)], process: { pid: 1, memory_bytes: 64, cpu_percent: null },
});

test("monitor formatters distinguish unavailable, zero and byte/rate units", () => {
  equal(formatBytes(null), "—");
  equal(formatBytes(Number.NaN), "—");
  equal(formatBytes(-1), "—");
  equal(formatBytes(0), "0 B");
  equal(formatBytes(1024), "1.0 KiB");
  equal(formatBytes(1024 ** 3), "1.0 GiB");
  equal(formatBytes(1024, true), "1.0 KiB/s");
  equal(formatPercent(null), "—");
  equal(formatPercent(0), "0.0%");
  equal(formatPercent(33.333), "33.3%");
  equal(percent(0, 0), null);
  equal(percent(5, 10), 50);
  equal(percent(20, 10), 100);
  equal(percent(-1, 10), 0);
  equal(percent(Infinity, 10), null);
  equal(formatUptime(90061, false), "1d 1h 1m");
  equal(formatUptime(90061, true), "1天 1时 1分");
});

test("network totals exclude loopback, preserve unknown rates and sum real idle zero", () => {
  for (const name of ["lo", "lo0", "Loopback Pseudo-Interface 1"]) ok(isLoopback(name));
  ok(!isLoopback("local-network"));
  deepEqual(networkSummary([network("eth0", 10, 20), network("wlan0", 30, 40), network("lo", 100, 100)]), {
    receive: 40, transmit: 60, received: 16384, transmitted: 8192, count: 2,
  });
  equal(networkSummary([network("eth0", null, null)]).receive, null);
  equal(networkSummary([network("eth0", 10, 10), network("eth1", null, 0)]).receive, null);
  equal(networkSummary([network("eth0", 0, 0)]).receive, 0);
  equal(networkSummary([]).receive, null);
  equal(networkSummary([network("lo0", 100, 100)]).count, 0);
});

test("disk summary selects the highest usage ratio, never adds shared mounts", () => {
  const disk = (mount: string, total: number, available: number) => ({ name: "test", mount_point: mount, total_bytes: total, available_bytes: available, file_system: "ext4", removable: false });
  equal(busiestDisk([]), null);
  equal(busiestDisk([disk("empty", 0, 0)]), null);
  equal(busiestDisk([disk("/", 100, 40), disk("/data", 200, 10)])?.mount_point, "/data");
});

test("history keeps warm-up nulls and deduplicates cached backend samples", () => {
  const first = status(1000);
  const history = appendMonitorHistory(undefined, first, 5, 1000, 3000);
  equal(history[0].cpu, null);
  equal(history[0].receive, null);
  equal(history[0].memory, 50);
  const snapshot: MonitorSnapshot = { status: first, latency: 5, history };
  const repeated = appendMonitorHistory(snapshot, first, 10, 1200, 3000);
  equal(repeated.length, 1);
  equal(repeated[0].timestamp, 1000);
  equal(repeated[0].latency, 10);
  const next = appendMonitorHistory(snapshot, status(4000), 5, 4000, 3000);
  equal(next.length, 2);
});

test("history breaks after pauses and caps retained points and age", () => {
  let snapshot: MonitorSnapshot | undefined;
  for (let i = 0; i < 80; i++) {
    const next = status(i * 3000);
    snapshot = { status: next, latency: 5, history: appendMonitorHistory(snapshot, next, 5, i * 3000, 3000) };
  }
  equal(snapshot!.history.length, 60);
  const gap = appendMonitorHistory(snapshot, status(300000), 3, 300000, 3000);
  equal(gap.length, 60);
  equal(gap[gap.length - 2].latency, null);
  equal(gap[gap.length - 1].latency, 3);
  const expired = appendMonitorHistory(snapshot, status(1000000), 3, 1000000, 3000);
  equal(expired.length, 1);
});

test("system metrics use the authenticated read-only HTTP adapter and preserve abort signal", async () => {
  const previousFetch = globalThis.fetch;
  const storage = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
  Object.defineProperty(globalThis, "localStorage", { configurable: true, value: { getItem: () => "admin-test" } });
  const controller = new AbortController();
  const fixture = status(1);
  globalThis.fetch = async (url, init) => {
    equal(url, "/api/v1/system/status");
    equal(init?.method, "GET");
    equal((init?.headers as Record<string, string>).Authorization, "Bearer admin-test");
    equal(init?.signal, controller.signal);
    return new Response(JSON.stringify({ data: fixture }));
  };
  try {
    deepEqual(await backend("get_system_status", undefined, { signal: controller.signal }), fixture);
  } finally {
    globalThis.fetch = previousFetch;
    if (storage) Object.defineProperty(globalThis, "localStorage", storage);
    else Reflect.deleteProperty(globalThis, "localStorage");
  }
});
