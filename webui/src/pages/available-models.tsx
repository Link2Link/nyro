import { type ReactNode, useId, useMemo, useState } from "react";
import { type UseQueryResult, useQueries, useQuery } from "@tanstack/react-query";
import { useNavigate } from "react-router-dom";
import {
  Boxes,
  ChevronDown,
  ChevronRight,
  CircleAlert,
  CircleCheck,
  CircleDashed,
  Eye,
  ListChecks,
  ListFilter,
  Loader2,
  RefreshCw,
  Route as RouteIcon,
  Search,
} from "lucide-react";

import { backend } from "@/lib/backend";
import { formatDuration, formatLocalDateTime, formatTokenCount, formatTps } from "@/lib/format";
import { useLocale } from "@/lib/i18n";
import {
  loadModelProbeResults,
  saveModelProbeResults,
  type ProviderModelProbeStore,
} from "@/lib/model-probe";
import type {
  Model as ModelMapping,
  ModelCapabilities,
  ModelProbeOutcome,
  ModelUsageStats,
  ModelProbeResult,
  Provider,
} from "@/lib/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ProviderIcon } from "@/components/ui/provider-icon";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";

const ALL_PROVIDERS = "__all__";
const MODEL_BATCH_SIZE = 40;

function normalizeSearch(value: string) {
  return value.toLocaleLowerCase().replace(/[\s._\p{Pd}/:]+/gu, "");
}

function uniqueSorted(values: string[]) {
  return [...new Set(values.map((value) => value.trim()).filter(Boolean))]
    .sort((a, b) => a.localeCompare(b, undefined, { sensitivity: "base" }));
}

function modelKey(providerId: string, model: string) {
  return providerId + "\u0000" + model;
}

function formatTokens(value?: number | null) {
  if (!value || value <= 0) return null;
  if (value >= 1_000_000) {
    const amount = value / 1_000_000;
    return (Number.isInteger(amount) ? amount : amount.toFixed(1)) + "M";
  }
  if (value >= 1_000) {
    const amount = value / 1_000;
    return (Number.isInteger(amount) ? amount : amount.toFixed(1)) + "K";
  }
  return String(value);
}

function formatPrice(value?: number | null) {
  if (value == null || !Number.isFinite(value)) return null;
  const digits = value > 0 && value < 0.01 ? 4 : value < 1 ? 3 : 2;
  return "$" + value.toFixed(digits) + " / 1M";
}

function formatTimestamp(value: string, locale: string) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return null;
  return new Intl.DateTimeFormat(locale, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  }).format(date);
}

function buildMappingIndex(mappings: ModelMapping[]) {
  const index = new Map<string, string[]>();
  for (const mapping of mappings) {
    const targets = mapping.targets?.length
      ? mapping.targets
      : [{ provider_id: mapping.target_provider, model: mapping.target_model }];
    for (const target of targets) {
      const key = modelKey(target.provider_id, target.model);
      const names = index.get(key) ?? [];
      if (!names.includes(mapping.name)) names.push(mapping.name);
      index.set(key, names);
    }
  }
  for (const names of index.values()) {
    names.sort((a, b) => a.localeCompare(b, undefined, { sensitivity: "base" }));
  }
  return index;
}

function buildProbeIndex(store: ProviderModelProbeStore) {
  const index = new Map<string, ModelProbeResult>();
  for (const [providerId, record] of Object.entries(store)) {
    for (const result of record.results) {
      index.set(modelKey(providerId, result.model), result);
    }
  }
  return index;
}

