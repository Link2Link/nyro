import { useQuery } from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import {
  Activity,
  AlertCircle,
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
import {
  outcomeFilterValue,
  outcomeQuery,
  usageOutcomeRates,
} from "@/lib/log-observability";
import { OutcomeFilter, ResultBadge } from "@/components/log-outcome";
import { prettyName } from "@/lib/protocol";
import type {
  LogPage,
  LogQuery,
  ProviderModelUsageStats,
  ProviderStats,
  ProviderUsageDetail,
  RequestLog,
} from "@/lib/types";
import { cn } from "@/lib/utils";
import { LogDetailDialog } from "@/components/log-detail-dialog";
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

type StatusFilter = "all" | "200" | "2xx" | "400+";
type Tab = "overview" | "requests";

export interface ProviderUsageDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  initialHours?: number;
  initialProviderId?: string | null;
}

const RANGES = [6, 24, 72, 168] as const;
const PAGE_SIZE = 10;
const ALL_MODELS_VALUE = JSON.stringify(["all"]);
const modelValue = (model: string) => JSON.stringify(["model", model]);
const safeHours = (value?: number) =>
  RANGES.includes(value as (typeof RANGES)[number]) ? value! : 24;
const protocolLabel = (value?: string | null) =>
  prettyName(value) ?? value ?? "–";
const errorText = (value: unknown, fallback: string) =>
  value instanceof Error && value.message ? value.message : fallback;

