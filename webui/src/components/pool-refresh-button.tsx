import { RefreshCw } from "lucide-react";

/** Top-right icon action shared by the pool usage boxes; the box needs `relative pr-10`. */
export function PoolRefreshButton({ onRefresh, refreshing, isZh, hint }: {
  onRefresh: () => void; refreshing: boolean; isZh: boolean;
  /** Short policy note appended to the hover text. */
  hint?: string;
}) {
  const label = isZh ? "刷新全池用量" : "Refresh pool usage";
  return <button
    type="button"
    onClick={(event) => { event.stopPropagation(); onRefresh(); }}
    disabled={refreshing}
    aria-label={label}
    aria-busy={refreshing}
    title={refreshing ? (isZh ? "刷新用量中…" : "Refreshing usage…") : hint ? `${label}\n${hint}` : label}
    className="absolute right-2 top-1.5 cursor-pointer rounded p-1 text-slate-400 transition-colors hover:bg-slate-100 hover:text-slate-600 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-wait disabled:opacity-50"
  >
    <RefreshCw aria-hidden="true" className={`h-3.5 w-3.5 ${refreshing ? "animate-spin" : ""}`} />
  </button>;
}
