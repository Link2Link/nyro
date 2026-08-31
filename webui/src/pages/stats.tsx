import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { PieChart, Pie, Cell, ResponsiveContainer, Tooltip } from "recharts";
import { backend } from "@/lib/backend";
import type { StatsOverview, StatsTimeSeries, ModelStats, ProviderStats, ApiKeyStats } from "@/lib/types";
import { Zap, Clock, Activity, BarChart3 } from "lucide-react";
import { ApiKeyUsageDialog } from "@/components/api-key-usage-dialog";
import { TokenTimeSeriesChart } from "@/components/token-time-series-chart";
import { ProviderUsageDialog } from "@/components/provider-usage-dialog";
import { ModelUsageDialog } from "@/components/model-usage-dialog";
import { ProviderIcon } from "@/components/ui/provider-icon";
import { Button } from "@/components/ui/button";
import { useLocale } from "@/lib/i18n";
import { formatLogTime, formatTps } from "@/lib/format";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

const COLORS = ["#3b82f6", "#10b981", "#f59e0b", "#ef4444", "#8b5cf6", "#ec4899", "#06b6d4", "#84cc16"];

function fmt(n: number) {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + "M";
  if (n >= 1_000) return (n / 1_000).toFixed(1) + "K";
  return String(n);
}

function fmtLatency(ms: number) {
  if (ms >= 1000) {
    return `${(ms / 1000).toFixed(ms >= 10_000 ? 1 : 2)}s`;
  }
  return `${ms.toFixed(0)}ms`;
}

// 图表 tooltip 数值格式化：大数字用 K/M，与表格保持一致
function chartTooltipFormatter(value: number | string, name: string): [string, string] {
  return [fmt(Number(value)), name];
}

