import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Activity, RefreshCw, Search } from "lucide-react";
import { Link } from "react-router-dom";
import { backend } from "@/lib/backend";
import { formatLocalDateTime, formatTps } from "@/lib/format";
import { useLocale } from "@/lib/i18n";
import {
  buildPerformanceRows, filterPerformanceRows, performancePoints, readPerformanceResponse,
} from "@/lib/model-performance";
import type { Provider } from "@/lib/types";
import { ModelPerformanceChart } from "@/components/model-performance-chart";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";

export default function PerformancePage() {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const [search, setSearch] = useState("");
  const [providerFilter, setProviderFilter] = useState("all");
  const providers = useQuery<Provider[]>({ queryKey: ["providers"], queryFn: () => backend("get_providers") });
  // One backend snapshot: rated prefixes joined with retained-call TPS per provider.
  const performance = useQuery({
    queryKey: ["model-performance"],
    queryFn: async () => readPerformanceResponse(await backend<unknown>("get_model_performance")),
    staleTime: 15_000, retry: false, refetchOnWindowFocus: true,
  });
  const rows = useMemo(() => performance.data ? buildPerformanceRows(performance.data, providers.data ?? []) : [], [performance.data, providers.data]);
  const visible = filterPerformanceRows(rows, search, providerFilter === "all" ? null : providerFilter.slice(9));
  const points = performancePoints(visible, "overall");
  const missing = visible.filter((row) => row.status === "missing" || row.overallTps === null);
  const providerOptions = [...new Map(rows.map((row) => [row.providerId, row.providerName])).entries()]
    .sort((a, b) => a[1].localeCompare(b[1], "en") || a[0].localeCompare(b[0], "en"));
  const busy = performance.isFetching || providers.isFetching;
  const ready = performance.isSuccess && providers.isSuccess;
  async function refresh() { await Promise.all([performance.refetch(), providers.refetch()]); }

  return (
    <div className="space-y-5">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-end sm:justify-between">
        <div><h1 className="text-2xl font-bold text-slate-900">{isZh ? "性能" : "Performance"}</h1>
          <p className="mt-1 text-sm text-slate-500">{isZh ? "按前缀评分 × 供应商实测速度对比。每个“前缀 × 供应商”一个点；同供应商的多个模型变体按有效样本加权合并。" : "Rated prefixes × measured provider speed. One point per prefix × provider; a provider's model variants merge weighted by valid samples."}</p></div>
        <Button variant="secondary" size="sm" disabled={busy} onClick={() => void refresh()} aria-label={isZh ? "刷新性能数据" : "Refresh performance"}>
          <RefreshCw className={busy ? "h-4 w-4 animate-spin" : "h-4 w-4"} />{isZh ? "刷新性能数据" : "Refresh performance"}
        </Button>
      </div>
      <div className="space-y-2 rounded-xl border border-blue-100 bg-blue-50/50 px-4 py-3 text-xs leading-relaxed text-slate-600">
        <p>{isZh ? "横轴按当前可见点的最低分向下、最高分向上取整至 10 的倍数自动缩放（满分 100），并在左右两端留出标记边距，极值点不会压在坐标轴上；纵轴为综合总体 TPS（总输出 Token / 上游总延时），包含首字延迟与 Prefill 开销，真实反映端到端吞吐，默认范围 0–100，超过 100 时自动扩大上限。每个变体取已保留日志中最近 10 次调用按有效样本合并。每个点显示该供应商的图标，虚线边框表示不足 3 个有效样本。不是主动测速或全历史平均。" : "X automatically rounds the visible minimum score down and maximum up to multiples of 10 (scores remain out of 100) and keeps marker room at both ends; Y is the overall TPS (total output tokens / total upstream latency), incorporating time-to-first-token and prefill latency, defaulting to 0–100 and expanding automatically above 100. Each variant averages its latest 10 retained calls. Every point shows its provider's icon; a dashed border marks fewer than 3 valid samples. This is not an active benchmark or an all-time average."}</p>
        <p>{isZh ? "同一前缀不同供应商各一个点：能力分数共享，速度差异一目了然。未命中任何前缀条目、或没有有效 TPS 的分组不绘图。" : "Each provider serving the same prefix gets its own point: capability is shared, speed differences stand out. Groups without a matching prefix entry or without valid TPS are not plotted."}</p>
        {performance.data && <p>{isZh ? "统计获取时间" : "Statistics fetched"}: {formatLocalDateTime(performance.data.as_of)}</p>}
      </div>
      {(performance.isPending || providers.isPending) && <p role="status" className="text-sm text-slate-500">{isZh ? "正在加载性能快照与供应商…" : "Loading the performance snapshot and providers…"}</p>}
      {(performance.isError || providers.isError) && <div role="alert" className="space-y-2 rounded-xl border border-red-200 bg-red-50 p-4 text-sm text-red-700">
        <p>{isZh ? "性能或供应商信息加载失败；未知不代表无评分或 TPS 为 0。" : "Performance or provider information failed to load. Unknown does not mean unrated or TPS 0."}</p>
        <p className="break-words text-xs">{String(performance.error ?? providers.error)}</p>
        <Button variant="secondary" size="sm" disabled={busy} onClick={() => void refresh()}>{isZh ? "重试" : "Retry"}</Button>
      </div>}
      {ready && <>
        <div className="glass flex flex-col gap-3 rounded-2xl p-3 md:flex-row">
          <div className="relative min-w-0 flex-1"><Search className="pointer-events-none absolute top-1/2 left-3 h-4 w-4 -translate-y-1/2 text-slate-400" />
            <Input className="pl-9" value={search} onChange={(event) => setSearch(event.target.value)} aria-label={isZh ? "搜索前缀、供应商或模型" : "Search prefixes, providers or models"} placeholder={isZh ? "搜索前缀、供应商或模型" : "Search prefixes, providers or models"} /></div>
          <Select value={providerFilter} onValueChange={setProviderFilter}>
            <SelectTrigger className="w-full md:w-52" aria-label={isZh ? "按供应商筛选" : "Filter by provider"}><SelectValue /></SelectTrigger>
            <SelectContent><SelectItem value="all">{isZh ? "全部供应商" : "All providers"}</SelectItem>{providerOptions.map(([id, name]) => <SelectItem key={id} value={`provider:${id}`}>{name}</SelectItem>)}</SelectContent>
          </Select>
        </div>
        <div className="flex flex-wrap gap-x-5 gap-y-2 text-sm text-slate-600" data-testid="performance-summary" data-profile-count={performance.data.models.length} data-rated-count={visible.length} data-plotted-count={points.length} data-missing-count={missing.length}>
          <span>{isZh ? "评分 × 供应商业绩点" : "Rated prefix×provider points"}: <strong>{performance.data.models.length}</strong></span>
          <span>{isZh ? "可见分组" : "Visible groups"}: <strong>{visible.length}</strong></span>
          <span>{isZh ? "已绘图" : "Plotted"}: <strong>{points.length}</strong></span>
          <span>{isZh ? "无有效 TPS" : "No valid TPS"}: <strong>{missing.length}</strong></span>
        </div>
        <section className="glass min-w-0 rounded-2xl border border-slate-200">
          <div className="border-b border-slate-200/70 px-4 py-3">
            <h2 className="text-sm font-semibold text-slate-800">{isZh ? "能力 × 综合速度（总体 TPS）" : "Capability × overall speed (overall TPS)"}</h2>
            <p className="mt-1 text-xs text-slate-500">
              {isZh ? "按总输出 Token 与总延时（总输出 / 上游总耗时）计算综合总体 TPS，真实反映包含首字延迟与 Prefill 开销在内的端到端综合交互吞吐。仅包络线上的点直接显示前缀与供应商名称；虚线连接综合速度维度的右上凸包边界。" : "Overall TPS computed from total output tokens divided by total upstream latency, capturing the real-world impact of time-to-first-token and prefill latency. Dashed line follows the upper-right convex boundary."}
            </p>
          </div>
          <ModelPerformanceChart points={points} metric="overall" isZh={isZh} />
          {!points.length && <div role="status" className="flex items-center justify-center gap-2 border-t border-slate-200 p-4 text-sm text-slate-500"><Activity className="h-4 w-4 shrink-0" />
            {!rows.length ? <span>{isZh ? "尚无已评分且已产生流量的前缀。" : "No rated prefix has logged traffic yet."} <Link to="/model-ratings" className="text-blue-600 underline">{isZh ? "前往评分" : "Rate models"}</Link></span> : !visible.length ? (isZh ? "没有匹配的分组。" : "No groups match your filters.") : (isZh ? "暂无可绘制的数据；请查看下方状态。" : "No plottable data; check the statuses below.")}</div>}
        </section>
        <details className="glass rounded-2xl p-4" open={missing.length > 0}>
          <summary className="cursor-pointer text-sm font-semibold">{isZh ? "所有分组状态与诊断" : "All group statuses & diagnostics"} ({visible.length})</summary>
          <div className="mt-3 overflow-x-auto"><table className="w-full min-w-[900px] text-left text-xs">
            <thead className="text-slate-500"><tr>{(isZh ? ["前缀 / 供应商", "评分 · 净速 / 综合 TPS", "有效 / 已选请求", "样本时间", "状态 / 变体"] : ["Prefix / provider", "Score · Net / Overall TPS", "Valid / selected requests", "Sample times", "Status / variants"]).map((label) => <th key={label} className="p-2">{label}</th>)}</tr></thead>
            <tbody>{visible.map((row) => <tr key={row.key} className="border-t border-slate-200/70 align-top">
              <td className="max-w-72 p-2"><strong className="font-mono">{row.modelPrefix}</strong><p className="mt-1"><strong>{row.providerName}</strong></p><p className="break-all text-slate-500">{row.providerId}</p>{!row.providerEnabled && <p className="text-amber-700">{isZh ? "供应商已禁用" : "Provider disabled"}</p>}</td>
              <td className="p-2 tabular-nums">
                {row.score}/100
                <p className="font-medium text-slate-800" title={isZh ? "综合总体速度 (含首字延时)" : "Overall speed (incl. TTFT)"}>
                  {row.overallTps !== null ? formatTps(row.overallTps) : "–"}
                </p>
                {row.tps !== null && (
                  <p className="text-[11px] text-slate-400" title={isZh ? "净生成速度 (纯吐字参考)" : "Net generation speed reference"}>
                    {formatTps(row.tps)} {isZh ? "(净吐字)" : "(net)"}
                  </p>
                )}
              </td>
              <td className="p-2 tabular-nums">{`${row.validTpsCount} / ${row.selectedRequestCount}`}{row.status === "ready" && row.validTpsCount < 3 && <p className="text-amber-700">○ {isZh ? "低样本量" : "Low sample count"}</p>}</td>
              <td className="p-2"><p>{formatLocalDateTime(row.firstSampleAt)}</p><p>{formatLocalDateTime(row.lastSampleAt)}</p></td>
              <td className="max-w-80 p-2"><Badge variant={row.status === "ready" ? "success" : "outline"}>{row.status === "ready" ? (isZh ? "已绘图" : "Plotted") : (isZh ? "无有效 TPS" : "No valid TPS")}</Badge>
                {row.status === "missing" && <p className="mt-1 text-slate-500">{isZh ? (row.selectedRequestCount === 0 ? "暂无调用记录" : "最近调用缺少有效输出 token 或耗时") : (row.selectedRequestCount === 0 ? "No recorded calls" : "Recent calls lack usable output tokens or timing")}</p>}
                {row.variants.length > 0 && <ul className="mt-1 space-y-0.5">{row.variants.map((variant) => (
                  <li key={variant.upstream_model} className="break-all font-mono text-[11px] text-slate-600">
                    {variant.upstream_model} · {variant.stats.average_tps !== null ? formatTps(variant.stats.average_tps) : "–"}{variant.stats.overall_tps != null ? ` (${isZh ? "综合" : "overall"} ${formatTps(variant.stats.overall_tps)})` : ""} · {variant.stats.valid_tps_count}/{variant.stats.selected_request_count}
                  </li>
                ))}</ul>}
              </td>
            </tr>)}</tbody>
          </table></div>
        </details>
      </>}
    </div>
  );
}
