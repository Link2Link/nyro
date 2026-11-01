export interface SystemStatus {
  sampled_at: number;
  sample_interval_seconds: number | null;
  hostname: string | null;
  os: string | null;
  kernel_version: string | null;
  architecture: string;
  uptime_seconds: number;
  cpu: {
    brand: string;
    logical_cores: number;
    physical_cores: number | null;
    usage_percent: number | null;
    frequency_mhz: number | null;
    cores: number[];
  };
  memory: {
    total_bytes: number;
    used_bytes: number;
    available_bytes: number;
    swap_total_bytes: number;
    swap_used_bytes: number;
  };
  disks: {
    name: string;
    mount_point: string;
    file_system: string;
    total_bytes: number;
    available_bytes: number;
    removable: boolean;
  }[];
  networks: {
    name: string;
    received_bytes: number;
    transmitted_bytes: number;
    receive_bytes_per_second: number | null;
    transmit_bytes_per_second: number | null;
  }[];
  process: { pid: number; memory_bytes: number; cpu_percent: number | null };
}

export interface MonitorPoint {
  timestamp: number;
  cpu: number | null;
  memory: number | null;
  receive: number | null;
  transmit: number | null;
  latency: number | null;
}

export interface MonitorSnapshot {
  status: SystemStatus;
  latency: number;
  history: MonitorPoint[];
}

export function percent(used: number, total: number): number | null {
  return Number.isFinite(used) && Number.isFinite(total) && total > 0
    ? Math.min(100, Math.max(0, used / total * 100))
    : null;
}

export function formatPercent(value: number | null | undefined): string {
  return value != null && Number.isFinite(value) ? `${value.toFixed(1)}%` : "—";
}

export function formatBytes(value: number | null | undefined, perSecond = false): string {
  if (value == null || !Number.isFinite(value) || value < 0) return "—";
  const units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
  const exponent = Math.min(units.length - 1, Math.floor(Math.log2(Math.max(1, value)) / 10));
  const scaled = value / 1024 ** exponent;
  return `${scaled.toFixed(exponent === 0 || scaled >= 100 ? 0 : 1)} ${units[exponent]}${perSecond ? "/s" : ""}`;
}

export function formatUptime(seconds: number, isZh: boolean): string {
  const minutes = Math.floor(Math.max(0, seconds) / 60);
  const days = Math.floor(minutes / 1440);
  const hours = Math.floor(minutes % 1440 / 60);
  return isZh ? `${days}天 ${hours}时 ${minutes % 60}分` : `${days}d ${hours}h ${minutes % 60}m`;
}

export function isLoopback(name: string): boolean {
  return /^(lo\d*|loopback.*)$/i.test(name);
}

export function networkSummary(networks: SystemStatus["networks"]) {
  const interfaces = networks.filter((network) => !isLoopback(network.name));
  // A missing rate is not zero (first sample, new interface, or counter reset).
  const rate = (key: "receive_bytes_per_second" | "transmit_bytes_per_second") =>
    !interfaces.length || interfaces.some((item) => item[key] == null) ? null
      : interfaces.reduce((sum, item) => sum + item[key]!, 0);
  return {
    receive: rate("receive_bytes_per_second"),
    transmit: rate("transmit_bytes_per_second"),
    received: interfaces.reduce((sum, item) => sum + item.received_bytes, 0),
    transmitted: interfaces.reduce((sum, item) => sum + item.transmitted_bytes, 0),
    count: interfaces.length,
  };
}

/** Most-full mounted volume, not a sum that double-counts shared filesystems. */
export function busiestDisk(disks: SystemStatus["disks"]) {
  return disks.filter((disk) => disk.total_bytes > 0).reduce<SystemStatus["disks"][number] | null>(
    (current, disk) => !current || (percent(disk.total_bytes - disk.available_bytes, disk.total_bytes) ?? 0)
      > (percent(current.total_bytes - current.available_bytes, current.total_bytes) ?? 0) ? disk : current,
    null,
  );
}

export function appendMonitorHistory(
  previous: MonitorSnapshot | undefined, status: SystemStatus, latency: number, now: number, interval: number,
): MonitorPoint[] {
  const network = networkSummary(status.networks);
  const point: MonitorPoint = {
    timestamp: now, cpu: status.cpu.usage_percent,
    memory: percent(status.memory.used_bytes, status.memory.total_bytes),
    receive: network.receive, transmit: network.transmit, latency,
  };
  const history = (previous?.history ?? []).filter((entry) => entry.timestamp > now - 10 * 60_000);
  if (previous?.status.sampled_at === status.sampled_at && history.length) {
    // The collector caches snapshots; repeated manual refreshes are not new samples.
    return [...history.slice(0, -1), { ...point, timestamp: history[history.length - 1].timestamp }];
  }
  const last = history[history.length - 1];
  if (last && now - last.timestamp > Math.max(15_000, interval * 3)) {
    history.push({ timestamp: now - 1, cpu: null, memory: null, receive: null, transmit: null, latency: null });
  }
  return [...history, point].slice(-60);
}
