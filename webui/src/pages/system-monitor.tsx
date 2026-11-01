import { useEffect, useState, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Activity, ArrowDown, ArrowUp, Clock3, Cpu, HardDrive, Info, MemoryStick, Monitor, Network, Pause, Play, RefreshCw, Server } from "lucide-react";
import { backend, IS_TAURI } from "@/lib/backend";
import { useLocale } from "@/lib/i18n";
import {
  appendMonitorHistory, busiestDisk, formatBytes, formatPercent, formatUptime, isLoopback,
  networkSummary, percent, type MonitorSnapshot, type SystemStatus,
} from "@/lib/system-monitor";
import { SystemMonitorChart } from "@/components/system-monitor-chart";
import { Button } from "@/components/ui/button";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import "./system-monitor.css";

const QUERY_KEY = ["system-monitor"];
const COLORS = { cpu: "#9b87e5", memory: "#50b8ca", receive: "#50b8ca", transmit: "#9b87e5" };

function Meter({ value, label, color }: { value: number | null; label: string; color?: string }) {
  return <div className="monitor-meter" role="meter" aria-label={label} aria-valuemin={0} aria-valuemax={100}
    aria-valuenow={value ?? undefined} aria-valuetext={formatPercent(value)}>
    <span style={{ width: `${value ?? 0}%`, background: value != null && value >= 90 ? "#e5a25c" : color }} />
  </div>;
}

function StatCard({ icon, label, value, detail, children }: {
  icon: ReactNode; label: string; value: string; detail: ReactNode; children?: ReactNode;
}) {
  return <section className="glass flex min-w-0 flex-col rounded-2xl p-4">
    <div className="flex items-center justify-between gap-2 text-xs text-muted-foreground"><h2>{label}</h2>{icon}</div>
    <p className="mt-4 text-3xl font-semibold tracking-tight text-foreground tabular-nums">{value}</p>
    <div className="mt-2 min-h-9 text-xs leading-relaxed text-muted-foreground">{detail}</div>
    <div className="mt-auto pt-3">{children}</div>
  </section>;
}

