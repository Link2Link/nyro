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
  // One backend snapshot, rated pairs only. Never query legacy per-model usage.
  const performance = useQuery({
    queryKey: ["model-performance"],
    queryFn: async () => readPerformanceResponse(await backend<unknown>("get_model_performance")),
    staleTime: 15_000, retry: false, refetchOnWindowFocus: true,
  });
  const rows = useMemo(() => performance.data ? buildPerformanceRows(performance.data, providers.data ?? []) : [], [performance.data, providers.data]);
  const visible = filterPerformanceRows(rows, search, providerFilter === "all" ? null : providerFilter.slice(9));
  const points = performancePoints(visible);
  const missing = visible.filter((row) => row.status === "missing");
  const failures = visible.filter((row) => row.status === "error");
  const providerOptions = [...new Map(rows.map((row) => [row.providerId, row.providerName])).entries()]
    .sort((a, b) => a[1].localeCompare(b[1], "en") || a[0].localeCompare(b[0], "en"));
  const busy = performance.isFetching || providers.isFetching;
  const ready = performance.isSuccess && providers.isSuccess;
  async function refresh() { await Promise.all([performance.refetch(), providers.refetch()]); }

  return (
    <div className="space-y-5">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-end sm:justify-between">
        <div><h1 className="text-2xl font-bold text-slate-900">{isZh ? "性能" : "Performance"}</h1>
          <p className="mt-1 text-sm text-slate-500">{isZh ? "比较已评分模型的能力与生成速度。每个供应商与模型只有一个综合评分和一个性能点。" : "Compare rated models by capability and generation speed. Each exact provider/model pair has one comprehensive score and one performance point."}</p></div>
        <Button variant="secondary" size="sm" disabled={busy} onClick={() => void refresh()} aria-label={isZh ? "刷新性能数据" : "Refresh performance"}>
          <RefreshCw className={busy ? "h-4 w-4 animate-spin" : "h-4 w-4"} />{isZh ? "刷新性能数据" : "Refresh performance"}
        </Button>
      </div>
      <div className="space-y-2 rounded-xl border border-blue-100 bg-blue-50/50 px-4 py-3 text-xs leading-relaxed text-slate-600">
        <p>{isZh ? "横轴从 0 开始，按当前最高分向上取整至 10 的倍数（满分 100）；纵轴为平均 TPS（输出 token/秒），默认范围 0–100，超过 100 时自动扩大上限。统计窗口为最近 7 天，每个分组取最近 10 次成功且完整结束的请求（排除输出上限截断），再平均有效 TPS；不足 3 个有效样本为空心点。不是主动测速或全历史平均。" : "X starts at 0 and rounds the visible maximum score up to a multiple of 10 (scores remain out of 100); Y is average TPS (output tokens/second), defaulting to 0–100 and expanding automatically when values exceed 100. Over the last 7 days, each group selects its latest 10 successful, complete requests (output-limit truncations excluded), then averages valid TPS samples. Fewer than 3 valid samples are hollow. This is not an active benchmark or an all-time average."}</p>
        <p>{isZh ? "统一显示综合评分 × 混合 TPS，不区分推理强度。仅统计完整成功请求；未评分或没有有效 TPS 的模型不绘图。" : "Shows one comprehensive score × mixed TPS, without reasoning-effort breakdowns. Only successful, complete requests contribute; unrated models or models without valid TPS are not plotted."}</p>
        {performance.data && <p>{isZh ? "服务端快照" : "Server snapshot"}: {formatLocalDateTime(performance.data.as_of)} · {isZh ? "窗口起始" : "Window starts"}: {formatLocalDateTime(performance.data.window_start)}</p>}
      </div>
      {(performance.isPending || providers.isPending) && <p role="status" className="text-sm text-slate-500">{isZh ? "正在加载性能快照与供应商…" : "Loading the performance snapshot and providers…"}</p>}
      {(performance.isError || providers.isError) && <div role="alert" className="space-y-2 rounded-xl border border-red-200 bg-red-50 p-4 text-sm text-red-700">
        <p>{isZh ? "性能或供应商信息加载失败；未知不代表未评分或 TPS 为 0。" : "Performance or provider information failed to load. Unknown does not mean unrated or TPS 0."}</p>
        <p className="break-words text-xs">{String(performance.error ?? providers.error)}</p>
        <Button variant="secondary" size="sm" disabled={busy} onClick={() => void refresh()}>{isZh ? "重试" : "Retry"}</Button>
      </div>}
      {ready && <>
        <div className="glass flex flex-col gap-3 rounded-2xl p-3 md:flex-row">
          <div className="relative min-w-0 flex-1"><Search className="pointer-events-none absolute top-1/2 left-3 h-4 w-4 -translate-y-1/2 text-slate-400" />
            <Input className="pl-9" value={search} onChange={(event) => setSearch(event.target.value)} aria-label={isZh ? "搜索供应商或模型" : "Search providers or models"} placeholder={isZh ? "搜索供应商或模型" : "Search providers or models"} /></div>
          <Select value={providerFilter} onValueChange={setProviderFilter}>
            <SelectTrigger className="w-full md:w-52" aria-label={isZh ? "按供应商筛选" : "Filter by provider"}><SelectValue /></SelectTrigger>
            <SelectContent><SelectItem value="all">{isZh ? "全部供应商" : "All providers"}</SelectItem>{providerOptions.map(([id, name]) => <SelectItem key={id} value={`provider:${id}`}>{name}</SelectItem>)}</SelectContent>
          </Select>
        </div>
        <div className="flex flex-wrap gap-x-5 gap-y-2 text-sm text-slate-600" data-testid="performance-summary" data-profile-count={performance.data.models.length} data-rated-count={visible.length} data-plotted-count={points.length} data-missing-count={missing.length} data-error-count={failures.length}>
          <span>{isZh ? "已评分模型" : "Rated models"}: <strong>{performance.data.models.length}</strong></span>
          <span>{isZh ? "可见分组" : "Visible groups"}: <strong>{visible.length}</strong></span>
          <span>{isZh ? "已绘图" : "Plotted"}: <strong>{points.length}</strong></span>
          <span>{isZh ? "无有效 TPS" : "No valid TPS"}: <strong>{missing.length}</strong></span>
          {!!failures.length && <span className="text-red-600">{isZh ? "统计失败" : "Statistics failed"}: <strong>{failures.length}</strong></span>}
        </div>
        {!!failures.length && <div role="alert" className="rounded-xl border border-amber-200 bg-amber-50 p-3 text-sm text-amber-700">{isZh ? "部分模型统计失败，其他数据仍可绘图。具体错误见下方明细；刷新可重试。" : "Some models failed; other data remains plotted. Exact errors are in the details below. Refresh to retry."}</div>}
        <section className="glass min-w-0 rounded-2xl border border-slate-200">
          <div className="border-b border-slate-200/70 px-4 py-3"><h2 className="text-sm font-semibold text-slate-800">{isZh ? "能力 × 速度" : "Capability × speed"}</h2>
            <p className="mt-1 text-xs text-slate-500">{isZh ? "图上直接显示模型与供应商名称；重合点列出所有模型。悬停、聚焦或轻触模型查看评分、TPS 和样本详情。" : "Models and providers are labeled directly; coincident points list every model. Hover, focus or tap a model for score, TPS and sample details."}</p></div>
          <ModelPerformanceChart points={points} isZh={isZh} />
          {!points.length && <div role="status" className="flex items-center justify-center gap-2 border-t border-slate-200 p-4 text-sm text-slate-500"><Activity className="h-4 w-4 shrink-0" />
            {!rows.length ? <span>{isZh ? "尚无已评分模型。" : "No models have been rated yet."} <Link to="/model-ratings" className="text-blue-600 underline">{isZh ? "前往评分" : "Rate models"}</Link></span> : !visible.length ? (isZh ? "没有匹配的分组。" : "No groups match your filters.") : (isZh ? "暂无可绘制的数据；请查看下方状态。" : "No plottable data; check the statuses below.")}</div>}
        </section>
        <details className="glass rounded-2xl p-4" open={missing.length > 0 || failures.length > 0}>
          <summary className="cursor-pointer text-sm font-semibold">{isZh ? "所有分组状态与诊断" : "All group statuses & diagnostics"} ({visible.length})</summary>
          <div className="mt-3 overflow-x-auto"><table className="w-full min-w-[900px] text-left text-xs">
            <thead className="text-slate-500"><tr>{(isZh ? ["供应商 / 模型", "评分 / TPS", "有效 / 已选请求", "样本时间", "状态 / 诊断"] : ["Provider / model", "Score / TPS", "Valid / selected requests", "Sample times", "Status / diagnostics"]).map((label) => <th key={label} className="p-2">{label}</th>)}</tr></thead>
            <tbody>{visible.map((row) => <tr key={row.key} className="border-t border-slate-200/70">
              <td className="max-w-72 p-2"><strong>{row.providerName}</strong><p className="break-all text-slate-500">{row.providerId}</p><p className="mt-1 whitespace-pre-wrap break-all font-mono">{row.model}</p></td>
              <td className="p-2 tabular-nums">{row.score === null ? "–" : `${row.score}/100`}<p>{row.status === "ready" ? formatTps(row.tps) : "–"}</p></td>
              <td className="p-2 tabular-nums">{row.status === "error" ? "–" : `${row.validTpsCount} / ${row.selectedRequestCount}`}{row.status === "ready" && row.validTpsCount < 3 && <p className="text-amber-700">○ {isZh ? "低样本量" : "Low sample count"}</p>}</td>
              <td className="p-2"><p>{formatLocalDateTime(row.firstSampleAt)}</p><p>{formatLocalDateTime(row.lastSampleAt)}</p></td>
              <td className="max-w-80 p-2"><Badge variant={row.status === "error" ? "danger" : row.status === "ready" ? "success" : "outline"}>{row.status === "ready" ? (isZh ? "已绘图" : "Plotted") : row.status === "missing" ? (isZh ? "无有效 TPS" : "No valid TPS") : (isZh ? "统计失败" : "Statistics failed")}</Badge>
                {row.error && <p role="alert" className="mt-1 whitespace-pre-wrap break-words text-red-700">{row.error}</p>}
                <p className="mt-1 text-slate-500">{isZh ? `不可信历史或未确认完成请求：${row.untrustedCount}` : `Untrusted historical or unconfirmed requests: ${row.untrustedCount}`}</p>
              </td>
            </tr>)}</tbody>
          </table></div>
        </details>
      </>}
    </div>
  );
}
