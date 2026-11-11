import { Clock } from "lucide-react";
import { claudeWindowSourceTitle, isClaudeMainWindow, shortLocalTime, type ClaudeWindowView } from "@/lib/claude-usage-windows";
import { tierLabel } from "@/lib/provider-usage";
import { steadyPacePercent, utilizationColor } from "@/lib/usage-pace";
import { UsageProgress } from "@/components/usage-progress";

const percentText = (used: number) => `${Number(used.toFixed(1))}%`;

function ClaudeUsageWindowRow({ view, now, isZh, markStale }: {
  view: ClaudeWindowView; now: number; isZh: boolean; markStale: boolean;
}) {
  const label = tierLabel(view.name, isZh);
  // A finished cycle keeps its recorded value as text only; time never implies 0%.
  const showBar = view.used != null && !view.expired;
  const resetMs = view.reset ? Date.parse(view.reset) : NaN;
  return <div className="flex items-center gap-2 text-xs" data-claude-window={view.name}
    data-claude-source={view.source ?? undefined} title={claudeWindowSourceTitle(view, now, isZh)}>
    <span className="min-w-14 max-w-36 shrink-0 truncate font-medium text-slate-500">{label}</span>
    {showBar ? <UsageProgress usedPercent={view.used!} pacePercent={steadyPacePercent({ name: view.name, resets_at: view.reset }, now)}
      label={`${label} ${isZh ? "已用额度" : "used quota"}`} isZh={isZh} />
      : <span className="min-w-20 flex-1" />}
    <span className={`w-12 shrink-0 text-right tabular-nums ${view.used == null ? "text-slate-400"
      : view.expired ? "font-semibold text-slate-400" : `font-semibold ${utilizationColor(view.used)}`}`}>
      {view.used == null ? (isZh ? "未知" : "Unknown") : percentText(view.used)}
    </span>
    {markStale && !view.fresh && <span data-claude-stale-mark className="shrink-0 rounded bg-amber-50 px-1 text-[10px] font-medium text-amber-700"
      title={view.expired ? (isZh ? "已过重置时间，等待新数据" : "Past reset; awaiting new data") : (isZh ? "数据可能已过时" : "Data may be outdated")}>
      {isZh ? "旧" : "Stale"}
    </span>}
    <span className="flex w-20 shrink-0 items-center gap-0.5 text-[10px] text-slate-400">
      {view.reset && <><Clock aria-hidden="true" className="h-3 w-3 shrink-0" />
        <time dateTime={view.reset} title={`${isZh ? "重置" : "Resets"} ${new Date(resetMs).toLocaleString()}`}>{shortLocalTime(resetMs, now)}</time></>}
    </span>
  </div>;
}

/**
 * One compact row per effective window. Main-window staleness is summarized by
 * the caller when it has a header (`markMainStale` false); model-specific
 * windows always get their own short stale mark.
 */
export function ClaudeUsageWindowRows({ views, now, isZh, markMainStale }: {
  views: ClaudeWindowView[]; now: number; isZh: boolean; markMainStale: boolean;
}) {
  return <div className="flex flex-col gap-1.5" data-claude-usage-windows>
    {views.map((view, index) => <ClaudeUsageWindowRow key={`${view.name}:${index}`} view={view} now={now} isZh={isZh}
      markStale={markMainStale || !isClaudeMainWindow(view.name)} />)}
  </div>;
}