export default function StatsPage() {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";

  const [hours, setHours] = useState(24);
  const [apiKeyDialog, setApiKeyDialog] = useState<{ open: boolean; apiKeyId: string | null }>({ open: false, apiKeyId: null });
  const [providerDialog, setProviderDialog] = useState<{ open: boolean; providerId: string | null }>({ open: false, providerId: null });
  const [modelDialog, setModelDialog] = useState<{ open: boolean; model: string | null }>({ open: false, model: null });

  const openApiKeyDialog = (apiKeyId: string | null = null) => {
    setApiKeyDialog({ open: true, apiKeyId });
  };
  const openProviderDialog = (providerId: string | null = null) => {
    setProviderDialog({ open: true, providerId });
  };
  const openModelDialog = (model: string | null = null) => {
    setModelDialog({ open: true, model });
  };

  const { data: overview } = useQuery<StatsOverview>({
    queryKey: ["stats-overview", hours],
    queryFn: () => backend("get_stats_overview", { hours }),
    refetchInterval: 10_000,
  });

  const { data: timeSeries } = useQuery<StatsTimeSeries>({
    queryKey: ["stats-timeseries", hours],
    queryFn: () => backend("get_stats_timeseries", { hours }),
    refetchInterval: 30_000,
  });

  const { data: modelStats = [] } = useQuery<ModelStats[]>({
    queryKey: ["stats-models", hours],
    queryFn: () => backend("get_stats_by_model", { hours }),
    refetchInterval: 30_000,
  });

  const { data: providerStats = [] } = useQuery<ProviderStats[]>({
    queryKey: ["stats-providers", hours],
    queryFn: () => backend("get_stats_by_provider", { hours }),
    refetchInterval: 30_000,
  });

  const { data: apiKeyStats = [] } = useQuery<ApiKeyStats[]>({
    queryKey: ["stats-apikeys", hours],
    queryFn: () => backend("get_stats_by_api_key", { hours }),
    refetchInterval: 30_000,
  });

  const showDateOnAxis = hours > 24;

  const modelPie = modelStats.slice(0, 6).map((m) => ({
    name: m.model,
    value: m.request_count,
  }));
  const modelTotal = modelPie.reduce((acc, m) => acc + m.value, 0);

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold text-slate-900">{isZh ? "统计" : "Statistics"}</h1>
          <p className="mt-1 text-sm text-slate-500">
            {isZh ? "Token 使用、延迟与错误分析" : "Token usage, latency, and error analytics"}
          </p>
        </div>
        <Select value={String(hours)} onValueChange={(value) => setHours(Number(value))}>
          <SelectTrigger className="w-40">
            <SelectValue placeholder={isZh ? "选择时间范围" : "Select range"} />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="6">{isZh ? "最近 6 小时" : "Last 6h"}</SelectItem>
            <SelectItem value="24">{isZh ? "最近 24 小时" : "Last 24h"}</SelectItem>
            <SelectItem value="72">{isZh ? "最近 3 天" : "Last 3d"}</SelectItem>
            <SelectItem value="168">{isZh ? "最近 7 天" : "Last 7d"}</SelectItem>
          </SelectContent>
        </Select>
      </div>

      <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
        {[
          { label: isZh ? "总请求数" : "Total Requests", value: fmt(overview?.total_requests ?? 0), icon: Activity, color: "text-blue-600" },
          { label: isZh ? "平均延迟" : "Avg Latency", value: `${(overview?.avg_duration_ms ?? 0).toFixed(0)}ms`, icon: Clock, color: "text-purple-600" },
        ].map((c) => (
          <div key={c.label} className="glass rounded-2xl p-4">
            <div className="flex items-center gap-2">
              <c.icon className={`h-4 w-4 ${c.color}`} />
              <p className="text-xs font-medium text-slate-500">{c.label}</p>
            </div>
            <p className="mt-1.5 text-[24px] leading-none font-semibold text-slate-900">
              {c.label === (isZh ? "平均延迟" : "Avg Latency") ? fmtLatency(overview?.avg_duration_ms ?? 0) : c.value}
            </p>
          </div>
        ))}
        {(() => {
          // 输入 Token 卡片单独渲染,带缓存命中率标注。
          // 命中率 = cache_read / input (input 已含 cache, GROSS 语义)。
          const inputTokens = overview?.total_input_tokens ?? 0;
          const cacheRead = overview?.total_cache_read_tokens ?? 0;
          const cacheRate = inputTokens > 0
            ? Math.round((cacheRead / inputTokens) * 100)
            : 0;
          return (
            <div className="glass rounded-2xl p-4">
              <div className="flex items-center gap-2">
                <Zap className="h-4 w-4 text-amber-600" />
                <p className="text-xs font-medium text-slate-500">
                  {isZh ? "输入 Token" : "Input Tokens"}
                </p>
              </div>
              <div className="mt-1.5 flex items-baseline gap-1.5">
                <p className="text-[24px] leading-none font-semibold text-slate-900">
                  {fmt(inputTokens)}
                </p>
                {cacheRead > 0 && (
                  <span
                    className="text-[11px] font-medium text-amber-600"
                    title={isZh
                      ? `缓存命中 ${fmt(cacheRead)} token,占输入 ${cacheRate}%`
                      : `${fmt(cacheRead)} tokens from cache (${cacheRate}% of input)`}
                  >
                    ({cacheRate}%)
                  </span>
                )}
              </div>
            </div>
          );
        })()}
        {(() => {
          const outputTokens = overview?.total_output_tokens ?? 0;
          return (
            <div className="glass rounded-2xl p-4">
              <div className="flex items-center gap-2">
                <Zap className="h-4 w-4 text-green-600" />
                <p className="text-xs font-medium text-slate-500">
                  {isZh ? "输出 Token" : "Output Tokens"}
                </p>
              </div>
              <p className="mt-1.5 text-[24px] leading-none font-semibold text-slate-900">
                {fmt(outputTokens)}
              </p>
            </div>
          );
        })()}
      </div>

      <div className="grid grid-cols-1 gap-3 xl:grid-cols-2">
        <div className="glass rounded-2xl p-6">
          <div className="mb-4 flex items-center justify-between gap-3">
            <h3 className="text-sm font-semibold text-slate-800">{isZh ? "Token 时序" : "Token Usage Over Time"}</h3>
            {timeSeries && (
              <span className="shrink-0 text-xs text-slate-400">
                {isZh
                  ? `${timeSeries.bucket_minutes} 分钟/点`
                  : `${timeSeries.bucket_minutes} min / point`}
              </span>
            )}
          </div>
          <div className="h-48">
            <TokenTimeSeriesChart series={timeSeries} zh={isZh} showDateOnAxis={showDateOnAxis} />
          </div>
        </div>

        <div className="glass rounded-2xl p-6">
          <h3 className="mb-4 text-sm font-semibold text-slate-800">{isZh ? "模型请求分布" : "Requests by Model"}</h3>
          <div className="grid grid-cols-1 gap-3 lg:grid-cols-[240px_1fr] lg:items-center">
            <div className="h-44">
            {modelPie.length > 0 ? (
                <ResponsiveContainer width="100%" height="100%">
                  <PieChart>
                    <Pie
                      data={modelPie}
                      cx="50%"
                      cy="50%"
                      innerRadius={44}
                      outerRadius={68}
                      paddingAngle={3}
                      dataKey="value"
                      label={false}
                      labelLine={false}
                    >
                      {modelPie.map((_, i) => (
                        <Cell key={i} fill={COLORS[i % COLORS.length]} />
                      ))}
                    </Pie>
                    <Tooltip formatter={chartTooltipFormatter} />
                  </PieChart>
                </ResponsiveContainer>
            ) : (
              <div className="flex h-full items-center justify-center text-sm text-slate-400">{isZh ? "暂无数据" : "No data"}</div>
            )}
            </div>
            {modelPie.length > 0 && (
              <div className="space-y-2">
                {modelPie.map((item, i) => {
                  const pct = modelTotal > 0 ? Math.round((item.value / modelTotal) * 100) : 0;
                  return (
                    <div key={item.name} className="flex items-center justify-between gap-3 text-sm">
                      <div className="min-w-0 flex items-center gap-2">
                        <span
                          className="h-2.5 w-2.5 shrink-0 rounded-full"
                          style={{ backgroundColor: COLORS[i % COLORS.length] }}
                        />
                        <span className="truncate text-slate-600">{item.name}</span>
                      </div>
                      <span className="shrink-0 font-medium text-slate-900">{pct}%</span>
                    </div>
                  );
                })}
              </div>
            )}
          </div>
        </div>
      </div>

      <div className="glass rounded-2xl p-6">
        <div className="mb-4 flex items-center justify-between gap-3">
          <h3 className="text-sm font-semibold text-slate-800">{isZh ? "提供商分布" : "Provider Breakdown"}</h3>
          <Button variant="outline" size="sm" className="h-8 gap-1.5" onClick={() => openProviderDialog()}>
            <BarChart3 className="h-4 w-4" />
            {isZh ? "查看详情" : "View Details"}
          </Button>
        </div>
        <div className="overflow-hidden rounded-xl border border-white/70 bg-white/50">
          <table className="w-full text-sm">
            <thead className="bg-white/70 text-slate-500">
              <tr>
                <th className="px-4 py-2.5 text-left font-medium">{isZh ? "提供商" : "Provider"}</th>
                <th className="px-4 py-2.5 text-right font-medium">{isZh ? "请求数" : "Requests"}</th>
                <th className="px-4 py-2.5 text-right font-medium">{isZh ? "错误数" : "Errors"}</th>
                <th className="px-4 py-2.5 text-right font-medium">{isZh ? "错误率" : "Error Rate"}</th>
                <th className="px-4 py-2.5 text-right font-medium">{isZh ? "平均延迟" : "Avg Latency"}</th>
                <th className="px-4 py-2.5 text-right font-medium">TPS</th>
              </tr>
            </thead>
            <tbody>
              {providerStats.length === 0 && (
                <tr><td className="px-4 py-6 text-center text-slate-400" colSpan={6}>{isZh ? "暂无数据" : "No data"}</td></tr>
              )}
              {providerStats.slice(0, 8).map((p) => {
                const tps = p.total_upstream_ms > 0 && p.total_output_tokens > 0
                  ? p.total_output_tokens / (p.total_upstream_ms / 1000)
                  : null;
                return (
                  <tr
                    key={p.provider_id}
                    role="button"
                    tabIndex={0}
                    onClick={() => openProviderDialog(p.provider_id)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter" || event.key === " ") {
                        event.preventDefault();
                        openProviderDialog(p.provider_id);
                      }
                    }}
                    className="cursor-pointer border-t border-white/70 text-slate-700 outline-none transition-colors hover:bg-blue-50/50 focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-blue-500"
                  >
                    <td className="px-4 py-2.5 font-medium">
                      <div className="flex min-w-0 items-center gap-2">
                        <ProviderIcon iconKey={p.provider_icon ?? undefined} name={p.provider} protocol={p.provider_protocol ?? undefined} size={24} />
                        <span className="truncate" title={p.provider}>{p.provider}</span>
                      </div>
                    </td>
                    <td className="px-4 py-2.5 text-right">{fmt(p.request_count)}</td>
                    <td className="px-4 py-2.5 text-right text-red-500">{p.error_count}</td>
                    <td className="px-4 py-2.5 text-right">
                      {p.request_count > 0 ? ((p.error_count / p.request_count) * 100).toFixed(1) : "0"}%
                    </td>
                    <td className="px-4 py-2.5 text-right">{fmtLatency(p.avg_duration_ms)}</td>
                    <td className="px-4 py-2.5 text-right">{formatTps(tps)}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      </div>

      <div className="glass rounded-2xl p-6">
        <div className="mb-4 flex items-center justify-between gap-3">
          <h3 className="text-sm font-semibold text-slate-800">{isZh ? "密钥调用统计" : "API Key Usage"}</h3>
          <Button
            variant="outline"
            size="sm"
            className="h-8 gap-1.5"
            onClick={() => openApiKeyDialog()}
          >
            <BarChart3 className="h-4 w-4" />
            {isZh ? "查看详情" : "View Details"}
          </Button>
        </div>
        <div className="overflow-hidden rounded-xl border border-white/70 bg-white/50">
          <table className="w-full text-sm">
            <thead className="bg-white/70 text-slate-500">
              <tr>
                <th className="px-4 py-2.5 text-left font-medium">{isZh ? "密钥" : "API Key"}</th>
                <th className="px-4 py-2.5 text-right font-medium">{isZh ? "请求数" : "Requests"}</th>
                <th className="px-4 py-2.5 text-right font-medium">{isZh ? "失败数" : "Failures"}</th>
                <th className="px-4 py-2.5 text-right font-medium">{isZh ? "输入 Token" : "Input Tokens"}</th>
                <th className="px-4 py-2.5 text-right font-medium">{isZh ? "缓存命中" : "Cache Hits"}</th>
                <th className="px-4 py-2.5 text-right font-medium">{isZh ? "输出 Token" : "Output Tokens"}</th>
                <th className="px-4 py-2.5 text-right font-medium">{isZh ? "最后调用" : "Last Used"}</th>
              </tr>
            </thead>
            <tbody>
              {apiKeyStats.length === 0 && (
                <tr><td className="px-4 py-6 text-center text-slate-400" colSpan={7}>{isZh ? "暂无数据" : "No data"}</td></tr>
              )}
              {apiKeyStats.slice(0, 8).map((k) => {
                // input_tokens 已包含 cache_read_tokens (GROSS 语义),
                // 所以命中率 = cache_read / input,不应再相加成总池子。
                const cacheRate = k.total_input_tokens > 0
                  ? Math.round((k.cache_read_tokens / k.total_input_tokens) * 100)
                  : 0;
                return (
                  <tr
                    key={k.api_key_id}
                    role="button"
                    tabIndex={0}
                    onClick={() => openApiKeyDialog(k.api_key_id)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter" || event.key === " ") {
                        event.preventDefault();
                        openApiKeyDialog(k.api_key_id);
                      }
                    }}
                    className="cursor-pointer border-t border-white/70 text-slate-700 outline-none transition-colors hover:bg-blue-50/50 focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-blue-500"
                  >
                    <td className="px-4 py-2.5 font-medium">{k.api_key_name || k.api_key_id}</td>
                    <td className="px-4 py-2.5 text-right">{fmt(k.request_count)}</td>
                    <td className="px-4 py-2.5 text-right text-red-500">{fmt(k.error_count)}</td>
                    <td className="px-4 py-2.5 text-right">{fmt(k.total_input_tokens)}</td>
                    <td className="px-4 py-2.5 text-right">
                      <span>{fmt(k.cache_read_tokens)}</span>
                      {cacheRate > 0 && <span className="ml-1 text-[11px] text-slate-400">{cacheRate}%</span>}
                    </td>
                    <td className="px-4 py-2.5 text-right">{fmt(k.total_output_tokens)}</td>
                    <td className="px-4 py-2.5 text-right text-xs text-slate-500 whitespace-nowrap">{formatLogTime(k.last_used_at)}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      </div>

      <ProviderUsageDialog
        open={providerDialog.open}
        onOpenChange={(open) => setProviderDialog((current) => ({ ...current, open }))}
        initialHours={hours}
        initialProviderId={providerDialog.providerId}
      />

      <ApiKeyUsageDialog
        open={apiKeyDialog.open}
        onOpenChange={(open) => setApiKeyDialog((current) => ({ ...current, open }))}
        initialHours={hours}
        initialApiKeyId={apiKeyDialog.apiKeyId}
      />

      <ModelUsageDialog
        open={modelDialog.open}
        onOpenChange={(open) => setModelDialog((current) => ({ ...current, open }))}
        initialHours={hours}
        initialModel={modelDialog.model}
      />

      <div className="glass rounded-2xl p-6">
        <div className="mb-4 flex items-center justify-between gap-3">
          <h3 className="text-sm font-semibold text-slate-800">{isZh ? "模型 Token 统计（调用次数前 10）" : "Model Token Stats (Top 10 by Requests)"}</h3>
          <Button variant="outline" size="sm" className="h-8 gap-1.5" onClick={() => openModelDialog()}>
            <BarChart3 className="h-4 w-4" />
            {isZh ? "查看详情" : "View Details"}
          </Button>
        </div>
        <div className="overflow-x-auto rounded-xl border border-white/70 bg-white/50">
          <table className="w-full text-sm">
            <thead className="bg-white/70 text-slate-500">
              <tr>
                <th className="px-4 py-2.5 text-left font-medium">{isZh ? "模型" : "Model"}</th>
                <th className="px-4 py-2.5 text-right font-medium">{isZh ? "调用次数" : "Requests"}</th>
                <th className="px-4 py-2.5 text-right font-medium">{isZh ? "输入 Token" : "Input Tokens"}</th>
                <th className="px-4 py-2.5 text-right font-medium">{isZh ? "缓存命中" : "Cache Hits"}</th>
                <th className="px-4 py-2.5 text-right font-medium">{isZh ? "输出 Token" : "Output Tokens"}</th>
                <th className="px-4 py-2.5 text-right font-medium">{isZh ? "缓存命中率" : "Cache Rate"}</th>
                <th className="px-4 py-2.5 text-right font-medium">{isZh ? "平均延迟" : "Avg Latency"}</th>
                <th className="px-4 py-2.5 text-right font-medium">TPS</th>
              </tr>
            </thead>
            <tbody>
              {modelStats.length === 0 && (
                <tr><td className="px-4 py-6 text-center text-slate-400" colSpan={8}>{isZh ? "暂无数据" : "No data"}</td></tr>
              )}
              {modelStats.slice(0, 10).map((m) => {
                // 同上:input_tokens 已含 cache_read_tokens,直接相除。
                const cacheRate = m.total_input_tokens > 0
                  ? Math.round((m.total_cache_read_tokens / m.total_input_tokens) * 100)
                  : 0;
                const tps = m.total_upstream_ms > 0 && m.total_output_tokens > 0
                  ? m.total_output_tokens / (m.total_upstream_ms / 1000)
                  : null;
                const clickable = m.model.length > 0;
                return (
                  <tr
                    key={m.model}
                    {...(clickable
                      ? {
                          role: "button" as const,
                          tabIndex: 0,
                          onClick: () => openModelDialog(m.model),
                          onKeyDown: (event: React.KeyboardEvent<HTMLTableRowElement>) => {
                            if (event.key === "Enter" || event.key === " ") {
                              event.preventDefault();
                              openModelDialog(m.model);
                            }
                          },
                        }
                      : {})}
                    className={clickable
                      ? "cursor-pointer border-t border-white/70 text-slate-700 outline-none transition-colors hover:bg-blue-50/50 focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-blue-500"
                      : "border-t border-white/70 text-slate-700"}
                  >
                    <td className="px-4 py-2.5 font-medium">{m.model || "–"}</td>
                    <td className="px-4 py-2.5 text-right">{fmt(m.request_count)}</td>
                    <td className="px-4 py-2.5 text-right">{fmt(m.total_input_tokens)}</td>
                    <td className="px-4 py-2.5 text-right">{fmt(m.total_cache_read_tokens)}</td>
                    <td className="px-4 py-2.5 text-right">{fmt(m.total_output_tokens)}</td>
                    <td className="px-4 py-2.5 text-right">{cacheRate}%</td>
                    <td className="px-4 py-2.5 text-right">{fmtLatency(m.avg_duration_ms)}</td>
                    <td className="px-4 py-2.5 text-right">{formatTps(tps)}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}
