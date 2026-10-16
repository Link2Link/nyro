import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Activity, ArrowDownLeft, ArrowUpRight, CalendarDays, ChevronLeft, ChevronRight, Coins, Info, RefreshCw, ShieldCheck, TrendingUp, Zap } from "lucide-react";
import { backend } from "@/lib/backend";
import { formatTokenCount } from "@/lib/format";
import { useLocale } from "@/lib/i18n";
import { compactUsageCost, formatUsageCost, groupTokenModels, sumTokenUsage, tokenCalendar, tokenDays, tokenHeat, tokenModelKey, USAGE_DISPLAY_CURRENCIES, USD_CNY_RATE, type TokenUsageTotals, type UsageCostTotals } from "@/lib/token-usage";
import type { TokenUsageStats } from "@/lib/types";
import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { TokenPriceDialog, type TokenPriceTarget } from "@/components/token-price-dialog";
import { TokenModelCosts } from "@/components/token-model-costs";
import "./token-usage-panel.css";

const ALL_MODELS = "__all_token_models__";
const EMPTY_ROWS: TokenUsageStats["rows"] = [];
const EMPTY_TOTAL = sumTokenUsage([]);
const VIEW_KEY = "nyro-usage-metric";
type Metric = "tokens" | "cost";
type Totals = ReturnType<typeof sumTokenUsage>;
type CostField = "total_cost" | "input_cost" | "cache_read_cost" | "output_cost";