function IconAction({
  label,
  children,
  onClick,
  disabled = false,
  tone = "neutral",
  expanded,
  controls,
}: {
  label: string;
  children: ReactNode;
  onClick: () => void;
  disabled?: boolean;
  tone?: "neutral" | "violet";
  expanded?: boolean;
  controls?: string;
}) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          aria-label={label}
          aria-expanded={expanded}
          aria-controls={controls}
          disabled={disabled}
          onClick={onClick}
          className={
            tone === "violet"
              ? "inline-flex h-8 w-8 shrink-0 items-center justify-center rounded-lg text-slate-400 transition-colors hover:bg-violet-50 hover:text-violet-600 disabled:cursor-not-allowed disabled:opacity-45"
              : "inline-flex h-8 w-8 shrink-0 items-center justify-center rounded-lg text-slate-400 transition-colors hover:bg-slate-100 hover:text-slate-700 disabled:cursor-not-allowed disabled:opacity-45"
          }
        >
          {children}
        </button>
      </TooltipTrigger>
      <TooltipContent>{label}</TooltipContent>
    </Tooltip>
  );
}

function ProbeBadge({ result, isZh }: { result?: ModelProbeResult; isZh: boolean }) {
  if (!result) {
    return (
      <Badge variant="outline" className="gap-1 border-slate-200 text-slate-500">
        <CircleDashed className="h-3 w-3" />
        {isZh ? "未测试" : "Untested"}
      </Badge>
    );
  }
  if (result.success) {
    return (
      <Badge variant="success" className="gap-1">
        <CircleCheck className="h-3 w-3" />
        {isZh ? "可用" : "Available"}
        <span className="opacity-70">{result.latency_ms}ms</span>
      </Badge>
    );
  }
  return (
    <Badge variant="danger" className="gap-1" title={result.error ?? undefined}>
      <CircleAlert className="h-3 w-3" />
      {isZh ? "不可用" : "Unavailable"}
    </Badge>
  );
}