export function ProviderUsageDialog({
  open,
  onOpenChange,
  initialHours,
  initialProviderId,
}: ProviderUsageDialogProps) {
  const { locale } = useLocale();
  const zh = locale === "zh-CN";
  const previousOpen = useRef(false);
  const defaultSelectionMade = useRef(initialProviderId != null);
  const [hours, setHours] = useState(() => safeHours(initialHours));
  const [selectedId, setSelectedId] = useState<string | null>(
    initialProviderId ?? null,
  );
  const [tab, setTab] = useState<Tab>("overview");
  const [status, setStatus] = useState<StatusFilter>("all");
  const [outcome, setOutcome] = useState("all");
  const [model, setModel] = useState<string | null>(null);
  const [page, setPage] = useState(0);
  const [selectedLog, setSelectedLog] = useState<RequestLog | null>(null);

  useEffect(() => {
    if (open && !previousOpen.current) {
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setHours(safeHours(initialHours));
      setSelectedId(initialProviderId ?? null);
      setTab("overview");
      setStatus("all");
      setOutcome("all");
      setModel(null);
      setPage(0);
      setSelectedLog(null);
      defaultSelectionMade.current = initialProviderId != null;
    }
    previousOpen.current = open;
  }, [open, initialHours, initialProviderId]);

  const list = useQuery<ProviderStats[]>({
    queryKey: ["provider-usage-list", hours],
    queryFn: () => backend("get_stats_by_provider", { hours }),
    enabled: open,
    refetchInterval: open ? 30_000 : false,
  });
  const providers = useMemo(() => list.data ?? [], [list.data]);

  useEffect(() => {
    if (
      open &&
      list.data &&
      selectedId === null &&
      !defaultSelectionMade.current
    ) {
      const highest = list.data.reduce<ProviderStats | null>(
        (current, provider) =>
          current === null || provider.request_count > current.request_count
            ? provider
            : current,
        null,
      );
      if (highest) {
        defaultSelectionMade.current = true;
        // eslint-disable-next-line react-hooks/set-state-in-effect
        setSelectedId(highest.provider_id);
      }
    }
  }, [open, list.data, selectedId]);

  const detail = useQuery<ProviderUsageDetail>({
    queryKey: ["provider-usage-detail", selectedId, hours],
    queryFn: () =>
      backend("get_provider_usage_detail", { id: selectedId!, hours }),
    enabled: open && !!selectedId,
    refetchInterval:
      open && selectedId ? (tab === "requests" ? 5_000 : 30_000) : false,
  });

  const logFilter = useMemo<LogQuery>(() => {
    const query: LogQuery = {
      limit: PAGE_SIZE,
      offset: page * PAGE_SIZE,
      provider: selectedId ?? undefined,
      upstream_model: model ?? undefined,
      after: detail.data?.start_at,
      before: detail.data?.end_at,
      ...outcomeQuery(outcome),
    };
    if (status === "200") {
      query.status_min = 200;
      query.status_max = 200;
    }
    if (status === "2xx") {
      query.status_min = 200;
      query.status_max = 299;
    }
    if (status === "400+") query.status_min = 400;
    return query;
  }, [page, selectedId, model, detail.data?.start_at, detail.data?.end_at, status, outcome]);

  const logs = useQuery<LogPage>({
    queryKey: ["provider-usage-logs", logFilter],
    queryFn: () => backend("query_logs", { query: logFilter }),
    enabled: open && tab === "requests" && !!selectedId && !!detail.data,
  });
  const pages = Math.max(1, Math.ceil((logs.data?.total ?? 0) / PAGE_SIZE));

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect
    setPage((current) => Math.min(current, pages - 1));
  }, [pages]);

  const selectModel = (value: string) => {
    setModel(value);
    setPage(0);
    setTab("requests");
  };
  const refresh = () => {
    void list.refetch();
    // Advancing detail.start_at/end_at changes the log query key, so the
    // request rows reload only after the new bounded snapshot arrives.
    if (selectedId) void detail.refetch();
  };

  const identity =
    detail.data ?? providers.find((provider) => provider.provider_id === selectedId);
  const identityName =
    identity &&
    ("provider_name" in identity ? identity.provider_name : identity.provider);
  const identityIcon =
    identity && ("provider_icon" in identity ? identity.provider_icon : null);
  const identityProtocol =
    identity &&
    ("provider_protocol" in identity ? identity.provider_protocol : null);

  return (
    <>
      <Dialog open={open} onOpenChange={onOpenChange}>
        <DialogContent className="flex h-[min(92vh,900px)] w-[min(96vw,1280px)] max-w-none flex-col overflow-hidden p-0 sm:w-[min(94vw,1280px)]">
          <DialogHeader className="shrink-0 border-b border-slate-200 px-4 py-4 pr-12 sm:px-6">
            <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
              <div className="min-w-0">
                <DialogTitle className="flex min-w-0 items-center gap-2">
                  {identity ? (
                    <ProviderIcon
                      iconKey={identityIcon ?? undefined}
                      name={identityName}
                      size={32}
                    />
                  ) : null}
                  <span className="truncate" title={identityName || undefined}>
                    {identityName || (zh ? "提供商用量" : "Provider Usage")}
                  </span>
                  {identityProtocol ? (
                    <Badge variant="outline" className="shrink-0 font-normal">
                      {protocolLabel(identityProtocol)}
                    </Badge>
                  ) : null}
                </DialogTitle>
                <DialogDescription>
                  {zh
                    ? "按上游模型查看尝试、Token 与性能。"
                    : "Attempts, tokens, and performance by upstream model."}
                </DialogDescription>
              </div>
              <div className="flex flex-wrap gap-2">
                <Select
                  value={String(hours)}
                  onValueChange={(value) => {
                    setHours(Number(value));
                    if (selectedId === null) defaultSelectionMade.current = false;
                    setModel(null);
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
                    <SelectItem value="6">{zh ? "最近 6 小时" : "Last 6h"}</SelectItem>
                    <SelectItem value="24">{zh ? "最近 24 小时" : "Last 24h"}</SelectItem>
                    <SelectItem value="72">{zh ? "最近 3 天" : "Last 3d"}</SelectItem>
                    <SelectItem value="168">{zh ? "最近 7 天" : "Last 7d"}</SelectItem>
                  </SelectContent>
                </Select>
                <Select
                  value={selectedId ?? undefined}
                  onValueChange={(value) => {
                    setSelectedId(value);
                    defaultSelectionMade.current = true;
                    setModel(null);
                    setPage(0);
                  }}
                >
                  <SelectTrigger
                    className="h-9 min-w-[180px] max-w-[280px]"
                    aria-label={zh ? "提供商" : "Provider"}
                  >
                    <SelectValue placeholder={zh ? "选择提供商" : "Select provider"} />
                  </SelectTrigger>
                  <SelectContent>
                    {selectedId &&
                    !providers.some(
                      (provider) => provider.provider_id === selectedId,
                    ) ? (
                      <SelectItem value={selectedId}>
                        <span className="flex min-w-0 items-center gap-2">
                          <ProviderIcon
                            iconKey={detail.data?.provider_icon ?? undefined}
                            name={detail.data?.provider_name}
                            size={20}
                          />
                          <span className="truncate">
                            {detail.data?.provider_name ||
                              (zh ? "未知提供商" : "Unknown provider")} · {detail.data && !detail.error
                                ? `${formatTokenCount(detail.data.request_count)} ${zh ? "次尝试" : "attempts"}`
                                : (detail.isLoading ? (zh ? "加载中…" : "Loading…") : (zh ? "不可用" : "Unavailable"))}
                          </span>
                        </span>
                      </SelectItem>
                    ) : null}
                    {providers.map((provider) => (
                      <SelectItem
                        key={provider.provider_id}
                        value={provider.provider_id}
                      >
                        <span className="flex min-w-0 items-center gap-2">
                          <ProviderIcon
                            iconKey={provider.provider_icon ?? undefined}
                            name={provider.provider}
                            size={20}
                          />
                          <span className="truncate" title={provider.provider}>
                            {provider.provider} · {formatTokenCount(provider.request_count)} {zh ? "次尝试" : "attempts"}
                          </span>
                        </span>
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
                      (list.isFetching || detail.isFetching || logs.isFetching) &&
                        "animate-spin",
                    )}
                  />
                </Button>
              </div>
            </div>
          </DialogHeader>
          <Tabs
            value={tab}
            onValueChange={(value) => setTab(value as Tab)}
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
                  {zh ? "尝试" : "Attempts"}
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
                  !selectedId || !detail.data || detail.data.request_count === 0
                }
                zh={zh}
                onRetry={() => {
                  if (list.error) void list.refetch();
                  if (selectedId && detail.error) void detail.refetch();
                }}
              >
                {detail.data ? (
                  <Overview detail={detail.data} zh={zh} onModel={selectModel} />
                ) : null}
              </QueryState>
            </TabsContent>
            <TabsContent
              value="requests"
              className="mt-0 min-h-0 flex-1 overflow-y-auto p-4 sm:p-6"
            >
              <QueryState
                loading={list.isLoading || (!!selectedId && detail.isLoading)}
                error={list.error ?? detail.error}
                empty={!selectedId || !detail.data}
                zh={zh}
                onRetry={refresh}
              >
                <Attempts
                  zh={zh}
                  outcome={outcome}
                  setOutcome={(value) => {
                    setOutcome(value);
                    setPage(0);
                  }}
                  status={status}
                  setStatus={(value) => {
                    setStatus(value);
                    setPage(0);
                  }}
                  model={model}
                  models={detail.data?.models ?? []}
                  setModel={(value) => {
                    setModel(value);
                    setPage(0);
                  }}
                  query={logs}
                  page={page}
                  pages={pages}
                  setPage={setPage}
                  onSelect={setSelectedLog}
                />
              </QueryState>
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
  if (loading) {
    return (
      <State
        icon={Loader2}
        spin
        text={zh ? "正在加载用量…" : "Loading usage…"}
      />
    );
  }
  if (error) {
    return (
      <State
        icon={AlertCircle}
        danger
        text={errorText(error, zh ? "加载失败" : "Failed to load usage")}
        action={
          <Button variant="outline" size="sm" onClick={onRetry}>
            <RefreshCw className="h-3.5 w-3.5" />
            {zh ? "重试" : "Retry"}
          </Button>
        }
      />
    );
  }
  if (empty) {
    return (
      <State
        icon={Activity}
        text={
          zh
            ? "此范围暂无提供商调用。可切换时间或提供商。"
            : "No provider calls in this range. Try another range or provider."
        }
      />
    );
  }
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
  onModel,
}: {
  detail: ProviderUsageDetail;
  zh: boolean;
  onModel: (model: string) => void;
}) {
  const { completedRate, unknownRate, authoritative } = usageOutcomeRates(detail);
  const unavailable = zh ? "不可用" : "Unavailable";
  const cachePercentage = Math.min(
    100,
    Math.max(
      0,
      detail.total_input_tokens
        ? (detail.total_cache_read_tokens / detail.total_input_tokens) * 100
        : 0,
    ),
  );
  const tps =
    detail.total_upstream_ms > 0
      ? detail.total_output_tokens / (detail.total_upstream_ms / 1000)
      : null;
  const cards = [
    {
      label: zh ? "尝试" : "Attempts",
      value: formatTokenCount(detail.request_count),
      icon: Activity,
      color: "from-blue-50 to-blue-100 text-blue-600",
    },
    {
      label: zh ? "完整成功率" : "Full Success Rate",
      value: completedRate == null ? unavailable : completedRate.toFixed(1) + "%",
      note: authoritative
        ? (zh
          ? `${formatTokenCount(detail.success_count)} 次确认完成 / ${formatTokenCount(detail.request_count)} 次尝试`
          : `${formatTokenCount(detail.success_count)} confirmed completed / ${formatTokenCount(detail.request_count)} attempts`)
        : (zh ? "结果统计元数据不可用" : "Outcome statistics unavailable"),
      icon: CheckCircle2,
      color: "from-emerald-50 to-emerald-100 text-emerald-600",
    },
    {
      label: zh ? "错误尝试" : "Error Attempts",
      value: authoritative ? formatTokenCount(detail.error_count) : unavailable,
      icon: XCircle,
      color: "from-red-50 to-red-100 text-red-600",
    },
    {
      label: zh ? "未知尝试" : "Unknown Attempts",
      value: authoritative ? formatTokenCount(detail.unknown_count) : unavailable,
      note: unknownRate == null ? unavailable : `${unknownRate.toFixed(1)}% ${zh ? "占总尝试" : "of all attempts"}`,
      icon: AlertCircle,
      color: "from-slate-50 to-slate-200 text-slate-600",
    },
    {
      label: zh ? "已取消尝试" : "Cancelled Attempts",
      value: authoritative ? formatTokenCount(detail.cancelled_count) : unavailable,
      icon: XCircle,
      color: "from-amber-50 to-amber-100 text-amber-600",
    },
    {
      label: zh ? "输出受限尝试" : "Output-limited Attempts",
      value: authoritative ? formatTokenCount(detail.output_limited_count) : unavailable,
      icon: AlertCircle,
      color: "from-orange-50 to-orange-100 text-orange-600",
    },
    {
      label: zh ? "输入 Token" : "Input Tokens",
      value: formatTokenCount(detail.total_input_tokens),
      icon: Zap,
      color: "from-sky-50 to-sky-100 text-sky-600",
    },
    {
      label: zh ? "缓存 Token" : "Cache Tokens",
      value: formatTokenCount(detail.total_cache_read_tokens),
      note:
        cachePercentage.toFixed(1) + "% " + (zh ? "输入占比" : "of input"),
      icon: Zap,
      color: "from-amber-50 to-amber-100 text-amber-600",
    },
    {
      label: zh ? "输出 Token" : "Output Tokens",
      value: formatTokenCount(detail.total_output_tokens),
      icon: Zap,
      color: "from-teal-50 to-emerald-100 text-emerald-600",
    },
    {
      label: zh ? "平均延迟" : "Avg Latency",
      value: formatDuration(detail.avg_duration_ms),
      icon: Timer,
      color: "from-violet-50 to-violet-100 text-violet-600",
    },
    {
      label: zh ? "平均首字延迟" : "Avg TTFT",
      value:
        detail.avg_first_token_ms == null
          ? "–"
          : formatDuration(detail.avg_first_token_ms),
      icon: Gauge,
      color: "from-fuchsia-50 to-fuchsia-100 text-fuchsia-600",
    },
    {
      label: "TPS",
      value: formatTps(tps),
      icon: Gauge,
      color: "from-cyan-50 to-cyan-100 text-cyan-600",
    },
    {
      label: zh ? "最后使用" : "Last Used",
      value:
        detail.last_used_at == null ? "–" : formatLogTime(detail.last_used_at),
      icon: Clock3,
      color: "from-slate-50 to-slate-200 text-slate-600",
      small: true,
    },
  ];
  const max = Math.max(1, ...detail.models.map((item) => item.request_count));

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
                  "inline-flex h-8 w-8 items-center justify-center rounded-lg bg-gradient-to-br",
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
              title={card.value}
            >
              {card.value}
            </div>
            {card.note ? (
              <div className="mt-1 text-xs text-slate-400">{card.note}</div>
            ) : null}
          </section>
        ))}
      </div>
      <section>
        <div className="mb-3 flex items-center gap-2">
          <Route className="h-4 w-4 text-blue-600" />
          <h3 className="text-sm font-semibold">
            {zh ? "上游模型" : "Upstream Models"}
          </h3>
        </div>
        {!detail.models.length ? (
          <State icon={Route} text={zh ? "暂无模型数据。" : "No model data."} />
        ) : (
          <div className="max-h-[360px] overflow-auto rounded-xl border border-slate-200">
            <table className="w-full min-w-[1050px] text-left">
              <thead className="sticky top-0 z-10 bg-slate-50 text-[11px] uppercase text-slate-500">
                <tr>
                  <th className="px-4 py-3">{zh ? "上游模型" : "Upstream Model"}</th>
                  <th className="px-3 py-3">{zh ? "尝试数 / 份额" : "Attempts / Share"}</th>
                  <th className="px-3 py-3 text-right">{zh ? "错误尝试 / 占比" : "Error Attempts / Rate"}</th>
                  <th className="px-3 py-3 text-right">{zh ? "Token (入/缓存/出)" : "Tokens (in/cache/out)"}</th>
                  <th className="px-3 py-3 text-right">{zh ? "延迟" : "Latency"}</th>
                  <th className="px-3 py-3 text-right">TTFT</th>
                  <th className="px-3 py-3 text-right">TPS</th>
                  <th className="px-4 py-3 text-right">{zh ? "最后调用" : "Last Called"}</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-slate-100">
                {detail.models.map((item) => {
                  const share = detail.request_count
                    ? (item.request_count / detail.request_count) * 100
                    : 0;
                  const failureRate = authoritative && Number.isSafeInteger(item.error_count)
                    && item.error_count >= 0 && item.error_count <= item.request_count && item.request_count > 0
                    ? (item.error_count / item.request_count) * 100
                    : null;
                  const modelTps =
                    item.total_upstream_ms > 0
                      ? item.total_output_tokens / (item.total_upstream_ms / 1000)
                      : null;
                  const filterable = item.upstream_model.length > 0;
                  const interactionProps = filterable
                    ? {
                        role: "button" as const,
                        tabIndex: 0,
                        onClick: () => onModel(item.upstream_model),
                        onKeyDown: (event: React.KeyboardEvent<HTMLTableRowElement>) => {
                          if (event.key === "Enter" || event.key === " ") {
                            event.preventDefault();
                            onModel(item.upstream_model);
                          }
                        },
                      }
                    : {};
                  return (
                    <tr
                      key={item.upstream_model}
                      {...interactionProps}
                      className={cn(
                        filterable &&
                          "cursor-pointer hover:bg-blue-50/50 focus:bg-blue-50 focus:outline-none",
                      )}
                    >
                      <td
                        className="max-w-[260px] truncate px-4 py-3 font-mono text-xs"
                        title={item.upstream_model || undefined}
                      >
                        {item.upstream_model || (zh ? "未知模型" : "Unknown model")}
                      </td>
                      <td className="px-3 py-3">
                        <div className="flex items-center gap-2">
                          <div className="h-1.5 w-20 overflow-hidden rounded-full bg-slate-100">
                            <div
                              className="h-full rounded-full bg-blue-500"
                              style={{
                                width:
                                  Math.max(3, (item.request_count / max) * 100) + "%",
                              }}
                            />
                          </div>
                          <span className="whitespace-nowrap text-xs tabular-nums">
                            {formatTokenCount(item.request_count)} · {share.toFixed(1)}%
                          </span>
                        </div>
                      </td>
                      <td className="px-3 py-3 text-right text-xs text-red-600">
                        {failureRate == null ? unavailable : `${formatTokenCount(item.error_count)} · ${failureRate.toFixed(1)}%`}
                      </td>
                      <td className="px-3 py-3 text-right font-mono text-xs">
                        {formatTokenCount(item.total_input_tokens)} /{" "}
                        {formatTokenCount(item.total_cache_read_tokens)} /{" "}
                        {formatTokenCount(item.total_output_tokens)}
                      </td>
                      <td className="px-3 py-3 text-right text-xs">
                        {formatDuration(item.avg_duration_ms)}
                      </td>
                      <td className="px-3 py-3 text-right text-xs">
                        {item.avg_first_token_ms == null
                          ? "–"
                          : formatDuration(item.avg_first_token_ms)}
                      </td>
                      <td className="px-3 py-3 text-right text-xs">
                        {formatTps(modelTps)}
                      </td>
                      <td className="whitespace-nowrap px-4 py-3 text-right text-xs text-slate-500">
                        {item.last_used_at == null
                          ? "–"
                          : formatLogTime(item.last_used_at)}
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

function Attempts({
  zh,
  outcome,
  setOutcome,
  status,
  setStatus,
  model,
  models,
  setModel,
  query,
  page,
  pages,
  setPage,
  onSelect,
}: {
  zh: boolean;
  outcome: string;
  setOutcome: (value: string) => void;
  status: StatusFilter;
  setStatus: (value: StatusFilter) => void;
  model: string | null;
  models: ProviderModelUsageStats[];
  setModel: (value: string | null) => void;
  query: ReturnType<typeof useQuery<LogPage>>;
  page: number;
  pages: number;
  setPage: (value: number) => void;
  onSelect: (value: RequestLog) => void;
}) {
  const selectableModels = models.filter((item) => item.upstream_model.length > 0);
  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex flex-wrap gap-2">
          <Select
            value={model == null ? ALL_MODELS_VALUE : modelValue(model)}
            onValueChange={(value) => {
              if (value === ALL_MODELS_VALUE) {
                setModel(null);
                return;
              }
              const decoded = JSON.parse(value) as [string, string];
              setModel(decoded[1]);
            }}
          >
            <SelectTrigger className="h-9 min-w-[220px] max-w-[420px]">
              <Route className="mr-1 h-3.5 w-3.5" />
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={ALL_MODELS_VALUE}>
                {zh ? "全部上游模型" : "All Upstream Models"}
              </SelectItem>
              {selectableModels.map((item) => (
                <SelectItem
                  key={item.upstream_model}
                  value={modelValue(item.upstream_model)}
                >
                  {item.upstream_model}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <OutcomeFilter
            value={outcomeQuery(outcome)}
            onChange={(next) => setOutcome(outcomeFilterValue(next))}
            isZh={zh}
          />
          <Select
            value={status}
            onValueChange={(value) => setStatus(value as StatusFilter)}
          >
            <SelectTrigger className="h-9 w-[170px]" aria-label={zh ? "HTTP 状态" : "HTTP status"}>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="all">{zh ? "全部 HTTP 状态" : "All HTTP statuses"}</SelectItem>
              <SelectItem value="200">HTTP 200</SelectItem>
              <SelectItem value="2xx">HTTP 2xx</SelectItem>
              <SelectItem value="400+">HTTP 4xx / 5xx</SelectItem>
            </SelectContent>
          </Select>
        </div>
        {query.isFetching ? (
          <span className="flex items-center gap-1 text-xs text-slate-400">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
            {zh ? "刷新中" : "Refreshing"}
          </span>
        ) : null}
      </div>
      {query.isLoading ? (
        <State
          icon={Loader2}
          spin
          text={zh ? "正在加载尝试…" : "Loading attempts…"}
        />
      ) : query.error ? (
        <State
          icon={AlertCircle}
          danger
          text={errorText(
            query.error,
            zh ? "尝试加载失败" : "Failed to load attempts",
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
          text={zh ? "没有匹配的尝试。" : "No matching attempts."}
        />
      ) : (
        <>
          <div className="overflow-x-auto rounded-xl border border-slate-200">
            <table className="w-full min-w-[1120px] text-left">
              <thead className="bg-slate-50 text-[11px] uppercase text-slate-500">
                <tr>
                  <th className="px-3 py-3">{zh ? "时间" : "Time"}</th>
                  <th className="px-3 py-3">{zh ? "HTTP / 结果" : "HTTP / Result"}</th>
                  <th className="px-3 py-3">{zh ? "上游模型" : "Upstream Model"}</th>
                  <th className="px-3 py-3">{zh ? "协议" : "Protocol"}</th>
                  <th className="px-3 py-3">{zh ? "类型" : "Type"}</th>
                  <th className="px-3 py-3 text-right">{zh ? "Token" : "Tokens"}</th>
                  <th className="px-3 py-3 text-right">{zh ? "总延迟" : "Total Latency"}</th>
                  <th className="px-3 py-3 text-right">TTFT</th>
                  <th className="px-3 py-3 text-right">TPS</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-slate-100">
                {query.data.items.map((log) => {
                  const code = log.client_status_code;
                  return (
                    <tr
                      key={log.id}
                      role="button"
                      tabIndex={0}
                      onClick={() => onSelect(log)}
                      onKeyDown={(event) => {
                        if (event.key === "Enter" || event.key === " ") {
                          event.preventDefault();
                          onSelect(log);
                        }
                      }}
                      className="cursor-pointer hover:bg-slate-50 focus:bg-blue-50 focus:outline-none"
                    >
                      <td className="whitespace-nowrap px-3 py-2.5 text-xs text-slate-500">
                        {formatLogTime(log.created_at)}
                      </td>
                      <td className="px-3 py-2.5">
                        <div className="flex flex-wrap items-center gap-1.5">
                          <Badge variant="outline" className="text-[10px] text-slate-600">
                            HTTP {code ?? "–"}
                          </Badge>
                          <ResultBadge log={log} isZh={zh} />
                        </div>
                      </td>
                      <td
                        className="max-w-[300px] truncate px-3 py-2.5 font-mono text-xs"
                        title={log.upstream_model}
                      >
                        {log.upstream_model || (zh ? "未知模型" : "Unknown model")}
                      </td>
                      <td className="px-3 py-2.5 text-xs text-slate-500">
                        {protocolLabel(log.client_protocol)} →{" "}
                        {protocolLabel(log.upstream_protocol)}
                      </td>
                      <td className="px-3 py-2.5">
                        <Badge variant="outline" className="text-[10px]">
                          {log.is_stream || (log.stream_chunks_count ?? 0) > 0
                            ? "SSE"
                            : "JSON"}
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
                ? `第 ${page + 1} / ${pages} 页，共 ${query.data.total} 次尝试`
                : `Page ${page + 1} of ${pages} · ${query.data.total} attempts`}
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