export function TokenUsagePanel() {
  const { locale } = useLocale();
  const zh = locale === "zh-CN";
  const today = new Date().toISOString().slice(0, 10);
  const [period, setPeriod] = useState(() => today.slice(0, 7));
  const [mode, setMode] = useState("calendar");
  const [metric, setMetric] = useState<Metric>(() => {
    try { return localStorage.getItem(VIEW_KEY) === "cost" ? "cost" : "tokens"; } catch { return "tokens"; }
  });
  const [currency, setCurrency] = useState("USD");
  const [model, setModel] = useState(ALL_MODELS);
  const [selectedDay, setSelectedDay] = useState<string | null>(null);
  const [priceTarget, setPriceTarget] = useState<TokenPriceTarget | null>(null);
  const [priceNotice, setPriceNotice] = useState<"saved" | "reset" | null>(null);
  const isCost = metric === "cost";
  const year = Number(period.slice(0, 4));
  const month = Number(period.slice(5, 7)) - 1;
  const usage = useQuery<TokenUsageStats>({
    queryKey: ["token-usage", year],
    queryFn: ({ signal }) => backend("get_token_usage_stats", { year }, { signal }),
    staleTime: 30_000,
    refetchInterval: 60_000,
  });
  const allRows = usage.data?.rows ?? EMPTY_ROWS;
  const models = useMemo(() => groupTokenModels(allRows), [allRows]);
  const activeModel = models.some((item) => item.key === model) ? model : ALL_MODELS;
  const filtered = useMemo(() => allRows.filter((row) => activeModel === ALL_MODELS || tokenModelKey(row) === activeModel), [allRows, activeModel]);
  const activeCurrency = currency;
  const periodRows = useMemo(() => filtered.filter((row) => mode === "trends" || row.date.startsWith(period)), [filtered, mode, period]);
  const days = useMemo(() => tokenDays(periodRows, activeCurrency), [periodRows, activeCurrency]);
  const total = sumTokenUsage(periodRows, activeCurrency);
  const amount = (value: Totals) => isCost ? value.cost.total_cost : value.total_tokens;
  const peak = Math.max(0, ...[...days.values()].map(amount));
  const activeDays = [...days.values()].filter((day) => day.request_count > 0).length;
  const detailRows = selectedDay ? periodRows.filter((row) => row.date === selectedDay) : periodRows;
  const detail = sumTokenUsage(detailRows, activeCurrency);
  const detailModels = groupTokenModels(detailRows, activeCurrency, metric);
  const months = Array.from({ length: 12 }, (_, index) => {
    const prefix = `${year}-${String(index + 1).padStart(2, "0")}`;
    return { prefix, total: sumTokenUsage(filtered.filter((row) => row.date.startsWith(prefix)), activeCurrency) };
  });
  const monthPeak = Math.max(0, ...months.map((item) => amount(item.total)));
  const number = (value: number) => value.toLocaleString(locale);
  const money = (value: number, compact = false) => formatUsageCost(value, activeCurrency, locale, compact);
  const dateLabel = (date: string, options: Intl.DateTimeFormatOptions) => new Date(`${date}-01T00:00:00Z`).toLocaleDateString(locale, { ...options, timeZone: "UTC" });
  const label = mode === "calendar" ? dateLabel(period, { year: "numeric", month: "long" }) : String(year);
  const cacheValue = (value: TokenUsageTotals, compact = true) => value.cache_read_reported_count === 0
    ? "—"
    : `${compact ? formatTokenCount(value.cache_read_tokens) : number(value.cache_read_tokens)}${value.cache_read_reported_count < value.request_count ? " *" : ""}`;
  const costValue = (value: UsageCostTotals, field: CostField = "total_cost", compact = false) =>
    value.priced_request_count === 0 && value.unpriced_request_count + value.other_currency_request_count > 0 ? "—" : money(value[field], compact);
  const partial = (value: Totals) => value.missing_usage_count > 0 || (isCost && value.cost.unpriced_request_count + value.cost.other_currency_request_count > 0);
  const display = (value: Totals, compact = false) => isCost ? costValue(value.cost, "total_cost", compact) : formatTokenCount(value.total_tokens);
  const coverage = total.request_count > 0 ? total.cost.priced_request_count / total.request_count * 100 : 0;
  const coverageLabel = total.request_count ? `${new Intl.NumberFormat(locale, { maximumFractionDigits: 1 }).format(coverage)}%` : "—";
  const breakdown = isCost
    ? [[zh ? "普通输入" : "Uncached input", costValue(detail.cost, "input_cost")], [zh ? "缓存读取" : "Cache read", costValue(detail.cost, "cache_read_cost")], [zh ? "输出" : "Output", costValue(detail.cost, "output_cost")], [zh ? "已计价尝试" : "Priced attempts", number(detail.cost.priced_request_count)]]
    : [[zh ? "输入" : "Input", formatTokenCount(detail.input_tokens)], [zh ? "输出" : "Output", formatTokenCount(detail.output_tokens)], [zh ? "缓存读取" : "Cache read", cacheValue(detail)], [zh ? "模型尝试" : "Model attempts", number(detail.request_count)]];
  function move(delta: number) {
    const date = new Date(Date.UTC(year + (mode === "trends" ? delta : 0), month + (mode === "calendar" ? delta : 0), 1));
    setPeriod(date.toISOString().slice(0, 7));
    setSelectedDay(null);
  }
  function changeMetric(next: Metric) {
    setMetric(next);
    try { localStorage.setItem(VIEW_KEY, next); } catch { /* Still usable without local storage. */ }
  }
  const minPeriod = mode === "calendar" ? period === "1970-01" : year === 1970;
  const maxPeriod = mode === "calendar" ? period === "9998-12" : year === 9998;
  const weekdays = zh ? ["一", "二", "三", "四", "五", "六", "日"] : ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
  const unknownLabel = zh ? "未计价" : "Unpriced";
  const tooltip = (value: Totals) => isCost
    ? `${display(value)} ${activeCurrency}\n${zh ? "已计价尝试" : "Priced attempts"}: ${number(value.cost.priced_request_count)}\n${unknownLabel}: ${number(value.cost.unpriced_request_count)}\n${zh ? "不支持换汇的尝试" : "Unsupported-currency attempts"}: ${number(value.cost.other_currency_request_count)}${value.missing_usage_count ? `\n${zh ? "用量不完整，仅计入已记录部分" : "Incomplete usage; recorded portion only"}` : ""}`
    : `${zh ? "输入" : "Input"}: ${number(value.input_tokens)}\n${zh ? "输出" : "Output"}: ${number(value.output_tokens)}\n${zh ? "尝试" : "Attempts"}: ${number(value.request_count)}`;

  const cards = isCost ? [
    { title: zh ? "等值 API 费用" : "Equivalent API value", value: costValue(total.cost), exact: `${costValue(total.cost)} ${activeCurrency}`, sub: zh ? `${activeCurrency} · 当前参考价估算，非实际账单` : `${activeCurrency} · Current rates, not a bill`, icon: Coins, featured: true },
    { title: zh ? "输入费用" : "Input value", value: total.cost.priced_request_count ? money(total.cost.input_cost + total.cost.cache_read_cost) : costValue(total.cost), exact: total.cost.priced_request_count ? money(total.cost.input_cost + total.cost.cache_read_cost) : costValue(total.cost), sub: `${zh ? "含缓存读取" : "Includes cache read"} · ${costValue(total.cost, "cache_read_cost")}`, icon: ArrowDownLeft },
    { title: zh ? "输出费用" : "Output value", value: costValue(total.cost, "output_cost"), exact: costValue(total.cost, "output_cost"), sub: `${zh ? "单日峰值" : "Peak day"} · ${total.cost.priced_request_count ? money(peak) : "—"}`, icon: ArrowUpRight },
    { title: zh ? "计价覆盖率" : "Pricing coverage", value: coverageLabel, exact: coverageLabel, sub: zh ? `${number(total.cost.priced_request_count)} / ${number(total.request_count)} 次尝试 · ${activeDays} 个活跃日` : `${number(total.cost.priced_request_count)} / ${number(total.request_count)} attempts · ${activeDays} active days`, icon: ShieldCheck },
  ] : [
    { title: zh ? "总 Token" : "Total tokens", value: formatTokenCount(total.total_tokens), exact: number(total.total_tokens), sub: zh ? `${number(total.request_count)} 次模型尝试` : `${number(total.request_count)} model attempts`, icon: Zap, featured: true },
    { title: zh ? "输入 Token" : "Input tokens", value: formatTokenCount(total.input_tokens), exact: number(total.input_tokens), sub: `${zh ? "缓存读取" : "Cache read"} · ${cacheValue(total)}`, icon: ArrowDownLeft },
    { title: zh ? "输出 Token" : "Output tokens", value: formatTokenCount(total.output_tokens), exact: number(total.output_tokens), sub: zh ? "模型记录的输出用量" : "Recorded model output", icon: ArrowUpRight },
    { title: zh ? "活跃天数" : "Active days", value: number(activeDays), exact: number(activeDays), sub: `${zh ? "单日峰值" : "Peak day"} · ${formatTokenCount(peak)}`, icon: Activity },
  ];

  return (
    <section className="token-usage glass overflow-hidden rounded-2xl" data-metric={metric} aria-labelledby="token-usage-title">
      <div className="space-y-5 p-4 sm:p-6">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="flex items-center gap-3">
            <span className="token-heading-icon flex h-10 w-10 items-center justify-center rounded-xl">{isCost ? <Coins className="h-5 w-5" /> : <CalendarDays className="h-5 w-5" />}</span>
            <div>
              <h2 id="token-usage-title" className="font-semibold text-slate-900">{zh ? "用量与价值" : "Usage & value"}</h2>
              <p className="mt-1 text-xs text-slate-500">{zh ? "同一份消耗，Token 与 API 等值费用两个视角" : "Your usage, in tokens or equivalent API value"}</p>
            </div>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <div className="token-metric-switch inline-flex rounded-xl border p-1" role="group" aria-label={zh ? "显示单位" : "Display metric"}>
              {([{ value: "tokens", label: "Token", icon: Zap }, { value: "cost", label: zh ? "等值费用" : "API value", icon: Coins }] as const).map((view) =>
                <Button key={view.value} variant="ghost" size="sm" aria-pressed={metric === view.value} className="token-metric-button h-8 rounded-lg px-3 text-xs" onClick={() => changeMetric(view.value)}><view.icon className="h-3.5 w-3.5" />{view.label}</Button>)}
            </div>
            <Button variant="outline" size="sm" disabled={usage.isFetching} onClick={() => void usage.refetch()}>
              <RefreshCw className={cn("h-3.5 w-3.5", usage.isFetching && "animate-spin")} />{zh ? "刷新统计" : "Refresh"}
            </Button>
          </div>
        </div>

        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="token-view-switch inline-flex rounded-lg bg-muted p-1" role="group" aria-label={zh ? "统计时间视图" : "Usage time view"}>
            {[
              { value: "calendar", label: zh ? "每日消耗" : "Daily calendar", icon: CalendarDays },
              { value: "trends", label: zh ? "月度趋势" : "Monthly trends", icon: TrendingUp },
            ].map((view) => <Button key={view.value} variant="ghost" size="sm" aria-pressed={mode === view.value}
              className={cn("token-view-button h-7 text-xs", mode === view.value ? "bg-background text-foreground shadow-sm" : "text-muted-foreground")}
              onClick={() => { setMode(view.value); setSelectedDay(null); }}><view.icon className="h-3.5 w-3.5" />{view.label}</Button>)}
          </div>
          <div className="flex w-full flex-wrap items-center gap-2 sm:w-auto">
            <div className="flex items-center gap-1">
              <Button variant="ghost" size="icon" disabled={minPeriod} aria-label={zh ? "上一周期" : "Previous period"} onClick={() => move(-1)}><ChevronLeft className="h-4 w-4" /></Button>
              <span className="min-w-32 text-center text-sm font-medium text-slate-800" aria-live="polite">{label}</span>
              <Button variant="ghost" size="icon" disabled={maxPeriod} aria-label={zh ? "下一周期" : "Next period"} onClick={() => move(1)}><ChevronRight className="h-4 w-4" /></Button>
            </div>
            <Button size="sm" variant="outline" onClick={() => { setPeriod(today.slice(0, 7)); setMode("calendar"); setSelectedDay(null); }}>{zh ? "本月" : "This month"}</Button>
            <Select value={activeModel} onValueChange={(value) => { setModel(value); setSelectedDay(null); }}>
              <SelectTrigger className="w-full sm:w-56" aria-label={zh ? "筛选模型与提供商" : "Filter model and provider"}><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value={ALL_MODELS}>{zh ? "全部模型" : "All models"}</SelectItem>
                {models.map((item) => <SelectItem key={item.key} value={item.key}>{item.model || (zh ? "未知模型" : "Unknown model")} · {item.provider_name || (zh ? "未知提供商" : "Unknown provider")}</SelectItem>)}
              </SelectContent>
            </Select>
          </div>
        </div>

        {isCost && <div className="token-pricing-note flex flex-col items-start justify-between gap-3 rounded-xl border px-3 py-3 sm:flex-row sm:items-center sm:px-4">
          <div className="flex min-w-0 flex-1 items-start gap-2.5">
            <Info className="mt-0.5 h-4 w-4 shrink-0" />
            <div className="space-y-1 text-xs">
              <p className="font-medium">{zh ? "按模型参考价或你的自定义价格估算，不代表实际支出或订阅节省额。" : "Estimated at reference or custom API rates, not actual spend or subscription savings."}</p>
              <p className="text-slate-500">{zh ? `USD 与 CNY 按固定汇率 1 USD = ${USD_CNY_RATE.toFixed(1)} CNY 合并计算；切换币种仅改变显示单位。` : `USD and CNY are combined at a fixed rate of 1 USD = ${USD_CNY_RATE.toFixed(1)} CNY. Switching currency changes only the display unit.`}</p>
            </div>
          </div>
          <Select value={activeCurrency} onValueChange={setCurrency}>
            <SelectTrigger className="w-32 shrink-0" aria-label={zh ? "显示币种" : "Display currency"}><SelectValue /></SelectTrigger>
            <SelectContent>{USAGE_DISPLAY_CURRENCIES.map((value) => <SelectItem key={value} value={value}>{value === "USD" ? "$ USD" : "¥ CNY"}</SelectItem>)}</SelectContent>
          </Select>
        </div>}

        {priceNotice && <p role="status" className="rounded-lg border border-border bg-muted/30 px-3 py-2 text-xs text-slate-600">{priceNotice === "saved" ? (zh ? "自定义价格已保存，统计将按新价格重新计算。" : "Custom prices saved. Statistics are recalculated at the new rates.") : (zh ? "已移除此模型的自定义价格，重新使用匹配的前缀规则或自动参考价。" : "This model’s override was removed. Matching prefix rules or automatic reference rates now apply.")}</p>}
        {usage.isError && <div role="alert" className="rounded-xl border border-red-200 bg-red-50 p-4 text-sm text-red-600">{zh ? "用量统计读取失败，请重试。" : "Unable to load usage statistics. Please retry."} {usage.error instanceof Error ? usage.error.message : ""}</div>}
        {usage.isPending && <div role="status" className="flex h-56 items-center justify-center gap-2 text-sm text-slate-500"><RefreshCw className="h-4 w-4 animate-spin" />{zh ? "正在读取用量统计…" : "Loading usage statistics…"}</div>}
        {usage.data && <>
          {total.missing_usage_count > 0 && <div role="status" className="flex items-start gap-2 rounded-xl border border-amber-200 bg-amber-50 p-3 text-xs text-amber-700"><Info className="h-4 w-4 shrink-0" />{zh ? `${number(total.missing_usage_count)} 次尝试缺少完整用量；Token 与等值费用仅包含已记录部分。` : `${number(total.missing_usage_count)} attempts have incomplete usage; token and value totals include recorded portions only.`}</div>}
          {isCost && (total.cost.unpriced_request_count > 0 || total.cost.other_currency_request_count > 0) && <div role="status" className="flex flex-wrap gap-x-4 gap-y-1 rounded-xl border border-amber-200 bg-amber-50 p-3 text-xs text-amber-700">
            {total.cost.unpriced_request_count > 0 && <span>{zh ? `${number(total.cost.unpriced_request_count)} 次尝试未匹配可靠价格，未计入金额；— 不表示免费。` : `${number(total.cost.unpriced_request_count)} attempts have no reliable price and are excluded; — does not mean free.`}</span>}
            {total.cost.other_currency_request_count > 0 && <span>{zh ? `${number(total.cost.other_currency_request_count)} 次尝试属于其他币种，请切换币种查看。` : `${number(total.cost.other_currency_request_count)} attempts use another currency; switch currency to view them.`}</span>}
          </div>}
          <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
            {cards.map((card) => <div key={card.title} className={cn("token-kpi min-w-0 rounded-xl border p-3 sm:p-4", card.featured && "token-kpi-featured")}>
              <div className="flex items-center justify-between gap-2 text-xs text-slate-500"><span>{card.title}</span><card.icon className="h-4 w-4 shrink-0" /></div>
              <p title={card.exact} className="mt-3 truncate text-2xl font-semibold tracking-tight text-slate-900 tabular-nums sm:text-3xl">{card.value}</p>
              <p className="mt-2 text-[11px] text-slate-500 sm:text-xs">{card.sub}</p>
            </div>)}
          </div>

          <div className="grid items-start gap-5 xl:grid-cols-[minmax(0,1fr)_280px]">
            <div className="min-w-0">
              <div className="mb-4">
                <h3 className="text-sm font-semibold text-slate-800">{mode === "calendar" ? (isCost ? (zh ? "每日等值费用" : "Daily API value") : (zh ? "每日消耗日历" : "Daily usage calendar")) : (isCost ? (zh ? "月度费用趋势" : "Monthly value trends") : (zh ? "月度消耗趋势" : "Monthly usage trends"))}</h3>
                <p className="mt-1 text-xs text-slate-500">{mode === "calendar" ? (zh ? `点击日期查看明细 · 颜色越深，${isCost ? "等值费用" : "消耗"}越高` : `Select a day for details · Darker cells mean higher ${isCost ? "value" : "usage"}`) : (zh ? "点击月份进入当月日历" : "Select a month to explore its calendar")}{isCost && ` · ${activeCurrency}`}</p>
              </div>
              {mode === "calendar" ? <>
                <div className="grid grid-cols-7 gap-1.5 sm:gap-2">
                  {weekdays.map((day) => <div key={day} className="pb-1 text-center text-[11px] text-slate-500">{day}</div>)}
                  {tokenCalendar(year, month).map((date, index) => {
                    if (!date) return <div key={`blank-${index}`} aria-hidden="true" />;
                    const day = days.get(date) ?? EMPTY_TOTAL;
                    const dayValue = day.request_count === 0 ? "—" : isCost ? display(day) : day.total_tokens ? formatTokenCount(day.total_tokens) : day.missing_usage_count ? "?" : "0";
                    return <button type="button" key={date} className="token-day flex min-h-16 min-w-0 flex-col justify-between rounded-lg border p-1.5 text-left sm:min-h-20 sm:p-2.5"
                      data-heat={tokenHeat(amount(day), peak)} data-today={date === today} aria-pressed={selectedDay === date}
                      aria-label={`${date}: ${isCost ? tooltip(day) : `${number(day.total_tokens)} tokens`}${partial(day) ? (zh ? "，数据不完整" : ", partial data") : ""}`}
                      title={`${date}\n${tooltip(day)}`} onClick={() => setSelectedDay(selectedDay === date ? null : date)}>
                      <span className="flex items-center justify-between text-xs text-slate-600"><span className="token-date">{Number(date.slice(-2))}</span>{partial(day) && <span className="h-1.5 w-1.5 rounded-full bg-amber-500" aria-hidden="true" />}</span>
                      <span className="token-amount truncate text-[10px] font-medium text-slate-800 tabular-nums sm:text-xs"><span className="hidden sm:inline">{dayValue}</span><span className="sm:hidden">{isCost && day.cost.priced_request_count > 0 ? compactUsageCost(day.cost.total_cost) : day.request_count && amount(day) > 0 ? new Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: 0 }).format(day.total_tokens) : dayValue}</span></span>
                    </button>;
                  })}
                </div>
                <div className="mt-3 flex flex-wrap items-center justify-end gap-1.5 text-[10px] text-slate-500">{partial(total) && <span className="mr-auto flex items-center gap-1"><span className="h-1.5 w-1.5 rounded-full bg-amber-500" />{zh ? "部分用量或计价缺失" : "Incomplete usage or pricing"}</span>}<span>{zh ? "少" : "Less"}</span>{[0, 1, 2, 3, 4, 5].map((heat) => <span key={heat} data-heat={heat} className="token-swatch h-2.5 w-2.5 rounded-sm" />)}<span>{zh ? "多" : "More"}</span></div>
              </> : <div className="token-month-chart grid h-72 grid-cols-12 gap-1.5 sm:gap-3">
                {months.map((item) => <button type="button" key={item.prefix} className="token-month flex min-w-0 flex-col items-center gap-2 rounded-md px-0.5 pb-2 pt-3" aria-label={`${item.prefix}: ${tooltip(item.total)}`} title={`${item.prefix}\n${tooltip(item.total)}`} onClick={() => { setPeriod(item.prefix); setMode("calendar"); setSelectedDay(null); }}>
                  <span className="hidden max-w-full truncate text-[10px] text-slate-500 sm:block">{item.total.request_count ? display(item.total, true) : "—"}{partial(item.total) && " *"}</span>
                  <span className="flex w-full flex-1 items-end justify-center"><span className="token-month-bar w-full max-w-10 rounded-t-md" style={{ height: `${monthPeak > 0 && amount(item.total) > 0 ? Math.max(1, amount(item.total) / monthPeak * 100) : 0}%` }} /></span>
                  <span className="text-[10px] text-slate-600">{dateLabel(item.prefix, { month: "short" })}</span>
                </button>)}
              </div>}
            </div>

            <aside className="token-detail rounded-xl border p-4" aria-live="polite">
              <div className="flex items-center justify-between gap-2">
                <h3 className="text-sm font-semibold text-slate-800">{selectedDay ? (zh ? "当日明细" : "Day details") : (zh ? "周期摘要" : "Period summary")}</h3>
                {selectedDay && <Button variant="ghost" size="sm" className="h-6 px-1.5 text-[11px]" onClick={() => setSelectedDay(null)}>{zh ? "整月" : "Whole month"}</Button>}
              </div>
              <p className="mt-1 text-xs text-slate-500">{selectedDay ?? label} · UTC</p>
              <p className="mt-5 break-words text-3xl font-semibold tracking-tight text-slate-900 tabular-nums" title={isCost ? tooltip(detail) : number(detail.total_tokens)}>{display(detail)}</p>
              <p className="mt-1 text-xs text-slate-500">{isCost ? `${activeCurrency} · ${zh ? "等值 API 估算" : "Estimated API value"}` : (zh ? "已记录 Token" : "Recorded tokens")}</p>
              {isCost && <p className="mt-2 text-[11px] text-slate-500">{formatTokenCount(detail.total_tokens)} tokens · {zh ? "原始消耗" : "Recorded usage"}</p>}
              <dl className="mt-5 space-y-3 text-xs">
                {breakdown.map(([key, value]) => <div key={key} className="flex justify-between gap-2"><dt className="text-slate-500">{key}</dt><dd className="font-medium text-slate-800 tabular-nums">{value}</dd></div>)}
              </dl>
              {detail.request_count === 0 ? <p className="mt-5 text-xs text-slate-500">{zh ? "这段时间没有已记录的模型尝试。" : "No recorded model attempts in this period."}</p> : <>
                <div className="token-composition mt-5 flex h-1.5 overflow-hidden rounded-full" aria-hidden="true">{(isCost ? [detail.cost.input_cost, detail.cost.cache_read_cost, detail.cost.output_cost] : [detail.input_tokens, 0, detail.output_tokens]).map((value, index) => <span key={index} className={["token-input-segment", "token-cache-segment", "token-output-segment"][index]} style={{ width: `${amount(detail) > 0 ? value / amount(detail) * 100 : 0}%` }} />)}</div>
                <p className="mt-2 text-[10px] leading-relaxed text-slate-500">{isCost ? (zh ? "普通输入 / 缓存 / 输出 · 缓存不重复计费" : "Input / cache / output · Cache is not counted twice") : (zh ? "输入 / 输出构成 · 缓存已包含在输入内" : "Input / output mix · Cache is included in input")}</p>
                {isCost && partial(detail) && <p className="mt-3 text-[11px] text-amber-600">{zh ? "仅含已知用量与已匹配价格，金额不完整。" : "Known usage and matched prices only; value is partial."}</p>}
                {isCost && detail.cost.other_currency_request_count > 0 && <p className="mt-2 text-[11px] text-slate-500">{zh ? "其他币种未计入此金额。" : "Other currencies are excluded from this value."}</p>}
                {isCost && detail.cost.cache_rate_fallback_count > 0 && <p className="mt-3 text-[11px] leading-relaxed text-slate-500">{zh ? "部分模型未提供缓存单价，缓存按普通输入价估算。" : "Some cache rates are unavailable; those cache reads use the regular input rate."}</p>}
              </>}
            </aside>
          </div>

          {isCost ? <TokenModelCosts key={`${period}:${mode}:${activeModel}:${selectedDay ?? "period"}`} rows={detailRows} currency={activeCurrency} selectedDay={selectedDay} onEdit={(target) => { setPriceNotice(null); setPriceTarget(target); }} /> : <div className="token-models border-t pt-5">
            <div className="mb-3 flex flex-wrap items-baseline justify-between gap-2">
              <h3 className="text-sm font-semibold text-slate-800">{selectedDay ? `${selectedDay} · ` : ""}{zh ? "模型消耗" : "Usage by model"}</h3>
              <span className="text-xs text-slate-500">{zh ? "按 Token 消耗排序 · 区分提供商" : "Ranked by tokens · Providers kept separate"}</span>
            </div>
            {detailModels.length === 0 ? <div className="py-8 text-center text-sm text-slate-500">{zh ? "暂无已记录的模型消耗" : "No recorded model usage"}</div> : <div className="overflow-x-auto">
              <table className="w-full text-xs tabular-nums">
                <thead className="text-slate-500"><tr>{[zh ? "模型 / 提供商" : "Model / provider", zh ? "总 Token" : "Total tokens", zh ? "占比" : "Share", zh ? "输入" : "Input", zh ? "缓存读取" : "Cache read", zh ? "输出" : "Output", zh ? "尝试数" : "Attempts"].map((text, index) => <th key={text} scope="col" className={cn("whitespace-nowrap px-3 py-2 font-medium", index === 0 ? "pl-0 text-left" : "text-right")}>{text}</th>)}</tr></thead>
                <tbody>{detailModels.map((item) => {
                  const share = detail.total_tokens > 0 ? item.total_tokens / detail.total_tokens * 100 : 0;
                  return <tr key={item.key} className="token-model-row border-t text-slate-700">
                    <td className="min-w-40 max-w-64 py-3 pr-3"><p className="break-words font-medium text-slate-800">{item.model || (zh ? "未知模型" : "Unknown model")}</p><p className="mt-1 break-words text-[11px] text-slate-500">{item.provider_name || (zh ? "未知提供商" : "Unknown provider")}</p></td>
                    <td className="whitespace-nowrap px-3 py-3 text-right font-medium" title={number(item.total_tokens)}>{formatTokenCount(item.total_tokens)}</td>
                    <td className="min-w-24 px-3 py-3 text-right"><span>{share.toFixed(1)}%</span><div className="token-share mt-1.5 h-1 overflow-hidden rounded-full"><div className="token-share-fill h-full rounded-full" style={{ width: `${share}%` }} /></div></td>
                    <td className="whitespace-nowrap px-3 py-3 text-right" title={number(item.input_tokens)}>{formatTokenCount(item.input_tokens)}</td>
                    <td className="whitespace-nowrap px-3 py-3 text-right" title={cacheValue(item, false)}>{cacheValue(item)}</td>
                    <td className="whitespace-nowrap px-3 py-3 text-right" title={number(item.output_tokens)}>{formatTokenCount(item.output_tokens)}</td>
                    <td className="px-3 py-3 text-right">{number(item.request_count)}{item.missing_usage_count > 0 && <span className="mt-1 block whitespace-nowrap text-[10px] text-amber-600">{number(item.missing_usage_count)} {zh ? "用量不完整" : "incomplete"}</span>}</td>
                  </tr>;
                })}</tbody>
              </table>
            </div>}
          </div>}
        </>}
      </div>
      <div className="token-footer border-t px-4 py-3 sm:px-6">
        <div className="flex flex-wrap justify-between gap-x-4 gap-y-1 text-[11px] text-slate-500"><span>{zh ? "自然月 / 自然年 · UTC 时区 · 独立于下方请求分析的时间范围" : "Calendar months / years · UTC · Independent of the request analytics range below"}</span>{usage.data && <span>{zh ? "更新于" : "Updated"} {new Date(usage.data.generated_at).toLocaleTimeString(locale)}</span>}</div>
        <details className="mt-2 text-[11px] text-slate-500"><summary className="w-fit cursor-pointer font-medium">{zh ? "统计口径与数据说明" : "Counting rules & data coverage"}</summary><p className="mt-2 max-w-4xl leading-relaxed">{zh ? "仅汇总 Nyro 保留的请求日志，包含重试及失败尝试；总量 = 输入 + 输出，缓存是输入子项，不重复累加。按 UTC 请求时间归属日期，不是上游账单。清理日志会影响历史统计。数据库为空的缓存字段显示 —，部分记录有值时显示 *。当前日志写入可能将未上报用量保存为 0，因此 0 不一定表示实际零消耗；缺失提示仅覆盖数据库中的空值，不保证完整覆盖。" : "Aggregated from retained Nyro request logs, including retries and failed attempts. Total = input + output; cache is part of input and is never added twice. Dates follow the UTC request timestamp, not upstream billing. Deleting logs affects history. Null cache fields show —; partially recorded cache values show *. Current logging may store unreported usage as 0, so zero does not always mean no consumption. Missing-usage indicators only cover database nulls and are not a complete coverage guarantee."}</p><p className="mt-2 max-w-4xl leading-relaxed">{zh ? "等值费用优先采用你设置的提供商 / 模型单价，否则采用本地参考目录的当前价格，不保存历史成交价；未设置且未匹配价格的模型不计价。价格覆盖按当前币种已匹配价格的尝试数计算，不代表用量完整度。未记录的缓存读取按普通输入处理；尚未单独核算缓存写入溢价、长上下文阶梯、批量折扣、工具调用、图片/音频等非 Token 收费及税费。因此这不是精确账单，也不是订阅节省金额。" : "API value uses your custom provider/model rates when set, otherwise current rates from the local reference catalog, not historical transaction prices. Models with neither custom nor reference rates are unpriced. Coverage counts attempts with a matched price in the selected currency, not usage completeness. Unreported cache reads are treated as regular input. Cache-write premiums, long-context tiers, batch discounts, tools, non-token image/audio charges and taxes are not separately modeled. This is neither an exact bill nor a measure of subscription savings."}</p></details>
      </div>
      {priceTarget && <TokenPriceDialog key={tokenModelKey(priceTarget)} target={priceTarget} onClose={() => setPriceTarget(null)} onSaved={(config, reset) => {
        setPriceNotice(reset ? "reset" : "saved");
        const effective = config.override_price ?? config.inherited_price ?? config.default_price;
        if (effective) setCurrency(effective.currency);
      }} />}
    </section>
  );
}
