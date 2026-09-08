import { useMemo, useState } from "react";
import { useQueries, useQuery } from "@tanstack/react-query";
import { ChevronLeft, ChevronRight, CircleAlert, Loader2, Pencil, RefreshCw, Search, Star } from "lucide-react";
import { backend } from "@/lib/backend";
import { formatLocalDateTime } from "@/lib/format";
import { useLocale } from "@/lib/i18n";
import { buildModelRatingRows, filterAndSortModelRatingRows, parseRatingScore, ratingDisplayState, uniqueModelIdentifiers, type ModelCatalogSnapshot, type RatingFilter, type RatingSort } from "@/lib/model-ratings";
import { useModelRatings } from "@/lib/use-model-ratings";
import type { Model, Provider } from "@/lib/types";
import { ModelRatingBadge, ModelRatingClearedNotice, ModelRatingEditor, ModelRatingsFeedback, type ModelRatingEditTarget } from "@/components/model-rating";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";

const PAGE_SIZE = 40;
const ALL_PROVIDERS = "all";

export default function ModelRatingsPage() {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const [search, setSearch] = useState("");
  const [providerFilter, setProviderFilter] = useState(ALL_PROVIDERS);
  const [ratingFilter, setRatingFilter] = useState<RatingFilter>("all");
  const [minDraft, setMinDraft] = useState("");
  const [maxDraft, setMaxDraft] = useState("");
  const [sort, setSort] = useState<RatingSort>("score-desc");
  const [pagination, setPagination] = useState({ key: "", page: 1 });
  const [editing, setEditing] = useState<ModelRatingEditTarget | null>(null);
  const [clearedRating, setClearedRating] = useState<ModelRatingEditTarget | null>(null);
  const providersQuery = useQuery<Provider[]>({ queryKey: ["providers"], queryFn: () => backend("get_providers") });
  const routesQuery = useQuery<Model[]>({ queryKey: ["routes"], queryFn: () => backend("list_models") });
  const ratings = useModelRatings();
  const providers = useMemo(() => [...(providersQuery.data ?? [])].sort((a, b) => (
    a.name.localeCompare(b.name, undefined, { sensitivity: "base" }) || a.id.localeCompare(b.id)
  )), [providersQuery.data]);
  const catalogQueries = useQueries({
    queries: providers.map((provider) => ({
      queryKey: ["provider-model-catalog", provider.id],
      queryFn: () => backend<string[]>("get_provider_models", { id: provider.id, requireCatalog: true }),
      enabled: provider.is_enabled,
      retry: false,
      staleTime: 60_000,
      refetchOnWindowFocus: false,
      select: uniqueModelIdentifiers,
    })),
  });
  const catalogs: ModelCatalogSnapshot[] = providers.map((provider, index) => {
    const query = catalogQueries[index];
    return {
      providerId: provider.id,
      status: !provider.is_enabled || query.isError || !providersQuery.isSuccess
        ? "unknown" : query.isSuccess ? "success" : "loading",
      models: query.data ?? [],
    };
  });
  const rows = buildModelRatingRows(providers, routesQuery.data ?? [], ratings.data ?? [], catalogs);
  const ratingsReady = ratings.loadState === "ready";
  const min = minDraft === "" ? null : parseRatingScore(minDraft);
  const max = maxDraft === "" ? null : parseRatingScore(maxDraft);
  const rangeInvalid = (minDraft !== "" && min === null) || (maxDraft !== "" && max === null)
    || (min !== null && max !== null && min > max);
  const filtered = filterAndSortModelRatingRows(rows, {
    search,
    providerId: providerFilter === ALL_PROVIDERS ? null : providerFilter.slice("provider:".length),
    rating: ratingFilter,
    min: rangeInvalid ? null : min,
    max: rangeInvalid ? null : max,
    sort,
    ratingsReady,
  });
  const pageKey = JSON.stringify([search, providerFilter, ratingFilter, minDraft, maxDraft, sort, ratings.loadState]);
  const pageCount = Math.max(1, Math.ceil(filtered.length / PAGE_SIZE));
  const page = Math.min(pagination.key === pageKey ? pagination.page : 1, pageCount);
  const visible = filtered.slice((page - 1) * PAGE_SIZE, page * PAGE_SIZE);
  const ratingCount = rows.filter((row) => row.rating !== null).length;
  const unratedCount = rows.filter((row) => row.rating === null && row.provider).length;
  const sourceLoading = providersQuery.isPending || routesQuery.isPending || catalogs.some((catalog) => catalog.status === "loading");
  const catalogErrors = providers.filter((provider, index) => provider.is_enabled && catalogQueries[index].isError);
  const disabledCount = providers.filter((provider) => !provider.is_enabled).length;
  const sourceIncomplete = providersQuery.isError || routesQuery.isError || catalogErrors.length > 0 || disabledCount > 0;
  const sourcesRefreshing = providersQuery.isFetching || routesQuery.isFetching || catalogQueries.some((query) => query.isFetching);
  const providerOptions = [...providers.map((provider) => ({ id: provider.id, name: provider.name }))];
  const knownProviderIds = new Set(providers.map((provider) => provider.id));
  for (const row of rows) {
    if (!knownProviderIds.has(row.providerId)) {
      knownProviderIds.add(row.providerId);
      providerOptions.push({ id: row.providerId, name: row.providerId });
    }
  }

  async function refreshCatalogs() {
    await Promise.all([
      providersQuery.refetch(),
      routesQuery.refetch(),
      ...catalogQueries.map((query, index) => providers[index].is_enabled ? query.refetch() : Promise.resolve()),
    ]);
  }

  return (
    <div className="space-y-5">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-end sm:justify-between">
        <div>
          <h1 className="text-2xl font-bold text-slate-900">{isZh ? "模型评分" : "Model Ratings"}</h1>
          <p className="mt-1 text-sm text-slate-500">{isZh ? "按供应商与确切模型管理 0–100 综合评分，不影响路由。" : "Comprehensive 0–100 scores per exact provider/model pair. Ratings do not affect routing."}</p>
        </div>
        <div className="flex flex-wrap gap-2">
          <Button variant="secondary" size="sm" disabled={ratings.isFetching} onClick={() => void ratings.refetch()}>
            <RefreshCw className={ratings.isFetching ? "h-3.5 w-3.5 animate-spin" : "h-3.5 w-3.5"} />{isZh ? "刷新评分" : "Refresh ratings"}
          </Button>
          <Button variant="secondary" size="sm" disabled={sourcesRefreshing} onClick={() => void refreshCatalogs()}>
            <RefreshCw className={sourcesRefreshing ? "h-3.5 w-3.5 animate-spin" : "h-3.5 w-3.5"} />{isZh ? "刷新模型目录" : "Refresh catalogs"}
          </Button>
        </div>
      </div>

      {clearedRating && <ModelRatingClearedNotice target={clearedRating} onDismiss={() => setClearedRating(null)} />}
      <ModelRatingsFeedback state={ratings.loadState} error={ratings.error} fetching={ratings.isFetching} onRetry={() => void ratings.refetch()} />
      {sourceIncomplete && (
        <div role="status" className="rounded-xl border border-amber-200 bg-amber-50/70 px-4 py-3 text-sm text-amber-700">
          <div className="flex gap-2"><CircleAlert className="mt-0.5 h-4 w-4 shrink-0" /><p>{isZh ? "模型目录不完整：部分目录不可用或供应商已禁用。仍显示已知的映射与已保存评分；目录未知不代表模型缺失。" : "The model directory is incomplete: some catalogs are unavailable or providers are disabled. Known route references and saved ratings remain visible; an unknown catalog does not mean a model is missing."}</p></div>
          {providersQuery.isError && <p className="mt-1 text-xs">{isZh ? "供应商加载失败：" : "Providers could not be loaded: "}{String(providersQuery.error)}</p>}
          {routesQuery.isError && <p className="mt-1 text-xs">{isZh ? "模型映射加载失败：" : "Model mappings could not be loaded: "}{String(routesQuery.error)}</p>}
          {catalogErrors.length > 0 && <p className="mt-1 text-xs">{isZh ? "目录加载失败：" : "Catalog requests failed: "}{catalogErrors.map((provider) => provider.name).join(", ")}</p>}
          {disabledCount > 0 && <p className="mt-1 text-xs">{isZh ? `${disabledCount} 个已禁用供应商的目录未查询。` : `Catalogs were not queried for ${disabledCount} disabled providers.`}</p>}
        </div>
      )}
      {sourceLoading && <p role="status" className="flex items-center gap-2 text-xs text-slate-500"><Loader2 className="h-3.5 w-3.5 animate-spin" />{isZh ? "模型目录和映射加载中，当前列表可能不完整。" : "Loading model catalogs and mappings; the current list may be incomplete."}</p>}

      <div className="glass space-y-3 rounded-2xl p-3">
        <div className="flex flex-col gap-3 md:flex-row">
          <div className="relative min-w-0 flex-1">
            <Search className="pointer-events-none absolute top-1/2 left-3 h-4 w-4 -translate-y-1/2 text-slate-400" />
            <Input value={search} onChange={(event) => setSearch(event.target.value)} aria-label={isZh ? "搜索模型或供应商" : "Search models or providers"} placeholder={isZh ? "搜索模型或供应商" : "Search models or providers"} className="pl-9" />
          </div>
          <Select value={providerFilter} onValueChange={setProviderFilter}>
            <SelectTrigger className="w-full md:w-56" aria-label={isZh ? "按供应商筛选" : "Filter by provider"}><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectItem value={ALL_PROVIDERS}>{isZh ? "全部供应商" : "All providers"}</SelectItem>
              {providerOptions.map((provider) => <SelectItem key={provider.id} value={`provider:${provider.id}`}>{provider.name}</SelectItem>)}
            </SelectContent>
          </Select>
        </div>
        <div className="flex flex-wrap items-end gap-3">
          <label className="space-y-1 text-xs text-slate-500">
            <span>{isZh ? "评分状态" : "Rating state"}</span>
            <Select value={ratingFilter} onValueChange={(value) => setRatingFilter(value as RatingFilter)} disabled={!ratingsReady}>
              <SelectTrigger className="w-36" aria-label={isZh ? "评分状态" : "Rating state"}><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value="all">{isZh ? "全部" : "All"}</SelectItem>
                <SelectItem value="rated">{isZh ? "已评分" : "Rated"}</SelectItem>
                <SelectItem value="unrated">{isZh ? "未评分" : "Unrated"}</SelectItem>
              </SelectContent>
            </Select>
          </label>
          <label className="space-y-1 text-xs text-slate-500"><span>{isZh ? "最低分" : "Min score"}</span><Input type="text" inputMode="numeric" value={minDraft} onChange={(event) => setMinDraft(event.target.value)} disabled={!ratingsReady} aria-invalid={rangeInvalid} placeholder="0" className="w-24" /></label>
          <label className="space-y-1 text-xs text-slate-500"><span>{isZh ? "最高分" : "Max score"}</span><Input type="text" inputMode="numeric" value={maxDraft} onChange={(event) => setMaxDraft(event.target.value)} disabled={!ratingsReady} aria-invalid={rangeInvalid} placeholder="100" className="w-24" /></label>
          <label className="space-y-1 text-xs text-slate-500 md:ml-auto">
            <span>{isZh ? "排序" : "Sort"}</span>
            <Select value={sort} onValueChange={(value) => setSort(value as RatingSort)}>
              <SelectTrigger className="w-48" aria-label={isZh ? "排序" : "Sort"}><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value="score-desc" disabled={!ratingsReady}>{isZh ? "评分：从高到低" : "Score: high to low"}</SelectItem>
                <SelectItem value="score-asc" disabled={!ratingsReady}>{isZh ? "评分：从低到高" : "Score: low to high"}</SelectItem>
                <SelectItem value="name">{isZh ? "模型名称" : "Model name"}</SelectItem>
                <SelectItem value="updated" disabled={!ratingsReady}>{isZh ? "最近评分更新" : "Recently rated"}</SelectItem>
              </SelectContent>
            </Select>
          </label>
        </div>
        {rangeInvalid && ratingsReady && <p role="alert" className="text-xs text-red-600">{isZh ? "评分范围须为 0–100 的整数，且最低分不能高于最高分。当前未应用范围筛选。" : "Score bounds must be whole numbers from 0 to 100, with min ≤ max. Range filters are not applied."}</p>}
        {!ratingsReady ? <p className="text-xs text-slate-500">{isZh ? "评分未知，评分筛选、排序与计数暂不可用；当前仅按文本和供应商筛选。" : "Rating filters, score sorting, and counts are unavailable until ratings load. Only text and provider filters are applied."}</p> : <p className="text-xs text-slate-500">{isZh ? "设置分数范围时仅包含已评分模型；未评分不是 0 分。" : "Score ranges include rated models only; unrated is not a score of 0."}</p>}
      </div>

      <div className="glass overflow-hidden rounded-2xl">
        {ratingsReady && <div className="border-b border-slate-200/80 px-4 py-3 text-xs text-slate-500">{isZh ? `已知模型：${rows.length} · 已评分：${ratingCount} · 未评分：${unratedCount} · 筛选结果：${filtered.length}` : `Known model pairs: ${rows.length} · Rated: ${ratingCount} · Unrated: ${unratedCount} · Matching: ${filtered.length}`}</div>}
        <div className="overflow-x-auto">
          <table className="w-full min-w-[850px] text-left text-sm">
            <thead className="bg-slate-50/65 text-xs text-slate-400">
              <tr><th className="px-4 py-3 font-medium">{isZh ? "供应商" : "Provider"}</th><th className="px-4 py-3 font-medium">{isZh ? "模型" : "Model"}</th><th className="px-4 py-3 font-medium">{isZh ? "评分" : "Score"}</th><th className="px-4 py-3 font-medium">{isZh ? "评分更新时间" : "Rating updated"}</th><th className="px-4 py-3 font-medium">{isZh ? "目录 / 供应商状态" : "Catalog / provider status"}</th><th className="px-4 py-3 text-right font-medium">{isZh ? "操作" : "Action"}</th></tr>
            </thead>
            <tbody>
              {visible.map((row) => (
                <tr key={row.key} className="border-t border-slate-200/80">
                  <td className="max-w-52 px-4 py-3"><p className="break-words font-medium text-slate-800">{row.provider?.name ?? row.providerId}</p><p className="mt-1 break-all text-[11px] text-slate-400">{row.providerId}</p></td>
                  <td className="max-w-80 whitespace-pre-wrap break-all px-4 py-3 font-mono text-[13px] text-slate-700">{row.model}</td>
                  <td className="whitespace-nowrap px-4 py-3"><ModelRatingBadge state={ratingDisplayState(ratings.loadState, row.rating, Boolean(row.provider))} /></td>
                  <td className="whitespace-nowrap px-4 py-3 text-xs text-slate-500">{ratingsReady && row.rating ? <time dateTime={row.rating.updated_at}>{formatLocalDateTime(row.rating.updated_at)}</time> : ratingsReady && row.provider ? "–" : isZh ? "未知" : "Unknown"}</td>
                  <td className="px-4 py-3"><div className="flex flex-wrap gap-1.5">
                    <Badge variant={row.catalogStatus === "listed" ? "success" : row.catalogStatus === "missing" ? "warning" : "outline"}>{row.catalogStatus === "listed" ? (isZh ? "目录内" : "Listed") : row.catalogStatus === "missing" ? (isZh ? "目录缺失" : "Not in catalog") : (isZh ? "目录未知" : "Catalog unknown")}</Badge>
                    <Badge variant={!row.provider ? "warning" : row.provider.is_enabled ? "success" : "secondary"}>{!row.provider ? (isZh ? "供应商未知" : "Provider unknown") : row.provider.is_enabled ? (isZh ? "已启用" : "Enabled") : (isZh ? "已禁用" : "Disabled")}</Badge>
                  </div></td>
                  <td className="px-4 py-3 text-right"><Button variant="ghost" size="sm" disabled={!ratingsReady || !row.provider} aria-label={isZh ? `编辑 ${row.provider?.name ?? row.providerId} / ${row.model} 的评分` : `Edit rating for ${row.provider?.name ?? row.providerId} / ${row.model}`} onClick={() => setEditing({ providerId: row.providerId, providerName: row.provider?.name ?? row.providerId, model: row.model, rating: row.rating })}><Pencil className="h-3.5 w-3.5" />{isZh ? "编辑" : "Edit"}</Button></td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        {visible.length === 0 && <div className="px-4 py-12 text-center text-sm text-slate-500"><Star className="mx-auto mb-3 h-8 w-8 text-slate-300" />{sourceLoading || ratings.loadState === "loading" ? (isZh ? "正在加载模型与评分…" : "Loading models and ratings…") : !ratingsReady ? (isZh ? "评分不可用，无法确认完整列表。请重试。" : "Ratings unavailable; the full list cannot be determined. Please retry.") : sourceIncomplete ? (isZh ? "当前已知数据中没有匹配模型，目录可能不完整。" : "No matching models in known data; catalogs may be incomplete.") : (isZh ? "没有匹配的模型" : "No matching models")}</div>}
        {filtered.length > 0 && <div className="flex items-center justify-between gap-3 border-t border-slate-200/80 px-4 py-3 text-xs text-slate-500"><span>{isZh ? `第 ${page} / ${pageCount} 页` : `Page ${page} of ${pageCount}`}</span><div className="flex gap-2"><Button variant="secondary" size="sm" disabled={page <= 1} onClick={() => setPagination({ key: pageKey, page: page - 1 })}><ChevronLeft className="h-3.5 w-3.5" />{isZh ? "上一页" : "Previous"}</Button><Button variant="secondary" size="sm" disabled={page >= pageCount} onClick={() => setPagination({ key: pageKey, page: page + 1 })}>{isZh ? "下一页" : "Next"}<ChevronRight className="h-3.5 w-3.5" /></Button></div></div>}
      </div>
      {editing && <ModelRatingEditor target={editing} onClose={() => setEditing(null)} onCleared={setClearedRating} />}
    </div>
  );
}