function CapabilityDetail({ provider, model, isZh }: { provider: Provider; model: string; isZh: boolean }) {
  const capabilities = useQuery<ModelCapabilities>({
    queryKey: ["model-capabilities", provider.id, model],
    queryFn: () => backend("get_model_capabilities", { providerId: provider.id, model }),
    retry: false,
    staleTime: 5 * 60_000,
  });
  const usage = useQuery<ModelUsageStats>({
    queryKey: ["model-usage", provider.id, model],
    queryFn: () => backend("get_model_usage_stats", { providerId: provider.id, model }),
    retry: false,
    staleTime: 15_000,
  });

  const caps = capabilities.data;
  const stats = usage.data;
  const context = formatTokens(caps?.context_window);
  const output = formatTokens(caps?.output_max_tokens);
  const inputPrice = formatPrice(caps?.input_cost);
  const outputPrice = formatPrice(caps?.output_cost);

  return (
    <div>
      <div className="border-b border-slate-200/70 px-4 py-4">
        <div className="flex flex-wrap items-baseline justify-between gap-2">
          <div className="text-xs font-semibold text-slate-700">{isZh ? "调用统计" : "Usage"}</div>
          {stats && stats.recent_sample_count > 0 && (
            <div className="text-[11px] text-slate-400">
              {isZh
                ? "性能取最近 " + stats.recent_sample_count + " 次调用平均值"
                : "Performance averages from the last " + stats.recent_sample_count + " calls"}
            </div>
          )}
        </div>
        {usage.isLoading ? (
          <div className="mt-3 flex items-center gap-2 text-xs text-slate-500">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
            {isZh ? "加载调用统计..." : "Loading usage statistics..."}
          </div>
        ) : usage.error || !stats ? (
          <div className="mt-3 text-xs text-slate-500">
            {isZh ? "调用统计暂不可用" : "Usage statistics unavailable"}
          </div>
        ) : (
          <div className="mt-3 grid gap-x-5 gap-y-4 sm:grid-cols-2 lg:grid-cols-4 xl:grid-cols-7">
            <div>
              <div className="text-[11px] font-medium text-slate-400">{isZh ? "调用次数" : "Calls"}</div>
              <div className="mt-1 text-base font-semibold text-slate-800">{stats.request_count.toLocaleString()}</div>
            </div>
            <div>
              <div className="text-[11px] font-medium text-slate-400">{isZh ? "输入 Token" : "Input Tokens"}</div>
              <div className="mt-1 text-base font-semibold text-slate-800">{formatTokenCount(stats.total_input_tokens)}</div>
            </div>
            <div>
              <div className="text-[11px] font-medium text-slate-400">{isZh ? "输出 Token" : "Output Tokens"}</div>
              <div className="mt-1 text-base font-semibold text-slate-800">{formatTokenCount(stats.total_output_tokens)}</div>
            </div>
            <div>
              <div className="text-[11px] font-medium text-slate-400">{isZh ? "缓存命中 Token" : "Cache-hit Tokens"}</div>
              <div className="mt-1 text-base font-semibold text-slate-800">{formatTokenCount(stats.total_cache_read_tokens)}</div>
            </div>
            <div>
              <div className="text-[11px] font-medium text-slate-400">{isZh ? "最后调用" : "Last Called"}</div>
              <div className="mt-1 text-sm font-medium text-slate-700">{formatLocalDateTime(stats.last_called_at)}</div>
            </div>
            <div>
              <div className="text-[11px] font-medium text-slate-400">{isZh ? "平均 TPS" : "Average TPS"}</div>
              <div className="mt-1 text-base font-semibold text-slate-800">{formatTps(stats.average_tps)}</div>
            </div>
            <div>
              <div className="text-[11px] font-medium text-slate-400">{isZh ? "平均首字延迟" : "Average First Token"}</div>
              <div className="mt-1 text-base font-semibold text-slate-800">{formatDuration(stats.average_first_token_ms)}</div>
            </div>
          </div>
        )}
      </div>

      {capabilities.isLoading ? (
        <div className="flex items-center gap-2 px-4 py-4 text-xs text-slate-500">
          <Loader2 className="h-3.5 w-3.5 animate-spin" />
          {isZh ? "加载模型信息..." : "Loading model details..."}
        </div>
      ) : !caps ? (
        <div className="px-4 py-4 text-xs text-slate-500">
          {isZh ? "暂无可用的模型元数据" : "No model metadata available"}
        </div>
      ) : (
        <div className="grid gap-4 px-4 py-4 sm:grid-cols-2 xl:grid-cols-4">
          <div>
            <div className="text-[11px] font-medium text-slate-400">{isZh ? "Token 上限" : "Token Limits"}</div>
            <div className="mt-1 flex flex-wrap gap-1.5">
              {context && <Badge variant="secondary">{isZh ? "上下文" : "Context"} {context}</Badge>}
              {output && <Badge variant="secondary">{isZh ? "最大输出" : "Max output"} {output}</Badge>}
              {caps.embedding_length != null && caps.embedding_length > 0 && (
                <Badge variant="secondary">{isZh ? "嵌入维度" : "Embedding"} {caps.embedding_length}</Badge>
              )}
            </div>
          </div>
          <div>
            <div className="text-[11px] font-medium text-slate-400">{isZh ? "能力" : "Capabilities"}</div>
            <div className="mt-1 flex flex-wrap gap-1.5">
              {caps.tool_call && <Badge variant="success">{isZh ? "工具调用" : "Tools"}</Badge>}
              {caps.reasoning && <Badge variant="success">{isZh ? "推理" : "Reasoning"}</Badge>}
              {!caps.tool_call && !caps.reasoning && <span className="text-xs text-slate-400">–</span>}
            </div>
          </div>
          <div>
            <div className="text-[11px] font-medium text-slate-400">{isZh ? "模态" : "Modalities"}</div>
            <div className="mt-1 space-y-1 text-xs text-slate-600">
              <div>{isZh ? "输入" : "Input"}: {caps.input_modalities.join(", ") || "–"}</div>
              <div>{isZh ? "输出" : "Output"}: {caps.output_modalities.join(", ") || "–"}</div>
            </div>
          </div>
          <div>
            <div className="text-[11px] font-medium text-slate-400">{isZh ? "价格" : "Pricing"}</div>
            <div className="mt-1 space-y-1 text-xs text-slate-600">
              <div>{isZh ? "输入" : "Input"}: {inputPrice ?? "–"}</div>
              <div>{isZh ? "输出" : "Output"}: {outputPrice ?? "–"}</div>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

function ModelRow({
  provider,
  model,
  mappings,
  probe,
  isZh,
}: {
  provider: Provider;
  model: string;
  mappings: string[];
  probe?: ModelProbeResult;
  isZh: boolean;
}) {
  const navigate = useNavigate();
  const [expanded, setExpanded] = useState(false);
  const detailId = useId();

  function createMapping() {
    const params = new URLSearchParams({ provider: provider.id, model });
    navigate("/models?" + params.toString());
  }

  return (
    <div className="border-t border-slate-200/80 first:border-t-0">
      <div className="grid min-h-14 items-center gap-3 px-3 py-2.5 md:grid-cols-[minmax(0,2.1fr)_minmax(7.5rem,.7fr)_minmax(0,1.35fr)_4.75rem] md:px-4">
        <button
          type="button"
          aria-expanded={expanded}
          aria-controls={detailId}
          onClick={() => setExpanded((value) => !value)}
          className="flex min-w-0 items-center gap-2 text-left"
        >
          {expanded ? (
            <ChevronDown className="h-4 w-4 shrink-0 text-slate-400" />
          ) : (
            <ChevronRight className="h-4 w-4 shrink-0 text-slate-400" />
          )}
          <span className="min-w-0 break-all font-mono text-[13px] font-medium text-slate-800" title={model}>
            {model}
          </span>
        </button>
        <div><ProbeBadge result={probe} isZh={isZh} /></div>
        <div className="flex min-w-0 flex-wrap gap-1.5">
          {mappings.length > 0 ? mappings.map((name) => (
            <Badge key={name} variant="secondary" className="max-w-full truncate" title={name}>
              {name}
            </Badge>
          )) : (
            <span className="text-xs text-slate-400">{isZh ? "未映射" : "Not mapped"}</span>
          )}
        </div>
        <div className="flex items-center justify-end gap-1">
          <IconAction label={isZh ? "创建映射" : "Create mapping"} onClick={createMapping}>
            <RouteIcon className="h-4 w-4" />
          </IconAction>
          <IconAction
            label={expanded ? (isZh ? "收起详情" : "Collapse details") : (isZh ? "查看详情" : "View details")}
            onClick={() => setExpanded((value) => !value)}
            expanded={expanded}
            controls={detailId}
          >
            {expanded ? <ChevronDown className="h-4 w-4" /> : <ChevronRight className="h-4 w-4" />}
          </IconAction>
        </div>
      </div>
      {expanded && (
        <div id={detailId} className="border-t border-slate-200/70 bg-slate-50/70">
          <CapabilityDetail provider={provider} model={model} isZh={isZh} />
        </div>
      )}
    </div>
  );
}

type CatalogQuery = UseQueryResult<string[], Error>;

function ProviderSection({
  provider,
  query,
  models,
  mappings,
  probes,
  probeStore,
  isZh,
  locale,
  search,
  showAllModels,
  collapsed,
  probing,
  probeError,
  onToggle,
  onProbe,
}: {
  provider: Provider;
  query: CatalogQuery;
  models: string[];
  mappings: Map<string, string[]>;
  probes: Map<string, ModelProbeResult>;
  probeStore: ProviderModelProbeStore;
  isZh: boolean;
  locale: string;
  search: string;
  showAllModels: boolean;
  collapsed: boolean;
  probing: boolean;
  probeError?: string;
  onToggle: () => void;
  onProbe: () => void;
}) {
  const [pagination, setPagination] = useState({ key: "", count: MODEL_BATCH_SIZE });
  const contentId = useId();
  const normalized = normalizeSearch(search);
  const availableModels = useMemo(() => models.filter((model) => (
    probes.get(modelKey(provider.id, model))?.success !== false
  )), [models, probes, provider.id]);
  const displayedModels = useMemo(() => showAllModels
    ? availableModels
    : availableModels.filter((model) => mappings.has(modelKey(provider.id, model))),
  [availableModels, mappings, provider.id, showAllModels]);
  const filteredModels = useMemo(() => displayedModels.filter((model) => {
    if (!normalized) return true;
    const aliases = mappings.get(modelKey(provider.id, model)) ?? [];
    return [provider.name, provider.vendor ?? "", model, ...aliases]
      .some((value) => normalizeSearch(value).includes(normalized));
  }), [displayedModels, mappings, normalized, provider.id, provider.name, provider.vendor]);

  const pageKey = search + "\u0000" + displayedModels.join("\u0000");
  const visibleCount = pagination.key === pageKey ? pagination.count : MODEL_BATCH_SIZE;
  const record = probeStore[provider.id];
  const testedAt = record?.tested_at ? formatTimestamp(record.tested_at, locale) : null;
  const availableCount = record?.results.filter((result) => result.success).length ?? 0;
  const visibleModels = filteredModels.slice(0, visibleCount);

  return (
    <section className="glass overflow-hidden rounded-2xl">
      <div className="flex min-h-16 items-center gap-3 px-4 py-3">
        <button
          type="button"
          aria-expanded={!collapsed}
          aria-controls={contentId}
          onClick={onToggle}
          className="flex min-w-0 flex-1 items-center gap-3 text-left"
        >
          {collapsed ? (
            <ChevronRight className="h-4 w-4 shrink-0 text-slate-400" />
          ) : (
            <ChevronDown className="h-4 w-4 shrink-0 text-slate-400" />
          )}
          <ProviderIcon
            name={provider.name}
            protocol={provider.protocol}
            baseUrl={provider.base_url}
            size={34}
          />
          <div className="min-w-0 flex-1">
            <div className="flex min-w-0 flex-wrap items-center gap-2">
              <h2 className="truncate text-sm font-semibold text-slate-900">{provider.name}</h2>
              <Badge variant="outline" className="border-slate-200 text-slate-500">{provider.protocol}</Badge>
              <Badge variant={provider.is_enabled ? "success" : "secondary"}>
                {provider.is_enabled ? (isZh ? "已启用" : "Enabled") : (isZh ? "已禁用" : "Disabled")}
              </Badge>
            </div>
            <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-slate-500">
              <span>{isZh ? displayedModels.length + " 个模型" : displayedModels.length + " models"}</span>
              {record && <span className="text-green-600">{isZh ? availableCount + " 个已验证可用" : availableCount + " verified available"}</span>}
              {testedAt && <span>{isZh ? "测试于 " + testedAt : "Tested " + testedAt}</span>}
            </div>
          </div>
        </button>
        <div className="flex shrink-0 items-center gap-1">
          <IconAction
            label={isZh ? "刷新模型目录" : "Refresh model catalog"}
            onClick={() => void query.refetch()}
            disabled={!provider.is_enabled || query.isFetching}
          >
            <RefreshCw className={query.isFetching ? "h-4 w-4 animate-spin" : "h-4 w-4"} />
          </IconAction>
          <IconAction
            label={isZh ? "向每个模型发送请求以测试可用性" : "Send a request to every model to test availability"}
            onClick={onProbe}
            disabled={!provider.is_enabled || probing}
            tone="violet"
          >
            {probing ? <Loader2 className="h-4 w-4 animate-spin" /> : <ListChecks className="h-4 w-4" />}
          </IconAction>
        </div>
      </div>

      {!collapsed && (
        <div id={contentId} className="border-t border-slate-200/80">
          {!provider.is_enabled ? (
            <div className="px-4 py-8 text-center text-sm text-slate-500">
              {isZh ? "供应商已禁用" : "Provider is disabled"}
            </div>
          ) : query.isLoading ? (
            <div className="flex items-center justify-center gap-2 px-4 py-8 text-sm text-slate-500">
              <Loader2 className="h-4 w-4 animate-spin" />
              {isZh ? "正在加载模型目录..." : "Loading model catalog..."}
            </div>
          ) : query.error ? (
            <div className="px-4 py-8 text-center">
              <CircleAlert className="mx-auto h-6 w-6 text-red-400" />
              <p className="mt-2 text-sm text-red-500">{isZh ? "模型目录加载失败" : "Failed to load model catalog"}</p>
              <p className="mt-1 text-xs text-slate-400">{String(query.error)}</p>
            </div>
          ) : null}

          {probeError && (
            <div className="border-b border-red-100 bg-red-50/70 px-4 py-2 text-xs text-red-600">{probeError}</div>
          )}

          {provider.is_enabled && !query.isLoading && !query.error && filteredModels.length === 0 && (
            <div className="px-4 py-8 text-center text-sm text-slate-500">
              {models.length === 0
                ? (isZh ? "供应商未返回模型" : "No models returned by this provider")
                : availableModels.length === 0
                  ? (isZh ? "暂无可用模型" : "No available models")
                  : displayedModels.length === 0 && !showAllModels
                    ? (isZh ? "暂无已映射模型" : "No mapped models")
                    : (isZh ? "没有匹配的模型" : "No matching models")}
            </div>
          )}

          {visibleModels.length > 0 && (
            <div>
              <div className="hidden grid-cols-[minmax(0,2.1fr)_minmax(7.5rem,.7fr)_minmax(0,1.35fr)_4.75rem] gap-3 bg-slate-50/65 px-4 py-2 text-[11px] font-medium text-slate-400 md:grid">
                <span>{isZh ? "模型 ID" : "Model ID"}</span>
                <span>{isZh ? "状态" : "Status"}</span>
                <span>{isZh ? "模型映射" : "Model Mapping"}</span>
                <span className="text-right">{isZh ? "操作" : "Actions"}</span>
              </div>
              {visibleModels.map((model) => (
                <ModelRow
                  key={model}
                  provider={provider}
                  model={model}
                  mappings={mappings.get(modelKey(provider.id, model)) ?? []}
                  probe={probes.get(modelKey(provider.id, model))}
                  isZh={isZh}
                />
              ))}
              {visibleCount < filteredModels.length && (
                <div className="border-t border-slate-200/80 px-4 py-3 text-center">
                  <Button
                    variant="secondary"
                    size="sm"
                    onClick={() => setPagination({ key: pageKey, count: visibleCount + MODEL_BATCH_SIZE })}
                  >
                    {isZh
                      ? "显示更多（剩余 " + (filteredModels.length - visibleCount) + "）"
                      : "Show more (" + (filteredModels.length - visibleCount) + " remaining)"}
                  </Button>
                </div>
              )}
            </div>
          )}
        </div>
      )}
    </section>
  );
}

export default function AvailableModelsPage() {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const [search, setSearch] = useState("");
  const [providerFilter, setProviderFilter] = useState(ALL_PROVIDERS);
  const [showAllModels, setShowAllModels] = useState(false);
  const [collapsedProviders, setCollapsedProviders] = useState<Set<string>>(() => new Set());
  const [probeStore, setProbeStore] = useState<ProviderModelProbeStore>(loadModelProbeResults);
  const [probingId, setProbingId] = useState<string | null>(null);
  const [probeErrors, setProbeErrors] = useState<Record<string, string>>({});

  const providersQuery = useQuery<Provider[]>({
    queryKey: ["providers"],
    queryFn: () => backend("get_providers"),
  });
  const mappingsQuery = useQuery<ModelMapping[]>({
    queryKey: ["routes"],
    queryFn: () => backend("list_models"),
  });

  const providers = useMemo(() => [...(providersQuery.data ?? [])].sort((a, b) => {
    if (a.is_enabled !== b.is_enabled) return a.is_enabled ? -1 : 1;
    return a.name.localeCompare(b.name, undefined, { sensitivity: "base" });
  }), [providersQuery.data]);

  const catalogQueries = useQueries({
    queries: providers.map((provider) => ({
      queryKey: ["provider-models", provider.id],
      queryFn: () => backend<string[]>("get_provider_models", { id: provider.id }),
      enabled: provider.is_enabled,
      retry: false,
      staleTime: 60_000,
      select: uniqueSorted,
    })),
  });

  const mappings = useMemo(() => buildMappingIndex(mappingsQuery.data ?? []), [mappingsQuery.data]);
  const probes = useMemo(() => buildProbeIndex(probeStore), [probeStore]);
  const filteredProviders = providers.filter((provider) => (
    providerFilter === ALL_PROVIDERS || provider.id === providerFilter
  ));
  const totalModels = providers.reduce((sum, provider, index) => {
    if (!provider.is_enabled) return sum;
    const models = catalogQueries[index]?.data ?? [];
    return sum + models.filter((model) => (
      probes.get(modelKey(provider.id, model))?.success !== false
    )).length;
  }, 0);
  const mappedModels = providers.reduce((sum, provider, index) => {
    if (!provider.is_enabled) return sum;
    const models = catalogQueries[index]?.data ?? [];
    return sum + models.filter((model) => (
      probes.get(modelKey(provider.id, model))?.success !== false
      && mappings.has(modelKey(provider.id, model))
    )).length;
  }, 0);
  const isRefreshing = catalogQueries.some((query) => query.isFetching);

  async function refreshAll() {
    await Promise.all(catalogQueries.map((query, index) => (
      providers[index]?.is_enabled ? query.refetch() : Promise.resolve()
    )));
  }

  async function probeProvider(provider: Provider) {
    setProbingId(provider.id);
    setProbeErrors((current) => {
      const next = { ...current };
      delete next[provider.id];
      return next;
    });
    try {
      const outcome = await backend<ModelProbeOutcome>("probe_provider_models", { id: provider.id });
      const nextRecord = { results: outcome.results, tested_at: new Date().toISOString() };
      setProbeStore((current) => {
        const next = { ...current, [provider.id]: nextRecord };
        saveModelProbeResults(next);
        return next;
      });
    } catch (error) {
      setProbeErrors((current) => ({
        ...current,
        [provider.id]: error instanceof Error ? error.message : String(error),
      }));
    } finally {
      setProbingId(null);
    }
  }

  function toggleProvider(providerId: string) {
    setCollapsedProviders((current) => {
      const next = new Set(current);
      if (next.has(providerId)) next.delete(providerId);
      else next.add(providerId);
      return next;
    });
  }

  if (providersQuery.isLoading) {
    return <div className="py-12 text-center text-sm text-slate-500">{isZh ? "加载中..." : "Loading..."}</div>;
  }

  return (
    <TooltipProvider>
      <div className="space-y-5">
        <div className="flex flex-col gap-3 sm:flex-row sm:items-end sm:justify-between">
          <div>
            <h1 className="text-2xl font-bold text-slate-900">{isZh ? "可用模型" : "Available Models"}</h1>
            <p className="mt-1 text-sm text-slate-500">
              {isZh ? "查看供应商模型目录、可达性与映射状态" : "Provider catalogs, availability, and mapping status"}
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-2 text-xs text-slate-500">
            <span>{isZh ? providers.length + " 个供应商" : providers.length + " providers"}</span>
            <span className="text-slate-300">/</span>
            <span>
              {showAllModels
                ? (isZh ? totalModels + " 个模型" : totalModels + " models")
                : (isZh ? mappedModels + " 个已映射模型" : mappedModels + " mapped models")}
            </span>
            {showAllModels && (
              <>
                <span className="text-slate-300">/</span>
                <span>{isZh ? mappedModels + " 个已映射" : mappedModels + " mapped"}</span>
              </>
            )}
          </div>
        </div>

        <div className="glass flex flex-col gap-3 rounded-2xl p-3 md:flex-row md:items-center">
          <div className="relative min-w-0 flex-1">
            <Search className="pointer-events-none absolute top-1/2 left-3 h-4 w-4 -translate-y-1/2 text-slate-400" />
            <Input
              aria-label={isZh ? "搜索模型、供应商或映射" : "Search models, providers, or mappings"}
              value={search}
              onChange={(event) => setSearch(event.target.value)}
              placeholder={isZh ? "搜索模型、供应商或映射" : "Search models, providers, or mappings"}
              className="pl-9"
            />
          </div>
          <Select value={providerFilter} onValueChange={setProviderFilter}>
            <SelectTrigger
              aria-label={isZh ? "按供应商筛选" : "Filter by provider"}
              className="w-full md:w-52"
            >
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={ALL_PROVIDERS}>{isZh ? "全部供应商" : "All providers"}</SelectItem>
              {providers.map((provider) => (
                <SelectItem key={provider.id} value={provider.id}>{provider.name}</SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Button
            variant="secondary"
            onClick={() => setShowAllModels((current) => !current)}
            className="w-full md:w-auto"
          >
            {showAllModels ? <ListFilter className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
            {showAllModels
              ? (isZh ? "仅显示已映射" : "Mapped only")
              : (isZh ? "显示全部" : "Show all")}
          </Button>
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                variant="secondary"
                size="icon"
                aria-label={isZh ? "刷新全部模型目录" : "Refresh all model catalogs"}
                disabled={isRefreshing || providers.every((provider) => !provider.is_enabled)}
                onClick={() => void refreshAll()}
              >
                <RefreshCw className={isRefreshing ? "h-4 w-4 animate-spin" : "h-4 w-4"} />
              </Button>
            </TooltipTrigger>
            <TooltipContent>{isZh ? "刷新全部模型目录" : "Refresh all model catalogs"}</TooltipContent>
          </Tooltip>
        </div>

        {providers.length === 0 ? (
          <div className="glass rounded-2xl p-12 text-center">
            <Boxes className="mx-auto h-10 w-10 text-slate-400" />
            <p className="mt-3 text-sm text-slate-500">{isZh ? "还没有配置供应商" : "No providers configured"}</p>
          </div>
        ) : filteredProviders.length === 0 ? (
          <div className="glass rounded-2xl p-12 text-center text-sm text-slate-500">
            {isZh ? "没有匹配的供应商" : "No matching providers"}
          </div>
        ) : (
          <div className="space-y-3">
            {filteredProviders.map((provider) => {
              const index = providers.findIndex((item) => item.id === provider.id);
              const query = catalogQueries[index];
              const models = provider.is_enabled ? (query.data ?? []) : [];
              return (
                <ProviderSection
                  key={provider.id}
                  provider={provider}
                  query={query}
                  models={models}
                  mappings={mappings}
                  probes={probes}
                  probeStore={probeStore}
                  isZh={isZh}
                  locale={locale}
                  search={search}
                  showAllModels={showAllModels}
                  collapsed={collapsedProviders.has(provider.id)}
                  probing={probingId === provider.id}
                  probeError={probeErrors[provider.id]}
                  onToggle={() => toggleProvider(provider.id)}
                  onProbe={() => void probeProvider(provider)}
                />
              );
            })}
          </div>
        )}
      </div>
    </TooltipProvider>
  );
}