export default function SystemMonitorPage() {
  const { locale } = useLocale();
  const zh = locale === "zh-CN";
  const t = (en: string, cn: string) => zh ? cn : en;
  const [interval, setIntervalMs] = useState(3000);
  const [paused, setPaused] = useState(false);
  const [now, setNow] = useState(Date.now);
  const queryClient = useQueryClient();
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, []);
  const query = useQuery({
    queryKey: QUERY_KEY,
    queryFn: async ({ signal }) => {
      const start = performance.now();
      const controller = new AbortController();
      const cancel = () => controller.abort(signal.reason);
      if (signal.aborted) cancel();
      signal.addEventListener("abort", cancel, { once: true });
      const timer = window.setTimeout(() => controller.abort(new Error("System metrics request timed out (10s)")), 10_000);
      try {
        const status = await backend<SystemStatus>("get_system_status", undefined, { signal: controller.signal });
        const latency = performance.now() - start;
        return {
          status, latency,
          history: appendMonitorHistory(queryClient.getQueryData<MonitorSnapshot>(QUERY_KEY), status, latency, Date.now(), interval),
        } satisfies MonitorSnapshot;
      } finally {
        window.clearTimeout(timer);
        signal.removeEventListener("abort", cancel);
      }
    },
    enabled: !paused, staleTime: 0, retry: false,
    refetchInterval: paused ? false : interval,
    refetchOnWindowFocus: false,
  });
  const data = query.data;
  const status = data?.status;
  const network = status ? networkSummary(status.networks) : null;
  const disk = status ? busiestDisk(status.disks) : null;
  const memoryPercent = status ? percent(status.memory.used_bytes, status.memory.total_bytes) : null;
  const diskPercent = disk ? percent(disk.total_bytes - disk.available_bytes, disk.total_bytes) : null;
  const stale = !!data && now - query.dataUpdatedAt > Math.max(15_000, interval * 3);
  const latestAge = Math.max(0, Math.floor((now - query.dataUpdatedAt) / 1000));
  const waiting = t("Waiting for a second sample", "等待第二次采样");
  const unavailable = t("Not available", "不可用");
  const history = data?.history ?? [];
  const latencySamples = history.flatMap((point) => point.latency == null ? [] : [point.latency]);
  const formatLatency = (value: number) => `${value.toFixed(1)} ms`;
  const stateLabel = query.isError ? t("Connection lost", "连接异常") : paused ? t("Paused", "已暂停")
    : stale ? t("Stale data", "数据已过期") : status ? t("Live monitoring", "实时监控") : t("Connecting", "正在连接");

  return <div className="system-monitor space-y-5">
    <header className="flex flex-col justify-between gap-4 xl:flex-row xl:items-center">
      <div>
        <div className="flex flex-wrap items-center gap-3">
          <h1 className="text-2xl font-bold text-slate-900">{t("System Monitor", "系统监控")}</h1>
          <span className="monitor-live" data-state={query.isError || stale ? "error" : paused || !status ? "paused" : "live"} role="status">
            <span aria-hidden="true" />{stateLabel}
          </span>
        </div>
        <p className="mt-1.5 text-sm text-slate-500">{t("A live view of the resources powering your gateway.", "实时了解网关运行环境的资源占用与网络状态。")}</p>
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <Select value={String(interval)} onValueChange={(value) => setIntervalMs(Number(value))}>
          <SelectTrigger className="h-8 w-28 text-xs" aria-label={t("Refresh interval", "刷新间隔")}><SelectValue /></SelectTrigger>
          <SelectContent>{[3000, 5000, 10000].map((value) => <SelectItem key={value} value={String(value)}>{value / 1000}{t("s refresh", " 秒刷新")}</SelectItem>)}</SelectContent>
        </Select>
        <Button variant="secondary" size="sm" onClick={() => setPaused(!paused)} aria-pressed={paused}>
          {paused ? <Play className="h-3.5 w-3.5" /> : <Pause className="h-3.5 w-3.5" />}{paused ? t("Resume", "继续") : t("Pause", "暂停")}
        </Button>
        <Button variant="secondary" size="sm" onClick={() => void query.refetch()} disabled={query.isFetching} aria-label={t("Refresh system metrics", "刷新系统指标")}>
          <RefreshCw className={`h-3.5 w-3.5 ${query.isFetching ? "animate-spin" : ""}`} />{t("Refresh", "刷新")}
        </Button>
      </div>
    </header>

    <div className="flex flex-wrap items-center justify-between gap-2 text-xs text-muted-foreground">
      <span className="flex min-w-0 items-center gap-2"><Server className="h-3.5 w-3.5 shrink-0" /><span className="break-all">{status?.hostname ?? "Nyro"}{status?.os ? ` · ${status.os}` : ""}</span></span>
      <span className="flex items-center gap-1.5"><Clock3 className="h-3.5 w-3.5" />{status ? `${t("Last received", "上次接收")} ${latestAge}${t("s ago", " 秒前")}` : t("Collecting system metrics…", "正在采集系统指标…")}</span>
    </div>

    {query.isError && <div role="alert" className="rounded-xl border border-red-200 bg-red-50 p-4 text-sm text-red-600">
      <p className="font-medium">{t("System metrics could not be refreshed.", "系统指标刷新失败。")}</p>
      <p className="mt-1 text-xs">{data ? t("The last successful snapshot remains visible and is not live.", "当前保留上次成功快照，并非实时数据。") : t("Check your connection and that the backend includes the system monitor endpoint.", "请检查连接，并确认后端已更新至包含系统监控接口的版本。")}</p>
      <p className="mt-2 break-words text-xs">{String(query.error)}</p>
      <Button className="mt-3" variant="secondary" size="sm" disabled={query.isFetching} onClick={() => void query.refetch()}>{t("Retry", "重试")}</Button>
    </div>}
    {!status && !query.isError && <div role="status" className="glass rounded-2xl p-8 text-center text-sm text-muted-foreground">{t("Loading your system snapshot…", "正在加载系统运行快照…")}</div>}

    {status && data && <>
      <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
        <StatCard icon={<Cpu className="h-4 w-4" />} label={t("CPU usage", "CPU 使用率")} value={formatPercent(status.cpu.usage_percent)}
          detail={<><span>{status.cpu.logical_cores} {t("logical cores", "逻辑核心")}</span><p className="truncate" title={status.cpu.brand}>{status.cpu.brand || unavailable}</p></>}>
          <Meter value={status.cpu.usage_percent} label={t("CPU usage", "CPU 使用率")} color={COLORS.cpu} />
        </StatCard>
        <StatCard icon={<MemoryStick className="h-4 w-4" />} label={t("Memory usage", "内存使用率")} value={formatPercent(memoryPercent)}
          detail={<>{formatBytes(status.memory.used_bytes)} / {formatBytes(status.memory.total_bytes)}<p>{t("Available", "可用")} {formatBytes(status.memory.available_bytes)}</p></>}>
          <Meter value={memoryPercent} label={t("Memory usage", "内存使用率")} color={COLORS.memory} />
        </StatCard>
        <StatCard icon={<HardDrive className="h-4 w-4" />} label={t("Most-used volume", "最高磁盘占用")} value={formatPercent(diskPercent)}
          detail={disk ? <><p className="truncate" title={disk.mount_point}>{disk.mount_point}</p>{formatBytes(disk.available_bytes)} {t("free of", "可用 / 共")} {formatBytes(disk.total_bytes)}</> : unavailable}>
          <Meter value={diskPercent} label={t("Disk usage", "磁盘使用率")} />
        </StatCard>
        <StatCard icon={<Activity className="h-4 w-4" />} label={IS_TAURI ? t("IPC round trip", "IPC 往返耗时") : t("Admin API round trip", "管理 API 往返耗时")} value={formatLatency(data.latency)}
          detail={t("Client → Nyro → client, including collection time. Not upstream latency.", "客户端 → Nyro → 客户端，含采集时间，非上游模型延迟。")}>
          <div className="flex flex-wrap justify-between gap-1 text-[11px] text-muted-foreground tabular-nums"><span>MIN {formatLatency(Math.min(...latencySamples))}</span><span>MAX {formatLatency(Math.max(...latencySamples))}</span></div>
        </StatCard>
      </div>

      <div className="grid min-w-0 gap-4 lg:grid-cols-2">
        <section className="glass min-w-0 rounded-2xl p-4">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <h2 className="flex items-center gap-2 text-sm font-semibold text-foreground"><Monitor className="h-4 w-4 text-muted-foreground" />{t("Resource trends", "资源占用趋势")}</h2>
            <div className="flex gap-3 text-xs text-muted-foreground"><span><i className="monitor-dot" style={{ background: COLORS.cpu }} />CPU</span><span><i className="monitor-dot" style={{ background: COLORS.memory }} />{t("Memory", "内存")}</span></div>
          </div>
          <p className="mt-2 text-xs text-muted-foreground">{t("Last 60 samples · up to 10 minutes · in-memory only", "最近 60 次采样 · 最多 10 分钟 · 仅保留于内存")}</p>
          <SystemMonitorChart history={history} series={[{ key: "cpu", label: "CPU", color: COLORS.cpu }, { key: "memory", label: t("Memory", "内存"), color: COLORS.memory }]} unit="percent" locale={locale} emptyLabel={waiting} />
        </section>
        <section className="glass min-w-0 rounded-2xl p-4">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <h2 className="flex items-center gap-2 text-sm font-semibold text-foreground"><Network className="h-4 w-4 text-muted-foreground" />{t("Network throughput", "网络实时吞吐")}</h2>
            <span className="text-xs text-muted-foreground">{network?.count} {t("interfaces, excluding loopback", "个接口，不含回环")}</span>
          </div>
          <div className="mt-2 flex flex-wrap gap-x-5 gap-y-1 text-xs tabular-nums">
            <span className="flex items-center gap-1" style={{ color: COLORS.receive }}><ArrowDown className="h-3.5 w-3.5" />{t("Receive", "接收")} <strong>{formatBytes(network?.receive, true)}</strong></span>
            <span className="flex items-center gap-1" style={{ color: COLORS.transmit }}><ArrowUp className="h-3.5 w-3.5" />{t("Send", "发送")} <strong>{formatBytes(network?.transmit, true)}</strong></span>
          </div>
          <SystemMonitorChart history={history} series={[{ key: "receive", label: t("Receive", "接收"), color: COLORS.receive }, { key: "transmit", label: t("Send", "发送"), color: COLORS.transmit }]} unit="bytes" locale={locale} emptyLabel={waiting} />
        </section>
      </div>

      <div className="grid min-w-0 gap-4 xl:grid-cols-2">
        <section className="glass min-w-0 rounded-2xl p-4">
          <h2 className="mb-4 flex items-center gap-2 text-sm font-semibold text-foreground"><HardDrive className="h-4 w-4 text-muted-foreground" />{t("Storage volumes", "磁盘与挂载点")}</h2>
          {!status.disks.length && <p className="py-4 text-xs text-muted-foreground">{t("No accessible volumes reported by this environment.", "当前环境未报告可访问的磁盘。")}</p>}
          <div className="max-h-80 space-y-5 overflow-y-auto pr-1">{status.disks.map((volume, index) => {
            const used = volume.total_bytes - volume.available_bytes;
            const usage = percent(used, volume.total_bytes);
            return <div key={`${volume.mount_point}-${index}`}>
              <div className="mb-2 flex items-start justify-between gap-3 text-xs"><div className="min-w-0"><p className="break-all font-medium text-foreground">{volume.mount_point}</p><p className="mt-1 break-all text-muted-foreground">{volume.name} · {volume.file_system}{volume.removable ? ` · ${t("Removable", "可移动")}` : ""}</p></div><span className="shrink-0 font-medium text-foreground tabular-nums">{formatPercent(usage)}</span></div>
              <Meter value={usage} label={`${volume.mount_point} ${t("usage", "使用率")}`} />
              <p className="mt-2 flex flex-wrap justify-between gap-1 text-[11px] text-muted-foreground"><span>{t("Used", "已用")} {formatBytes(used)} / {formatBytes(volume.total_bytes)}</span><span>{t("Free", "可用")} {formatBytes(volume.available_bytes)}</span></p>
            </div>;
          })}</div>
          <p className="mt-4 text-[11px] leading-relaxed text-muted-foreground">{t("Each mount is shown separately; shared or container mounts may refer to the same storage and are not added together.", "逐挂载点显示；共享或容器挂载可能指向同一存储，因此不简单累加容量。")}</p>
        </section>
        <section className="glass min-w-0 rounded-2xl p-4">
          <h2 className="mb-4 flex items-center gap-2 text-sm font-semibold text-foreground"><Server className="h-4 w-4 text-muted-foreground" />{t("System information", "系统信息")}</h2>
          <dl className="monitor-info">
            <div><dt>{t("Host", "主机")}</dt><dd>{status.hostname ?? unavailable}</dd></div>
            <div><dt>{t("Operating system", "操作系统")}</dt><dd>{status.os ?? unavailable} · {status.architecture}</dd></div>
            <div><dt>{t("Kernel", "内核")}</dt><dd>{status.kernel_version ?? unavailable}</dd></div>
            <div><dt>{t("System uptime", "系统运行时间")}</dt><dd>{formatUptime(status.uptime_seconds, zh)}</dd></div>
            <div><dt>{t("CPU topology", "CPU 核心")}</dt><dd>{status.cpu.physical_cores ?? "—"} {t("physical", "物理")} / {status.cpu.logical_cores} {t("logical", "逻辑")}</dd></div>
            <div><dt>{t("CPU frequency", "CPU 频率")}</dt><dd>{status.cpu.frequency_mhz == null ? unavailable : `${status.cpu.frequency_mhz.toFixed(0)} MHz`}</dd></div>
            <div><dt>Swap</dt><dd>{status.memory.swap_total_bytes > 0 ? `${formatBytes(status.memory.swap_used_bytes)} / ${formatBytes(status.memory.swap_total_bytes)}` : t("Not configured", "未配置")}</dd></div>
            <div><dt>{t("Nyro process", "Nyro 进程")}</dt><dd>PID {status.process.pid} · {formatBytes(status.process.memory_bytes)} · CPU {formatPercent(status.process.cpu_percent)}</dd></div>
          </dl>
          <p className="mt-3 text-[11px] leading-relaxed text-muted-foreground">{t("Process CPU is normalized to the whole machine (0–100%). CPU frequency depends on platform support.", "进程 CPU 按整机核心归一化至 0–100%；CPU 频率取决于平台支持。")}</p>
        </section>
      </div>

      <section className="glass min-w-0 rounded-2xl p-4">
        <div className="mb-4 flex flex-wrap items-center justify-between gap-2">
          <h2 className="flex items-center gap-2 text-sm font-semibold text-foreground"><Network className="h-4 w-4 text-muted-foreground" />{t("Network interfaces", "网络接口")}</h2>
          <p className="text-xs text-muted-foreground">↓ {formatBytes(network?.received)} <span className="mx-2">↑ {formatBytes(network?.transmitted)}</span>{t("total, excluding loopback", "累计，不含回环")}</p>
        </div>
        {!status.networks.length ? <p className="py-4 text-xs text-muted-foreground">{t("No network interfaces reported.", "未检测到网络接口。")}</p> : <div className="overflow-x-auto"><table className="w-full min-w-[600px] text-left text-xs tabular-nums">
          <thead className="text-muted-foreground"><tr>{[t("Interface", "接口"), t("Receive / s", "接收速率"), t("Send / s", "发送速率"), t("Total received", "累计接收"), t("Total sent", "累计发送")].map((label) => <th key={label} className="px-3 pb-3 font-normal first:pl-0">{label}</th>)}</tr></thead>
          <tbody>{status.networks.map((item) => <tr key={item.name} className="border-t border-slate-200/70 text-foreground">
            <td className="py-3 pr-3 font-mono">{item.name}{isLoopback(item.name) && <span className="ml-2 font-sans text-[10px] text-muted-foreground">{t("loopback", "回环")}</span>}</td>
            <td className="p-3">{formatBytes(item.receive_bytes_per_second, true)}</td><td className="p-3">{formatBytes(item.transmit_bytes_per_second, true)}</td>
            <td className="p-3">{formatBytes(item.received_bytes)}</td><td className="p-3">{formatBytes(item.transmitted_bytes)}</td>
          </tr>)}</tbody>
        </table></div>}
      </section>

      <details className="glass rounded-2xl p-4">
        <summary className="cursor-pointer text-sm font-semibold text-foreground">{t("Per-core CPU usage", "各逻辑核心 CPU 占用")} <span className="ml-2 text-xs font-normal text-muted-foreground">{status.cpu.logical_cores} {t("cores", "核心")}</span></summary>
        {!status.cpu.cores.length ? <p className="mt-4 text-xs text-muted-foreground">{waiting}</p> : <div className="mt-4 grid grid-cols-2 gap-3 sm:grid-cols-4 xl:grid-cols-8">{status.cpu.cores.map((usage, index) => <div key={index} className="rounded-xl border border-slate-200/70 p-2.5"><div className="mb-2 flex justify-between gap-1 text-[10px] text-muted-foreground"><span>#{index}</span><span>{formatPercent(usage)}</span></div><Meter value={usage} label={`CPU ${index}`} color={COLORS.cpu} /></div>)}</div>}
      </details>
      <footer className="flex items-start gap-2 px-1 text-[11px] leading-relaxed text-muted-foreground">
        <Info className="mt-0.5 h-3.5 w-3.5 shrink-0" /><div>
          <p>{t("Metrics describe the environment running Nyro, not this browser. In containers, CPU/memory visibility depends on the host and runtime; these are not guaranteed to match container quotas.", "指标来自 Nyro 所在运行环境，而非浏览器设备。容器内 CPU、内存可见范围由宿主系统及运行时决定，不保证等于容器配额。")}</p>
          <p>{t("Throughput is measured from interface byte counters, not an internet speed test or link capacity. Virtual interfaces may count the same traffic more than once. Totals reset with the interface or OS. A dash means unavailable or warming up, not zero.", "吞吐按网卡字节计数差值计算，并非公网测速或链路带宽上限；虚拟接口可能重复计入流量。累计值可能随接口或系统重启而重置。“—”表示不可用或采样预热，并非零。")}</p>
        </div>
      </footer>
    </>}
  </div>;
}
