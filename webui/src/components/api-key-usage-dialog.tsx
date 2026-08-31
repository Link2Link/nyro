import { useQuery } from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import {
  Activity,
  AlertCircle,
  ArrowRight,
  CheckCircle2,
  ChevronLeft,
  ChevronRight,
  Clock3,
  Gauge,
  Loader2,
  Radio,
  RefreshCw,
  Route,
  Timer,
  XCircle,
  Zap,
} from "lucide-react";
import { backend } from "@/lib/backend";
import {
  computeTps,
  formatDuration,
  formatLogTime,
  formatTokenCount,
  formatTps,
} from "@/lib/format";
import { useLocale } from "@/lib/i18n";
import { prettyName } from "@/lib/protocol";
import type {
  ApiKeyModelRouteStats,
  ApiKeyStats,
  ApiKeyUsageDetail,
  LogPage,
  LogQuery,
  RequestLog,
} from "@/lib/types";
import { cn } from "@/lib/utils";
import { LogDetailDialog } from "@/components/log-detail-dialog";
import { ModelTokenTimeSeriesChart } from "@/components/model-token-time-series-chart";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { ProviderIcon } from "@/components/ui/provider-icon";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";

type StatusFilter = "all" | "2xx" | "400+";
type Tab = "overview" | "requests";
export interface ApiKeyUsageDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  initialHours?: number;
  initialApiKeyId?: string | null;
}
const RANGES = [6, 24, 72, 168] as const;
const PAGE_SIZE = 10;
const safeHours = (value?: number) =>
  RANGES.includes(value as (typeof RANGES)[number]) ? value! : 24;
const err = (value: unknown, fallback: string) =>
  value instanceof Error && value.message ? value.message : fallback;
const routeKey = (r: ApiKeyModelRouteStats) =>
  JSON.stringify([r.client_model, r.provider_id, r.upstream_model]);
const ALL_ROUTES = "__all_routes__";
const protocolLabel = (value?: string | null) =>
  prettyName(value) ?? value ?? "–";

