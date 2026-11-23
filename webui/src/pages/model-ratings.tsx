import { Fragment, useMemo, useState } from "react";
import { useQueries, useQuery } from "@tanstack/react-query";
import { ChevronDown, ChevronRight, ChevronLeft, CircleAlert, Link2, Pencil, Plus, RefreshCw, Search, Star, TriangleAlert } from "lucide-react";
import { backend } from "@/lib/backend";
import { formatLocalDateTime } from "@/lib/format";
import { useLocale } from "@/lib/i18n";
import { resolveEffectiveRating } from "@/lib/aa-rating-match";
import {
  buildModelRatingRows,
  buildUnmatchedModels,
  filterAndSortModelRatingRows,
  longestRatingMatch,
  parseRatingScore,
  ratingDisplayState,
  uniqueModelIdentifiers,
  type ModelCatalogSnapshot,
  type RatingFilter,
  type RatingSort,
} from "@/lib/model-ratings";
import { useAaRatingSync, useModelRatings } from "@/lib/use-model-ratings";
import type { AaModelRatingEntry, Model, Provider } from "@/lib/types";
import { ModelRatingBadge, ModelRatingClearedNotice, ModelRatingEditor, ModelRatingsFeedback, type ModelRatingEditTarget } from "@/components/model-rating";
import { AaAliasBindDialog, AaRatingSyncBar } from "@/components/aa-rating-sync";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";

const PAGE_SIZE = 40;

type ViewMode = "entries" | "aa" | "unmatched";

export default function ModelRatingsPage() {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const [view, setView] = useState<ViewMode>("entries");
  const [search, setSearch] = useState("");
  const [ratingFilter, setRatingFilter] = useState<RatingFilter>("all");
  const [minDraft, setMinDraft] = useState("");
  const [maxDraft, setMaxDraft] = useState("");
  const [sort, setSort] = useState<RatingSort>("score-desc");
  const [pagination, setPagination] = useState({ key: "", page: 1 });
  const [editing, setEditing] = useState<ModelRatingEditTarget | null>(null);
  const [bindingAlias, setBindingAlias] = useState<{ modelPrefix: string; currentTargetKey?: string | null } | null>(null);
  const [clearedRating, setClearedRating] = useState<ModelRatingEditTarget | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);
  const providersQuery = useQuery<Provider[]>({ queryKey: ["providers"], queryFn: () => backend("get_providers") });
  const routesQuery = useQuery<Model[]>({ queryKey: ["routes"], queryFn: () => backend("list_models") });
  const ratings = useModelRatings();
  const aaSync = useAaRatingSync();
  const providers = useMemo(() => [...(providersQuery.data ?? [])].sort((a, b) => (
    a.name.localeCompare(b.name, undefined, { sensitivity: "base" }) || a.id.localeCompare(b.id)
  )), [providersQuery.data]);
  const enabledProviders = providers.filter((provider) => provider.is_enabled);
  const catalogQueries = useQueries({
    queries: enabledProviders.map((provider) => ({
      queryKey: ["provider-model-catalog", provider.id],
      queryFn: () => backend<string[]>("get_provider_models", { id: provider.id, requireCatalog: true }),
      retry: false,
      staleTime: 60_000,
      refetchOnWindowFocus: false,
      select: uniqueModelIdentifiers,
    })),
  });
  const catalogs: ModelCatalogSnapshot[] = enabledProviders.map((provider, index) => {
    const query = catalogQueries[index];
    return {
      providerId: provider.id,
      status: query.isError || !providersQuery.isSuccess ? "unknown" : query.isSuccess ? "success" : "loading",
      models: query.data ?? [],
    };
  });
  const entries = ratings.data ?? [];
  const aaModels = aaSync.data?.models ?? [];
  const aaAliases = aaSync.data?.aliases ?? [];
  const aaSyncedAt = aaSync.data?.synced_at ?? null;
  const rows = buildModelRatingRows(entries, catalogs, providers);
  const rawUnmatched = routesQuery.isSuccess
    ? buildUnmatchedModels(providers, routesQuery.data ?? [], catalogs, entries)
    : [];
  // Models not covered by manual prefixes, AA aliases, or AA auto-matching.
  const unmatched = rawUnmatched.filter(
    (row) => !resolveEffectiveRating(row.model, entries, aaAliases, aaSync.index, aaSyncedAt),
  );
  const ratingsReady = ratings.loadState === "ready";
  const coverageReady = catalogs.length > 0 && catalogs.every((catalog) => catalog.status !== "loading");
  const catalogErrors = enabledProviders.filter((_provider, index) => catalogQueries[index].isError);
  const sourceIncomplete = providersQuery.isError || routesQuery.isError || catalogErrors.length > 0;
  const sourceLoading = providersQuery.isPending || routesQuery.isPending || catalogs.some((catalog) => catalog.status === "loading");
  const min = minDraft === "" ? null : parseRatingScore(minDraft);
  const max = maxDraft === "" ? null : parseRatingScore(maxDraft);
  const rangeInvalid = (minDraft !== "" && min === null) || (maxDraft !== "" && max === null)
    || (min !== null && max !== null && min > max);
  const filtered = filterAndSortModelRatingRows(rows, {
    search,
    rating: ratingFilter,
    min: rangeInvalid ? null : min,
    max: rangeInvalid ? null : max,
    sort,
    ratingsReady,
    coverageReady,
  });

  // AA auto-synced models sorted & filtered by search and score range.
  const aaQuery = search.trim().toLowerCase();
  const filteredAaModels = aaModels
    .filter((item) => {
      if (aaQuery && !(
        item.canonical_key.includes(aaQuery)
        || item.aa_name.toLowerCase().includes(aaQuery)
        || item.aa_creator.toLowerCase().includes(aaQuery)
      )) return false;
      if (!rangeInvalid && min !== null && item.score < min) return false;
      if (!rangeInvalid && max !== null && item.score > max) return false;
      return true;
    })
    .sort((a: AaModelRatingEntry, b: AaModelRatingEntry) => {
      if (sort === "score-asc") return a.raw_score - b.raw_score || a.canonical_key.localeCompare(b.canonical_key);
      if (sort === "name") return a.canonical_key.localeCompare(b.canonical_key);
      return b.raw_score - a.raw_score || a.canonical_key.localeCompare(b.canonical_key);
    });

  // Known models from catalogs/routes that resolve to each AA canonical_key.
  const aaCoverageMap = new Map<string, { provider: Provider; model: string }[]>();
  const providerIndex = new Map(providers.map((p) => [p.id, p]));
  const seenCoverage = new Set<string>();
  function recordAaCoverage(providerId: string, model: string) {
    const provider = providerIndex.get(providerId);
    if (!provider || !provider.is_enabled) return;
    const dedup = JSON.stringify([providerId, model]);
    if (seenCoverage.has(dedup)) return;
    seenCoverage.add(dedup);
    const eff = resolveEffectiveRating(model, entries, aaAliases, aaSync.index, aaSyncedAt);
    if (!eff || eff.source === "manual" || !eff.aa_entry) return;
    const key = eff.aa_entry.canonical_key;
    const list = aaCoverageMap.get(key) ?? [];
    list.push({ provider, model });
    aaCoverageMap.set(key, list);
  }
  for (const catalog of catalogs) {
    if (catalog.status === "success") {
      for (const model of catalog.models) recordAaCoverage(catalog.providerId, model);
    }
  }
  for (const route of routesQuery.data ?? []) {
    const targets = route.targets?.length
      ? route.targets
      : [{ provider_id: route.target_provider, model: route.target_model }];
    for (const target of targets) recordAaCoverage(target.provider_id, target.model);
  }

  const pageKey = JSON.stringify([view, search, ratingFilter, minDraft, maxDraft, sort, ratings.loadState]);
  const totalForView = view === "entries" ? filtered.length : view === "aa" ? filteredAaModels.length : unmatched.length;
  const pageCount = Math.max(1, Math.ceil(totalForView / PAGE_SIZE));
  const page = Math.min(pagination.key === pageKey ? pagination.page : 1, pageCount);
  const visibleEntries = view === "entries"
    ? filtered.slice((page - 1) * PAGE_SIZE, page * PAGE_SIZE)
    : [];
  const visibleAaModels = view === "aa"
    ? filteredAaModels.slice((page - 1) * PAGE_SIZE, page * PAGE_SIZE)
    : [];
  const visibleUnmatched = view === "unmatched"
    ? unmatched.slice((page - 1) * PAGE_SIZE, page * PAGE_SIZE)
    : [];
  const unmatchedSearch = search.trim().toLocaleLowerCase();
  const visibleUnmatchedFiltered = view === "unmatched" && unmatchedSearch
    ? visibleUnmatched.filter((row) => `${row.provider.name}\n${row.model}`.toLocaleLowerCase().includes(unmatchedSearch))
    : visibleUnmatched;
  const zeroHitCount = rows.filter((row) => row.modelCount === 0).length;
  const sourcesRefreshing = providersQuery.isFetching || routesQuery.isFetching || catalogQueries.some((query) => query.isFetching);

  async function refreshCatalogs() {
    await Promise.all([
      providersQuery.refetch(),
      routesQuery.refetch(),
      ...catalogQueries.map((query) => query.refetch()),
    ]);
  }

  return (
    <div className="space-y-5">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-end sm:justify-between">
        <div>
          <h1 className="text-2xl font-bold text-slate-900">{isZh ? "模型评分" : "Model Ratings"}</h1>
          <p className="mt-1 text-sm text-slate-500">{isZh ? "支持每日自动同步 Artificial Analysis Intelligence Index（取各模型最高推理档位固定分），同时保留手工前缀覆盖（手工优先）。评分不影响路由。" : "Supports daily Artificial Analysis Intelligence Index sync (highest reasoning effort tier per model) alongside manual prefix overrides (manual takes precedence). Ratings do not affect routing."}</p>
        </div>
        <div className="flex flex-wrap gap-2">
          <Button size="sm" onClick={() => setEditing({ initialPrefix: "", entry: null })}>
            <Plus className="h-3.5 w-3.5" />{isZh ? "新建前缀评分" : "New prefix rating"}
          </Button>
          <Button variant="secondary" size="sm" disabled={ratings.isFetching || aaSync.isFetching} onClick={() => { void ratings.refetch(); void aaSync.refetch(); }}>
            <RefreshCw className={(ratings.isFetching || aaSync.isFetching) ? "h-3.5 w-3.5 animate-spin" : "h-3.5 w-3.5"} />{isZh ? "刷新评分" : "Refresh ratings"}
          </Button>
          <Button variant="secondary" size="sm" disabled={sourcesRefreshing} onClick={() => void refreshCatalogs()}>
            <RefreshCw className={sourcesRefreshing ? "h-3.5 w-3.5 animate-spin" : "h-3.5 w-3.5"} />{isZh ? "刷新模型目录" : "Refresh catalogs"}
          </Button>
        </div>
      </div>

      <AaRatingSyncBar />

      {clearedRating && <ModelRatingClearedNotice target={clearedRating} onDismiss={() => setClearedRating(null)} />}
      <ModelRatingsFeedback state={ratings.loadState} error={ratings.error} fetching={ratings.isFetching} onRetry={() => void ratings.refetch()} />
      {sourceIncomplete && (
        <div role="status" className="rounded-xl border border-amber-200 bg-amber-50/70 px-4 py-3 text-sm text-amber-700">
          <div className="flex gap-2"><CircleAlert className="mt-0.5 h-4 w-4 shrink-0" /><p>{isZh ? "模型目录不完整：部分目录不可用。命中计数只基于已知目录，不代表模型缺失。" : "The model directory is incomplete: some catalogs are unavailable. Match counts reflect known catalogs only; an unknown catalog does not mean a model is missing."}</p></div>
          {providersQuery.isError && <p className="mt-1 text-xs">{isZh ? "供应商加载失败：" : "Providers could not be loaded: "}{String(providersQuery.error)}</p>}
          {routesQuery.isError && <p className="mt-1 text-xs">{isZh ? "模型映射加载失败：" : "Model mappings could not be loaded: "}{String(routesQuery.error)}</p>}
          {catalogErrors.length > 0 && <p className="mt-1 text-xs">{isZh ? "目录加载失败：" : "Catalog requests failed: "}{catalogErrors.map((provider) => provider.name).join(", ")}</p>}
        </div>
      )}

      <div className="glass space-y-3 rounded-2xl p-3">
        <div className="flex flex-col gap-3 md:flex-row">
          <div className="relative min-w-0 flex-1">
            <Search className="pointer-events-none absolute top-1/2 left-3 h-4 w-4 -translate-y-1/2 text-slate-400" />
            <Input value={search} onChange={(event) => setSearch(event.target.value)} aria-label={isZh ? "搜索前缀或模型" : "Search prefixes or models"} placeholder={isZh ? "搜索前缀或模型" : "Search prefixes or models"} className="pl-9" />
          </div>
          <div className="flex flex-wrap gap-1 rounded-xl bg-slate-100/80 p-1">
            {([
              ["entries", isZh ? `手工前缀 (${rows.length})` : `Manual (${rows.length})`],
              ["aa", isZh ? `AA 同步 (${aaModels.length})` : `AA Synced (${aaModels.length})`],
              ["unmatched", isZh ? `未命中模型 (${unmatched.length})` : `Unmatched (${unmatched.length})`],
            ] as const).map(([mode, label]) => (
              <button
                key={mode}
                type="button"
                onClick={() => setView(mode)}
                className={`rounded-lg px-3 py-1.5 text-sm transition-colors ${view === mode ? "bg-white font-medium text-slate-900 shadow-sm" : "text-slate-500 hover:text-slate-700"}`}
              >
                {label}
              </button>
            ))}
          </div>
        </div>
        {(view === "entries" || view === "aa") && (
          <div className="flex flex-wrap items-end gap-3">
            {view === "entries" && (
              <label className="space-y-1 text-xs text-slate-500">
                <span>{isZh ? "命中状态" : "Match state"}</span>
                <Select value={ratingFilter} onValueChange={(value) => setRatingFilter(value as RatingFilter)} disabled={!ratingsReady || !coverageReady}>
                  <SelectTrigger className="w-36" aria-label={isZh ? "命中状态" : "Match state"}><SelectValue /></SelectTrigger>
                  <SelectContent>
                    <SelectItem value="all">{isZh ? "全部" : "All"}</SelectItem>
                    <SelectItem value="matched">{isZh ? "已命中模型" : "Matched models"}</SelectItem>
                    <SelectItem value="unmatched">{isZh ? "零命中" : "Zero matches"}</SelectItem>
                  </SelectContent>
                </Select>
              </label>
            )}
            <label className="space-y-1 text-xs text-slate-500"><span>{isZh ? "最低分" : "Min score"}</span><Input type="text" inputMode="numeric" value={minDraft} onChange={(event) => setMinDraft(event.target.value)} disabled={!ratingsReady} aria-invalid={rangeInvalid} placeholder="0" className="w-24" /></label>
            <label className="space-y-1 text-xs text-slate-500"><span>{isZh ? "最高分" : "Max score"}</span><Input type="text" inputMode="numeric" value={maxDraft} onChange={(event) => setMaxDraft(event.target.value)} disabled={!ratingsReady} aria-invalid={rangeInvalid} placeholder="100" className="w-24" /></label>
            <label className="space-y-1 text-xs text-slate-500 md:ml-auto">
              <span>{isZh ? "排序" : "Sort"}</span>
              <Select value={sort} onValueChange={(value) => setSort(value as RatingSort)}>
                <SelectTrigger className="w-48" aria-label={isZh ? "排序" : "Sort"}><SelectValue /></SelectTrigger>
                <SelectContent>
                  <SelectItem value="score-desc" disabled={!ratingsReady}>{isZh ? "评分：从高到低" : "Score: high to low"}</SelectItem>
                  <SelectItem value="score-asc" disabled={!ratingsReady}>{isZh ? "评分：从低到高" : "Score: low to high"}</SelectItem>
                  <SelectItem value="name">{isZh ? "前缀名称" : "Prefix name"}</SelectItem>
                  {view === "entries" && <SelectItem value="updated" disabled={!ratingsReady}>{isZh ? "最近评分更新" : "Recently rated"}</SelectItem>}
                </SelectContent>
              </Select>
            </label>
          </div>
        )}
        {(view === "entries" || view === "aa") && rangeInvalid && ratingsReady && <p role="alert" className="text-xs text-red-600">{isZh ? "评分范围须为 0–100 的整数，且最低分不能高于最高分。当前未应用范围筛选。" : "Score bounds must be whole numbers from 0 to 100, with min ≤ max. Range filters are not applied."}</p>}
        {view === "entries" && <p className="text-xs text-slate-500">{isZh ? `手工条目：${rows.length} · 零命中：${zeroHitCount} · 匹配：${filtered.length}` : `Manual entries: ${rows.length} · Zero matches: ${zeroHitCount} · Matching: ${filtered.length}`}</p>}
        {view === "aa" && <p className="text-xs text-slate-500">{isZh ? `AA 基础模型：${aaModels.length} · 自定义别名：${aaAliases.length} · 当前显示：${filteredAaModels.length}` : `AA base models: ${aaModels.length} · Custom aliases: ${aaAliases.length} · Matching: ${filteredAaModels.length}`}</p>}
        {view === "unmatched" && <p className="text-xs text-slate-500">{isZh ? "以下已知模型未被手工前缀、AA 别名或 AA 自动匹配覆盖。可手工打分或绑定到已有 AA 模型。" : "Known models not covered by manual prefixes, AA aliases, or AA auto-matching. Rate manually or bind to an AA model."}</p>}
      </div>

      <div className="glass overflow-hidden rounded-2xl">
        {view === "entries" ? (
          <div className="overflow-x-auto">
            <table className="w-full min-w-[760px] text-left text-sm">
              <thead className="bg-slate-50/65 text-xs text-slate-400">
                <tr><th className="px-4 py-3 font-medium">{isZh ? "模型前缀" : "Model prefix"}</th><th className="px-4 py-3 font-medium">{isZh ? "评分" : "Score"}</th><th className="px-4 py-3 font-medium">{isZh ? "评分更新时间" : "Rating updated"}</th><th className="px-4 py-3 font-medium">{isZh ? "命中" : "Matches"}</th><th className="px-4 py-3 text-right font-medium">{isZh ? "操作" : "Action"}</th></tr>
              </thead>
              <tbody>
                {visibleEntries.map((row) => (
                  <Fragment key={row.key}>
                    <tr className="border-t border-slate-200/80">
                      <td className="max-w-96 break-all px-4 py-3 font-mono text-[13px] font-medium text-slate-800">{row.entry.model_prefix}</td>
                      <td className="whitespace-nowrap px-4 py-3"><ModelRatingBadge state={{ status: "rated", entry: row.entry }} source="manual" /></td>
                      <td className="whitespace-nowrap px-4 py-3 text-xs text-slate-500"><time dateTime={row.entry.updated_at}>{formatLocalDateTime(row.entry.updated_at)}</time></td>
                      <td className="px-4 py-3">
                        <button type="button" onClick={() => setExpanded(expanded === row.key ? null : row.key)} className="flex items-center gap-1.5 text-sm text-slate-600 hover:text-slate-900" aria-expanded={expanded === row.key}>
                          {expanded === row.key ? <ChevronDown className="h-3.5 w-3.5" /> : <ChevronRight className="h-3.5 w-3.5" />}
                          {row.modelCount > 0
                            ? (isZh ? `${row.modelCount} 个模型 / ${row.providerCount} 个供应商` : `${row.modelCount} models / ${row.providerCount} providers`)
                            : <span className="flex items-center gap-1 text-amber-600"><TriangleAlert className="h-3.5 w-3.5" />{isZh ? "0 命中" : "0 matches"}</span>}
                        </button>
                      </td>
                      <td className="px-4 py-3 text-right"><Button variant="ghost" size="sm" disabled={!ratingsReady} aria-label={isZh ? `编辑 ${row.entry.model_prefix} 的评分` : `Edit rating for ${row.entry.model_prefix}`} onClick={() => setEditing({ initialPrefix: row.entry.model_prefix, entry: row.entry })}><Pencil className="h-3.5 w-3.5" />{isZh ? "编辑" : "Edit"}</Button></td>
                    </tr>
                    {expanded === row.key && (
                      <tr className="border-t border-slate-100 bg-slate-50/60">
                        <td colSpan={5} className="px-4 py-3">
                          {row.coverage.length === 0 ? (
                            <p className="text-xs text-slate-500">{isZh ? "当前没有已知模型命中此前缀。" : "No known model matches this prefix yet."}</p>
                          ) : (
                            <div className="space-y-2">
                              {row.coverage.map((group) => (
                                <div key={group.provider.id} className="text-xs">
                                  <span className="font-medium text-slate-700">{group.provider.name}</span>
                                  <div className="mt-1 flex flex-wrap gap-1.5">
                                    {group.models.map((model) => <Badge key={model} variant="outline" className="max-w-full break-all font-mono text-[11px]">{model}</Badge>)}
                                  </div>
                                </div>
                              ))}
                            </div>
                          )}
                        </td>
                      </tr>
                    )}
                  </Fragment>
                ))}
              </tbody>
            </table>
          </div>
        ) : view === "aa" ? (
          <div className="overflow-x-auto">
            <table className="w-full min-w-[820px] text-left text-sm">
              <thead className="bg-slate-50/65 text-xs text-slate-400">
                <tr>
                  <th className="px-4 py-3 font-medium">{isZh ? "基础模型键" : "Canonical key"}</th>
                  <th className="px-4 py-3 font-medium">{isZh ? "AA 最高推理档位" : "AA highest tier"}</th>
                  <th className="px-4 py-3 font-medium">{isZh ? "AA 分数" : "AA score"}</th>
                  <th className="px-4 py-3 font-medium">{isZh ? "已自动命中" : "Auto-matched"}</th>
                  <th className="px-4 py-3 text-right font-medium">{isZh ? "操作" : "Action"}</th>
                </tr>
              </thead>
              <tbody>
                {visibleAaModels.map((item) => {
                  const hits = aaCoverageMap.get(item.canonical_key) ?? [];
                  const manualOverride = longestRatingMatch(item.canonical_key, entries);
                  return (
                    <tr key={item.canonical_key} className="border-t border-slate-200/80">
                      <td className="px-4 py-3">
                        <p className="font-mono text-[13px] font-medium text-slate-800">{item.canonical_key}</p>
                        <p className="text-xs text-slate-400">{item.aa_creator}</p>
                      </td>
                      <td className="px-4 py-3 text-xs text-slate-600">
                        <p className="font-medium text-slate-700">{item.aa_name}</p>
                        <p className="mt-0.5 text-slate-400">{isZh ? `档位：${item.selected_effort}` : `Effort: ${item.selected_effort}`}</p>
                      </td>
                      <td className="whitespace-nowrap px-4 py-3">
                        <div className="flex items-center gap-1.5">
                          <ModelRatingBadge
                            state={{
                              status: "rated",
                              entry: {
                                model_prefix: item.canonical_key,
                                score: item.score,
                                updated_at: aaSyncedAt ?? "",
                              },
                            }}
                            source="aa_auto"
                            detailTitle={`${item.aa_name} · ${item.raw_score.toFixed(1)}`}
                          />
                          <span className="text-xs tabular-nums text-slate-400">({item.raw_score.toFixed(1)})</span>
                        </div>
                      </td>
                      <td className="px-4 py-3 text-xs text-slate-600">
                        {manualOverride ? (
                          <Badge variant="secondary" className="text-[11px]">
                            {isZh ? `手工覆盖：${manualOverride.score}` : `Manual override: ${manualOverride.score}`}
                          </Badge>
                        ) : hits.length > 0 ? (
                          <div className="flex flex-wrap gap-1">
                            {hits.slice(0, 4).map((h) => (
                              <Badge key={`${h.provider.id}:${h.model}`} variant="outline" className="font-mono text-[11px]">
                                {h.model}
                              </Badge>
                            ))}
                            {hits.length > 4 && <span className="text-slate-400">+{hits.length - 4}</span>}
                          </div>
                        ) : (
                          <span className="text-slate-400">—</span>
                        )}
                      </td>
                      <td className="px-4 py-3 text-right">
                        <Button
                          variant="ghost"
                          size="sm"
                          disabled={!ratingsReady}
                          onClick={() => setEditing({ initialPrefix: item.canonical_key, entry: manualOverride })}
                        >
                          <Pencil className="h-3.5 w-3.5" />
                          {manualOverride ? (isZh ? "编辑覆盖" : "Edit override") : (isZh ? "手工覆盖" : "Override")}
                        </Button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        ) : (
          <div className="divide-y divide-slate-200/80">
            {visibleUnmatchedFiltered.map((row) => (
              <div key={row.key} className="flex flex-wrap items-center justify-between gap-2 px-4 py-2.5">
                <div className="min-w-0">
                  <p className="break-all font-mono text-[13px] text-slate-700">{row.model}</p>
                  <p className="mt-0.5 text-xs text-slate-400">{row.provider.name}</p>
                </div>
                <div className="flex items-center gap-2">
                  <ModelRatingBadge state={ratingDisplayState(ratings.loadState, row.model, entries)} />
                  {aaModels.length > 0 && (
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() => setBindingAlias({ modelPrefix: row.model })}
                    >
                      <Link2 className="h-3.5 w-3.5" />{isZh ? "关联 AA" : "Bind AA"}
                    </Button>
                  )}
                  <Button variant="ghost" size="sm" disabled={!ratingsReady} onClick={() => setEditing({ initialPrefix: row.model, entry: longestRatingMatch(row.model, entries) })}>
                    <Pencil className="h-3.5 w-3.5" />{isZh ? "手工评分" : "Rate"}
                  </Button>
                </div>
              </div>
            ))}
          </div>
        )}
        {(view === "entries" ? visibleEntries.length === 0 : view === "aa" ? visibleAaModels.length === 0 : visibleUnmatchedFiltered.length === 0) && (
          <div className="px-4 py-12 text-center text-sm text-slate-500">
            <Star className="mx-auto mb-3 h-8 w-8 text-slate-300" />
            {sourceLoading || ratings.loadState === "loading"
              ? (isZh ? "正在加载模型与评分…" : "Loading models and ratings…")
              : !ratingsReady
                ? (isZh ? "评分不可用，无法确认完整列表。请重试。" : "Ratings unavailable; the full list cannot be determined. Please retry.")
                : view === "unmatched"
                  ? (isZh ? "没有未命中的模型：所有已知模型均已获得手工评分或 AA 自动评分。" : "No unmatched models: every known model has a manual or AA rating.")
                  : view === "aa"
                    ? (isZh ? "暂无已同步的 AA 模型。点击上方“同步设置”配置 API Key 并立即同步。" : "No AA models synced yet. Open Sync settings above to configure your API key and sync.")
                    : (isZh ? "没有匹配的手工前缀条目。点击右上角新建。" : "No manual prefix entries match. Create one with the button above.")}
          </div>
        )}
        {totalForView > 0 && (
          <div className="flex items-center justify-between gap-3 border-t border-slate-200/80 px-4 py-3 text-xs text-slate-500">
            <span>{isZh ? `第 ${page} / ${pageCount} 页` : `Page ${page} of ${pageCount}`}</span>
            <div className="flex gap-2">
              <Button variant="secondary" size="sm" disabled={page <= 1} onClick={() => setPagination({ key: pageKey, page: page - 1 })}><ChevronLeft className="h-3.5 w-3.5" />{isZh ? "上一页" : "Previous"}</Button>
              <Button variant="secondary" size="sm" disabled={page >= pageCount} onClick={() => setPagination({ key: pageKey, page: page + 1 })}>{isZh ? "下一页" : "Next"}<ChevronRight className="h-3.5 w-3.5" /></Button>
            </div>
          </div>
        )}
      </div>
      {editing && (
        <ModelRatingEditor
          target={editing}
          providers={providers}
          catalogs={catalogs}
          onClose={() => setEditing(null)}
          onCleared={setClearedRating}
        />
      )}
      {bindingAlias && (
        <AaAliasBindDialog
          modelPrefix={bindingAlias.modelPrefix}
          models={aaModels}
          currentTargetKey={bindingAlias.currentTargetKey}
          onClose={() => setBindingAlias(null)}
        />
      )}
    </div>
  );
}