export function ApiKeyUsageDialog({
  open,
  onOpenChange,
  initialHours,
  initialApiKeyId,
}: ApiKeyUsageDialogProps) {
  const { locale } = useLocale();
  const zh = locale === "zh-CN";
  const previousOpen = useRef(false);
  const defaultSelectionMade = useRef(initialApiKeyId != null);
  const [hours, setHours] = useState(() => safeHours(initialHours));
  const [selectedId, setSelectedId] = useState<string | null>(
    initialApiKeyId ?? null,
  );
  const [tab, setTab] = useState<Tab>("overview");
  const [status, setStatus] = useState<StatusFilter>("all");
  const [route, setRoute] = useState<ApiKeyModelRouteStats | null>(null);
  const [page, setPage] = useState(0);
  const [selectedLog, setSelectedLog] = useState<RequestLog | null>(null);

  // Controlled-prop transitions intentionally synchronize the dialog snapshot.
  useEffect(() => {
    if (open && !previousOpen.current) {
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setHours(safeHours(initialHours));
      setSelectedId(initialApiKeyId ?? null);
      setTab("overview");
      setStatus("all");
      setRoute(null);
      setPage(0);
      setSelectedLog(null);
      defaultSelectionMade.current = initialApiKeyId != null;
    }
    previousOpen.current = open;
  }, [open, initialHours, initialApiKeyId]);

  const list = useQuery<ApiKeyStats[]>({
    queryKey: ["api-key-usage-list", hours],
    queryFn: () => backend("get_stats_by_api_key", { hours }),
    enabled: open,
    refetchInterval: open ? 30_000 : false,
  });
  const keys = useMemo(() => list.data ?? [], [list.data]);
  // Select the initial fallback once when the asynchronous list arrives.
  useEffect(() => {
    if (
      open &&
      list.data &&
      selectedId === null &&
      !defaultSelectionMade.current
    ) {
      const highest = list.data.reduce<ApiKeyStats | null>(
        (current, key) =>
          current === null || key.request_count > current.request_count
            ? key
            : current,
        null,
      );
      if (highest) {
        defaultSelectionMade.current = true;
        // eslint-disable-next-line react-hooks/set-state-in-effect
        setSelectedId(highest.api_key_id);
      }
    }
  }, [open, list.data, selectedId]);

  const detail = useQuery<ApiKeyUsageDetail>({
    queryKey: ["api-key-usage-detail", selectedId, hours],
    queryFn: () =>
      backend("get_api_key_usage_detail", { id: selectedId!, hours }),
    enabled: open && !!selectedId,
    refetchInterval:
      open && selectedId ? (tab === "requests" ? 5_000 : 30_000) : false,
  });
  const logFilter = useMemo<LogQuery>(() => {
    const query: LogQuery = {
      limit: PAGE_SIZE,
      offset: page * PAGE_SIZE,
      api_key: selectedId ?? undefined,
      after: detail.data?.start_at,
      before: detail.data?.end_at,
      client_model: route?.client_model,
      provider: route?.provider_id,
      upstream_model: route?.upstream_model,
    };
    if (status === "2xx") {
      query.status_min = 200;
      query.status_max = 299;
    }
    if (status === "400+") query.status_min = 400;
    return query;
  }, [
    page,
    selectedId,
    detail.data?.start_at,
    detail.data?.end_at,
    route,
    status,
  ]);
  const logs = useQuery<LogPage>({
    queryKey: ["api-key-usage-logs", logFilter],
    queryFn: () => backend("query_logs", { query: logFilter }),
    enabled: open && tab === "requests" && !!selectedId && !!detail.data,
  });
  const pages = Math.max(1, Math.ceil((logs.data?.total ?? 0) / PAGE_SIZE));

  // A refreshed total can invalidate the current page.
  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect
    setPage((current) => Math.min(current, pages - 1));
  }, [pages]);
  const selectRoute = (value: ApiKeyModelRouteStats) => {
    setRoute(value);
    setPage(0);
    setTab("requests");
  };
  const refresh = () => {
    void list.refetch();
    if (selectedId) void detail.refetch();
    if (tab === "requests") void logs.refetch();
  };

  return (
    <>
      <Dialog open={open} onOpenChange={onOpenChange}>
        <DialogContent className="flex h-[min(92vh,900px)] w-[min(96vw,1280px)] max-w-none flex-col overflow-hidden p-0 sm:w-[min(94vw,1280px)]">
          <DialogHeader className="shrink-0 border-b border-slate-200 px-4 py-4 pr-12 sm:px-6">
            <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
              <div>
                <DialogTitle>
                  {zh ? "API 密钥用量" : "API Key Usage"}
                </DialogTitle>
                <DialogDescription>
                  {zh
                    ? "按密钥查看请求、Token 与路由表现。"
                    : "Requests, tokens, and route performance by API key."}
                </DialogDescription>
              </div>
              <div className="flex flex-wrap gap-2">
                <Select
                  value={String(hours)}
                  onValueChange={(v) => {
                    setHours(Number(v));
                    if (selectedId === null) defaultSelectionMade.current = false;
                    setRoute(null);
                    setPage(0);
                  }}
                >
                  <SelectTrigger
                    className="h-9 w-[132px]"
                    aria-label={zh ? "时间范围" : "Time range"}
                  >
                    <Clock3 className="mr-1 h-3.5 w-3.5" />
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="6">
                      {zh ? "最近 6 小时" : "Last 6h"}
                    </SelectItem>
                    <SelectItem value="24">
                      {zh ? "最近 24 小时" : "Last 24h"}
                    </SelectItem>
                    <SelectItem value="72">
                      {zh ? "最近 3 天" : "Last 3d"}
                    </SelectItem>
                    <SelectItem value="168">
                      {zh ? "最近 7 天" : "Last 7d"}
                    </SelectItem>
                  </SelectContent>
                </Select>
                <Select
                  value={selectedId ?? undefined}
                  onValueChange={(v) => {
                    setSelectedId(v);
                    defaultSelectionMade.current = true;
                    setRoute(null);
                    setPage(0);
                  }}
                >
                  <SelectTrigger
                    className="h-9 min-w-[180px] max-w-[260px]"
                    aria-label={zh ? "API 密钥" : "API key"}
                  >
                    <SelectValue
                      placeholder={zh ? "选择 API 密钥" : "Select API key"}
                    />
                  </SelectTrigger>
                  <SelectContent>
                    {selectedId &&
                      !keys.some((key) => key.api_key_id === selectedId) && (
                        <SelectItem value={selectedId}>
                          {detail.data?.api_key_name ||
                            (zh ? "未命名密钥" : "Unnamed key")}{" "}
                          · 0
                        </SelectItem>
                      )}
                    {keys.map((key) => (
                      <SelectItem key={key.api_key_id} value={key.api_key_id}>
                        {key.api_key_name ||
                          (zh ? "未命名密钥" : "Unnamed key")}{" "}
                        · {formatTokenCount(key.request_count)}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <Button
                  variant="outline"
                  size="icon"
                  className="h-9 w-9"
                  onClick={refresh}
                  aria-label={zh ? "刷新" : "Refresh"}
                >
                  <RefreshCw
                    className={cn(
                      "h-4 w-4",
                      (list.isFetching ||
                        detail.isFetching ||
                        logs.isFetching) &&
                        "animate-spin",
                    )}
                  />
                </Button>
              </div>
            </div>
          </DialogHeader>
          <Tabs
            value={tab}
            onValueChange={(v) => setTab(v as Tab)}
            className="min-h-0 flex-1 gap-0"
          >
            <div className="shrink-0 border-b border-slate-200 px-4 sm:px-6">
              <TabsList className="h-11 rounded-none bg-transparent p-0">
                <TabsTrigger
                  value="overview"
                  className="h-11 gap-2 rounded-none border-b-2 border-transparent bg-transparent shadow-none data-[state=active]:border-blue-600 data-[state=active]:shadow-none"
                >
                  <Activity className="h-4 w-4" />
                  {zh ? "概览" : "Overview"}
                </TabsTrigger>
                <TabsTrigger
                  value="requests"
                  className="h-11 gap-2 rounded-none border-b-2 border-transparent bg-transparent shadow-none data-[state=active]:border-blue-600 data-[state=active]:shadow-none"
                >
                  <Radio className="h-4 w-4" />
                  {zh ? "请求" : "Requests"}
                </TabsTrigger>
              </TabsList>
            </div>
            <TabsContent
              value="overview"
              className="mt-0 min-h-0 flex-1 overflow-y-auto p-4 sm:p-6"
            >
              <QueryState
                loading={list.isLoading || (!!selectedId && detail.isLoading)}
                error={list.error ?? detail.error}
                empty={
                  !selectedId ||
                  !detail.data ||
                  detail.data.request_count === 0
                }
                zh={zh}
                onRetry={() => {
                  if (list.error) void list.refetch();
                  if (selectedId && detail.error) void detail.refetch();
                }}
              >
                {detail.data && (
                  <Overview
                    detail={detail.data}
                    zh={zh}
                    showDateOnAxis={hours > 24}
                    onRoute={selectRoute}
                  />
                )}
              </QueryState>
            </TabsContent>
            <TabsContent
              value="requests"
              className="mt-0 min-h-0 flex-1 overflow-y-auto p-4 sm:p-6"
            >
              <Requests
                zh={zh}
                status={status}
                setStatus={(v) => {
                  setStatus(v);
                  setPage(0);
                }}
                route={route}
                routes={detail.data?.model_routes ?? []}
                setRoute={(value) => {
                  setRoute(value);
                  setPage(0);
                }}
                query={logs}
                page={page}
                pages={pages}
                setPage={setPage}
                onSelect={setSelectedLog}
              />
            </TabsContent>
          </Tabs>
        </DialogContent>
      </Dialog>
      <LogDetailDialog
        logId={selectedLog?.id ?? null}
        summary={selectedLog}
        open={!!selectedLog}
        onOpenChange={(next) => {
          if (!next) setSelectedLog(null);
        }}
      />
    </>
  );
}

function QueryState({
  loading,
  error,
  empty,
  zh,
  onRetry,
  children,
}: {
  loading: boolean;
  error: unknown;
  empty: boolean;
  zh: boolean;
  onRetry?: () => void;
  children: ReactNode;
}) {
  if (loading)
    return (
      <State
        icon={Loader2}
        spin
        text={zh ? "正在加载用量…" : "Loading usage…"}
      />
    );
  if (error)
    return (
      <State
        icon={AlertCircle}
        danger
        text={err(error, zh ? "加载失败" : "Failed to load usage")}
        action={
          <Button variant="outline" size="sm" onClick={onRetry}>
            <RefreshCw className="h-3.5 w-3.5" />
            {zh ? "重试" : "Retry"}
          </Button>
        }
      />
    );
  if (empty)
    return (
      <State
        icon={Activity}
        text={
          zh ? "此范围暂无 API 密钥用量。" : "No API key usage in this range."
        }
      />
    );
  return <>{children}</>;
}
function State({
  icon: Icon,
  text,
  spin,
  danger,
  action,
}: {
  icon: typeof Activity;
  text: string;
  spin?: boolean;
  danger?: boolean;
  action?: ReactNode;
}) {
  return (
    <div
      className={cn(
        "flex min-h-56 flex-col items-center justify-center gap-3 rounded-xl border border-dashed text-sm",
        danger
          ? "border-red-200 bg-red-50/50 text-red-600"
          : "border-slate-200 text-slate-500",
      )}
    >
      <Icon className={cn("h-6 w-6", spin && "animate-spin")} />
      <p className="px-4 text-center">{text}</p>
      {action}
    </div>
  );
}

function Overview({
  detail,
  zh,
  showDateOnAxis,
  onRoute,
}: {
  detail: ApiKeyUsageDetail;
  zh: boolean;
  showDateOnAxis: boolean;
  onRoute: (r: ApiKeyModelRouteStats) => void;
}) {
  const rate = detail.request_count
    ? (detail.success_count / detail.request_count) * 100
    : 0;
  const cachePercentage = Math.min(
    100,
    Math.max(
      0,
      detail.total_input_tokens
        ? (detail.total_cache_read_tokens / detail.total_input_tokens) * 100
        : 0,
    ),
  );
  const cards = [
    {
      label: zh ? "请求" : "Requests",
      value: formatTokenCount(detail.request_count),
      icon: Activity,
      color: "bg-gradient-to-br from-blue-50 to-blue-100 text-blue-600",
    },
    {
      label: zh ? "严格 2xx 成功率" : "Strict 2xx Success",
      value: rate.toFixed(1) + "%",
      note: formatTokenCount(detail.success_count) + " 2xx",
      icon: CheckCircle2,
      color: "bg-gradient-to-br from-emerald-50 to-emerald-100 text-emerald-600",
    },
    {
      label: zh ? "失败" : "Failures",
      value: formatTokenCount(detail.error_count),
      icon: XCircle,
      color: "bg-gradient-to-br from-red-50 to-red-100 text-red-600",
    },
    {
      label: zh ? "输入 Token" : "Input Tokens",
      value: formatTokenCount(detail.total_input_tokens),
      icon: Zap,
      color: "bg-gradient-to-br from-sky-50 to-sky-100 text-sky-600",
    },
    {
      label: zh ? "缓存 Token" : "Cache Tokens",
      value: formatTokenCount(detail.total_cache_read_tokens),
      note: zh
        ? `输入占比 ${cachePercentage.toFixed(1)}%`
        : `${cachePercentage.toFixed(1)}% of input`,
      icon: Zap,
      color: "bg-gradient-to-br from-amber-50 to-amber-100 text-amber-600",
    },
    {
      label: zh ? "输出 Token" : "Output Tokens",
      value: formatTokenCount(detail.total_output_tokens),
      icon: Zap,
      color: "bg-gradient-to-br from-teal-50 to-emerald-100 text-emerald-600",
    },
    {
      label: zh ? "平均延迟" : "Avg Latency",
      value: formatDuration(detail.avg_duration_ms),
      icon: Timer,
      color: "bg-gradient-to-br from-violet-50 to-violet-100 text-violet-600",
    },
    {
      label: "TTFT",
      value:
        detail.avg_first_token_ms == null
          ? "–"
          : formatDuration(detail.avg_first_token_ms),
      icon: Gauge,
      color: "bg-gradient-to-br from-fuchsia-50 to-fuchsia-100 text-fuchsia-600",
    },
    {
      label: zh ? "最后使用" : "Last Used",
      value:
        detail.last_used_at == null ? "–" : formatLogTime(detail.last_used_at),
      icon: Clock3,
      color: "bg-gradient-to-br from-slate-50 to-slate-200 text-slate-600",
      small: true,
    },
  ];
  const max = Math.max(1, ...detail.model_routes.map((r) => r.request_count));
  return (
    <div className="space-y-6">
      <div className="grid grid-cols-2 gap-3 lg:grid-cols-3 xl:grid-cols-5">
        {cards.map((card) => (
          <section
            key={card.label}
            className="rounded-xl border border-slate-200 bg-white p-4 shadow-sm"
            aria-label={card.label}
          >
            <div className="flex items-center gap-2 text-xs font-medium text-slate-500">
              <span
                className={cn(
                  "inline-flex h-8 w-8 items-center justify-center rounded-lg",
                  card.color,
                )}
              >
                <card.icon className="h-4 w-4" />
              </span>
              {card.label}
            </div>
            <div
              className={cn(
                "mt-2 font-semibold tabular-nums text-slate-900",
                card.small ? "text-sm" : "text-2xl",
              )}
            >
              {card.value}
            </div>
            {card.note && (
              <div className="mt-1 text-xs text-slate-400">{card.note}</div>
            )}
          </section>
        ))}
      </div>

      {detail.model_time_series?.length ? (
        <section className="rounded-xl border border-slate-200 bg-white p-4 shadow-sm">
          <div className="mb-3 flex items-center gap-2">
            <Zap className="h-4 w-4 text-blue-600" />
            <h3 className="text-sm font-semibold">
              {zh ? "模型 Token 时序" : "Token Trends by Model"}
            </h3>
          </div>
          <div className="h-64">
            <ModelTokenTimeSeriesChart
              series={detail.model_time_series}
              zh={zh}
              showDateOnAxis={showDateOnAxis}
            />
          </div>
        </section>
      ) : null}

      <section>
        <div className="mb-3 flex items-center gap-2">
          <Route className="h-4 w-4 text-blue-600" />
          <h3 className="text-sm font-semibold">
            {zh ? "模型路由" : "Model Routes"}
          </h3>
        </div>
        {detail.model_routes.length === 0 ? (
          <State icon={Route} text={zh ? "暂无路由数据。" : "No route data."} />
        ) : (
          <div className="max-h-[360px] overflow-auto rounded-xl border border-slate-200">
            <table className="w-full min-w-[1050px] text-left">
              <thead className="sticky top-0 z-10 bg-slate-50 text-[11px] uppercase text-slate-500">
                <tr>
                  <th className="px-4 py-3">
                    {zh
                      ? "客户端模型 → 提供商 → 上游"
                      : "Client model → Provider → Upstream"}
                  </th>
                  <th className="px-3 py-3">
                    {zh ? "请求数 / 份额" : "Requests / Share"}
                  </th>
                  <th className="px-3 py-3 text-right">
                    {zh ? "失败数 / 失败率" : "Failures / Rate"}
                  </th>
                  <th className="px-3 py-3 text-right">
                    {zh ? "Token (入/缓存/出)" : "Tokens (in/cache/out)"}
                  </th>
                  <th className="px-3 py-3 text-right">
                    {zh ? "延迟" : "Latency"}
                  </th>
                  <th className="px-3 py-3 text-right">TTFT</th>
                  <th className="px-4 py-3 text-right">TPS</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-slate-100">
                {detail.model_routes.map((r) => {
                  const share = detail.request_count
                    ? (r.request_count / detail.request_count) * 100
                    : 0;
                  const failureRate = r.request_count
                    ? (r.error_count / r.request_count) * 100
                    : 0;
                  const tps =
                    r.total_upstream_ms > 0
                      ? r.total_output_tokens / (r.total_upstream_ms / 1000)
                      : null;
                  return (
                    <tr
                      key={routeKey(r)}
                      role="button"
                      tabIndex={0}
                      onClick={() => onRoute(r)}
                      onKeyDown={(e) => {
                        if (e.key === "Enter" || e.key === " ") {
                          e.preventDefault();
                          onRoute(r);
                        }
                      }}
                      className="cursor-pointer hover:bg-blue-50/50 focus:bg-blue-50 focus:outline-none"
                    >
                      <td className="px-4 py-3">
                        <div className="flex items-center gap-2 text-xs">
                          <span className="max-w-[210px] truncate font-mono">
                            {r.client_model ||
                              (zh ? "未知模型" : "Unknown model")}
                          </span>
                          <ArrowRight className="h-3.5 w-3.5 text-slate-300" />
                          <ProviderIcon name={r.provider_name} size={24} />
                          <span className="max-w-[150px] truncate">
                            {r.provider_name ||
                              (zh ? "未知提供商" : "Unknown provider")}
                          </span>
                          <ArrowRight className="h-3.5 w-3.5 text-slate-300" />
                          <span className="max-w-[210px] truncate font-mono">
                            {r.upstream_model ||
                              (zh ? "未知模型" : "Unknown model")}
                          </span>
                        </div>
                      </td>
                      <td className="px-3 py-3">
                        <div className="flex items-center gap-2">
                          <div className="h-1.5 w-20 overflow-hidden rounded-full bg-slate-100">
                            <div
                              className="h-full rounded-full bg-blue-500"
                              style={{
                                width:
                                  Math.max(3, (r.request_count / max) * 100) +
                                  "%",
                              }}
                            />
                          </div>
                          <span className="whitespace-nowrap text-xs tabular-nums">
                            {formatTokenCount(r.request_count)} · {share.toFixed(1)}%
                          </span>
                        </div>
                      </td>
                      <td className="px-3 py-3 text-right text-xs text-red-600">
                        {formatTokenCount(r.error_count)} · {failureRate.toFixed(1)}%
                      </td>
                      <td className="px-3 py-3 text-right font-mono text-xs">
                        {formatTokenCount(r.total_input_tokens)} /{" "}
                        {formatTokenCount(r.total_cache_read_tokens)} /{" "}
                        {formatTokenCount(r.total_output_tokens)}
                      </td>
                      <td className="px-3 py-3 text-right text-xs">
                        {formatDuration(r.avg_duration_ms)}
                      </td>
                      <td className="px-3 py-3 text-right text-xs">
                        {r.avg_first_token_ms == null
                          ? "–"
                          : formatDuration(r.avg_first_token_ms)}
                      </td>
                      <td className="px-4 py-3 text-right text-xs">
                        {formatTps(tps)}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </section>
    </div>
  );
}

function Requests({
  zh,
  status,
  setStatus,
  route,
  routes,
  setRoute,
  query,
  page,
  pages,
  setPage,
  onSelect,
}: {
  zh: boolean;
  status: StatusFilter;
  setStatus: (v: StatusFilter) => void;
  route: ApiKeyModelRouteStats | null;
  routes: ApiKeyModelRouteStats[];
  setRoute: (value: ApiKeyModelRouteStats | null) => void;
  query: ReturnType<typeof useQuery<LogPage>>;
  page: number;
  pages: number;
  setPage: (v: number) => void;
  onSelect: (v: RequestLog) => void;
}) {
  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex flex-wrap gap-2">
          <Select
            value={status}
            onValueChange={(v) => setStatus(v as StatusFilter)}
          >
            <SelectTrigger className="h-9 w-[140px]">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="all">
                {zh ? "全部状态" : "All statuses"}
              </SelectItem>
              <SelectItem value="2xx">
                {zh ? "成功（2xx）" : "Success (2xx)"}
              </SelectItem>
              <SelectItem value="400+">
                {zh ? "失败（4xx+）" : "Failures (4xx+)"}
              </SelectItem>
            </SelectContent>
          </Select>
          <Select
            value={route ? routeKey(route) : ALL_ROUTES}
            onValueChange={(value) =>
              setRoute(
                value === ALL_ROUTES
                  ? null
                  : (routes.find(
                      (candidate) => routeKey(candidate) === value,
                    ) ?? null),
              )
            }
          >
            <SelectTrigger className="h-9 min-w-[220px] max-w-[420px]">
              <Route className="mr-1 h-3.5 w-3.5" />
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={ALL_ROUTES}>
                {zh ? "全部路由" : "All Routes"}
              </SelectItem>
              {routes.map((candidate) => (
                <SelectItem
                  key={routeKey(candidate)}
                  value={routeKey(candidate)}
                >
                  {candidate.client_model ||
                    (zh ? "未知模型" : "Unknown model")}{" "}
                  ·{" "}
                  {candidate.provider_name ||
                    (zh ? "未知提供商" : "Unknown provider")}{" "}
                  ·{" "}
                  {candidate.upstream_model ||
                    (zh ? "未知模型" : "Unknown model")}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        {query.isFetching && (
          <span className="flex items-center gap-1 text-xs text-slate-400">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
            {zh ? "刷新中" : "Refreshing"}
          </span>
        )}
      </div>
      {query.isLoading ? (
        <State
          icon={Loader2}
          spin
          text={zh ? "正在加载请求…" : "Loading requests…"}
        />
      ) : query.error ? (
        <State
          icon={AlertCircle}
          danger
          text={err(
            query.error,
            zh ? "请求加载失败" : "Failed to load requests",
          )}
          action={
            <Button
              variant="outline"
              size="sm"
              onClick={() => void query.refetch()}
            >
              <RefreshCw className="h-3.5 w-3.5" />
              {zh ? "重试" : "Retry"}
            </Button>
          }
        />
      ) : !query.data?.items.length ? (
        <State
          icon={Radio}
          text={zh ? "没有匹配的请求。" : "No matching requests."}
        />
      ) : (
        <>
          <div className="overflow-x-auto rounded-xl border border-slate-200">
            <table className="w-full min-w-[1120px] text-left">
              <thead className="bg-slate-50 text-[11px] uppercase text-slate-500">
                <tr>
                  <th className="px-3 py-3">{zh ? "时间" : "Time"}</th>
                  <th className="px-3 py-3">{zh ? "状态" : "Status"}</th>
                  <th className="px-3 py-3">{zh ? "路由" : "Route"}</th>
                  <th className="px-3 py-3">{zh ? "协议" : "Protocol"}</th>
                  <th className="px-3 py-3">{zh ? "类型" : "Type"}</th>
                  <th className="px-3 py-3 text-right">
                    {zh ? "Token" : "Tokens"}
                  </th>
                  <th className="px-3 py-3 text-right">
                    {zh ? "延迟" : "Latency"}
                  </th>
                  <th className="px-3 py-3 text-right">TTFT</th>
                  <th className="px-3 py-3 text-right">TPS</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-slate-100">
                {query.data.items.map((log) => {
                  const code = log.client_status_code;
                  const statusStyle =
                    code != null && code >= 200 && code < 300
                      ? "border-emerald-200 bg-emerald-50 text-emerald-700"
                      : code != null && code >= 400
                        ? "border-red-200 bg-red-50 text-red-700"
                        : "border-amber-200 bg-amber-50 text-amber-700";
                  return (
                    <tr
                      key={log.id}
                      role="button"
                      tabIndex={0}
                      onClick={() => onSelect(log)}
                      onKeyDown={(e) => {
                        if (e.key === "Enter" || e.key === " ") {
                          e.preventDefault();
                          onSelect(log);
                        }
                      }}
                      className="cursor-pointer hover:bg-slate-50 focus:bg-blue-50 focus:outline-none"
                    >
                      <td className="whitespace-nowrap px-3 py-2.5 text-xs text-slate-500">
                        {formatLogTime(log.created_at)}
                      </td>
                      <td className="px-3 py-2.5">
                        <Badge
                          variant="outline"
                          className={cn("text-[10px]", statusStyle)}
                        >
                          {code ?? "–"}
                        </Badge>
                      </td>
                      <td className="px-3 py-2.5">
                        <div className="flex max-w-[360px] items-center gap-1.5 text-xs">
                          <span className="truncate font-mono">
                            {log.client_model ?? log.model_name ?? "–"}
                          </span>
                          <ArrowRight className="h-3 w-3 text-slate-300" />
                          <ProviderIcon
                            name={log.provider_name}
                            protocol={log.upstream_protocol}
                            size={21}
                          />
                          <span className="truncate">
                            {log.provider_name ?? log.provider_id ?? "–"}
                          </span>
                          <ArrowRight className="h-3 w-3 text-slate-300" />
                          <span className="truncate font-mono">
                            {log.upstream_model ?? "–"}
                          </span>
                        </div>
                      </td>
                      <td className="px-3 py-2.5 text-xs text-slate-500">
                        {protocolLabel(log.client_protocol)} →{" "}
                        {protocolLabel(log.upstream_protocol)}
                      </td>
                      <td className="px-3 py-2.5">
                        <Badge variant="outline" className="text-[10px]">
                          {log.is_stream || (log.stream_chunks_count ?? 0) > 0 ? "SSE" : "JSON"}
                        </Badge>
                      </td>
                      <td className="px-3 py-2.5 text-right font-mono text-xs">
                        {formatTokenCount(log.input_tokens)} /{" "}
                        {formatTokenCount(log.cache_read_tokens ?? 0)} /{" "}
                        {formatTokenCount(log.output_tokens)}
                      </td>
                      <td className="px-3 py-2.5 text-right text-xs">
                        {log.latency_total_ms == null
                          ? "–"
                          : formatDuration(log.latency_total_ms)}
                      </td>
                      <td className="px-3 py-2.5 text-right text-xs">
                        {log.stream_first_chunk_ms == null
                          ? "–"
                          : formatDuration(log.stream_first_chunk_ms)}
                      </td>
                      <td className="px-3 py-2.5 text-right text-xs">
                        {formatTps(computeTps(log))}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
          <div className="flex items-center justify-between">
            <span className="text-xs text-slate-500">
              {zh
                ? "第 " +
                  (page + 1) +
                  " / " +
                  pages +
                  " 页，共 " +
                  query.data.total +
                  " 条"
                : "Page " +
                  (page + 1) +
                  " of " +
                  pages +
                  " · " +
                  query.data.total +
                  " total"}
            </span>
            <div className="flex gap-1">
              <Button
                variant="outline"
                size="icon"
                onClick={() => setPage(Math.max(0, page - 1))}
                disabled={page === 0}
                aria-label={zh ? "上一页" : "Previous page"}
              >
                <ChevronLeft className="h-4 w-4" />
              </Button>
              <Button
                variant="outline"
                size="icon"
                onClick={() => setPage(Math.min(pages - 1, page + 1))}
                disabled={page >= pages - 1}
                aria-label={zh ? "下一页" : "Next page"}
              >
                <ChevronRight className="h-4 w-4" />
              </Button>
            </div>
          </div>
        </>
      )}
    </div>
  );
}
