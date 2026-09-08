import { useMemo, useState, type PointerEvent } from "react";
import {
  buildPerformanceHitIndex, groupPerformancePoints, layoutPerformanceLabels, PERFORMANCE_CHART,
  performanceTierLabel, performanceTpsMaximum, visiblePerformanceSelection, type PerformancePoint,
} from "@/lib/model-performance";
import { formatLocalDateTime } from "@/lib/format";
import { Button } from "@/components/ui/button";

/** A small SVG keeps point centers stationary and overlap selection explicit. No jitter. */
export function ModelPerformanceChart({ points, isZh }: {
  points: PerformancePoint[]; isZh: boolean;
}) {
  const [hover, setHover] = useState<string[]>([]);
  const [pinned, setPinned] = useState<string[]>([]);
  const [zoom, setZoom] = useState(1);
  // Visible valid points determine Y; fixed X and snapshot-assigned IDs never change.
  const yMax = performanceTpsMaximum(points);
  const groups = useMemo(() => groupPerformancePoints(points, yMax), [points, yMax]);
  const labels = useMemo(() => layoutPerformanceLabels(groups), [groups]);
  const hit = useMemo(() => buildPerformanceHitIndex(groups), [groups]);
  const groupSizes = useMemo(() => new Map(groups.flatMap((group) => group.points.map((point) => [point.key, group.points.length] as const))), [groups]);
  const active = visiblePerformanceSelection(points, hover, pinned);
  const activeSet = new Set(active);
  const candidates = points.filter((point) => pinned.includes(point.key));
  const hiddenPinned = pinned.filter((key) => !points.some((point) => point.key === key)).length;
  const hiddenLabels = groups.filter((group) => !labels.has(group.key)).length;
  const { width, height, left, right, top, bottom } = PERFORMANCE_CHART;
  const sourceLabel = (source: PerformancePoint["source"]) => source === "override" ? (isZh ? "独立覆盖" : "Override") : source === "common" ? (isZh ? "通用评分" : "Common") : (isZh ? "未评分" : "Unrated");
  // Candidate identities must not collapse visually through legacy TPS rounding.
  const exactTps = (tps: number) => `${tps} tok/s`;
  const title = (point: PerformancePoint) => `${point.pointId} · ${point.providerName} / ${point.model} · ${performanceTierLabel(point.tier, isZh)} · ${point.score}/100 · ${exactTps(point.tps)}`;
  function pointerCandidates(event: PointerEvent<SVGSVGElement>) {
    const matrix = event.currentTarget.getScreenCTM();
    if (!matrix) return [];
    const actual = new DOMPoint(event.clientX, event.clientY).matrixTransform(matrix.inverse());
    return hit(actual.x, actual.y).map((point) => point.key);
  }
  function chooseGroup(x: number, y: number) { setPinned(hit(x, y).map((point) => point.key)); setHover([]); }
  function clear() { setPinned([]); setHover([]); }
  return (
    <div onKeyDown={(event) => { if (event.key === "Escape") { clear(); event.stopPropagation(); } }}>
      <div className="flex flex-wrap items-center gap-3 border-b border-slate-200 p-3 text-xs text-slate-600">
        <label className="flex items-center gap-2">{isZh ? "缩放" : "Zoom"}
          <select className="rounded border bg-white p-1" value={zoom} onChange={(event) => setZoom(Number(event.target.value))}>
            <option value={1}>100%</option><option value={1.5}>150%</option><option value={2}>200%</option>
          </select>
        </label>
        <span>{isZh ? "空心：有效样本少于 3；×N：完全重合的成员数" : "Hollow: fewer than 3 valid samples · ×N: exactly coincident members"}</span>
        <Button size="sm" variant="secondary" onClick={clear} disabled={!pinned.length && !hover.length}>{isZh ? "清除选择" : "Clear selection"}</Button>
        {!!hiddenPinned && <span role="status">{isZh ? `${hiddenPinned} 个已选点被筛选隐藏` : `${hiddenPinned} selected points hidden by filters`}</span>}
      </div>
      <div className="grid min-w-0 xl:grid-cols-[minmax(0,1fr)_340px]">
        <div className="min-w-0">
          <div className="overflow-auto" tabIndex={0} role="region" aria-label={isZh ? "能力与速度散点图，可滚动" : "Capability and speed scatter plot, scrollable"}>
            <svg viewBox={`0 0 ${width} ${height}`} style={{ width: `${zoom * 100}%`, minWidth: 660 * zoom, height: "auto" }}
              data-testid="performance-chart" data-x-min={0} data-x-max={100} data-y-max={yMax}
              data-plot-left={left} data-plot-right={right} data-plot-top={top} data-plot-bottom={bottom}
              aria-label={isZh ? "评分 0–100 与平均 TPS；完整信息见编号列表" : "Score 0–100 versus average TPS; full details in numbered index"}
              onPointerMove={(event) => { if (!(event.target as Element).closest('[role="button"]')) setHover(pointerCandidates(event)); }}
              onPointerLeave={() => setHover([])}
              onClick={(event) => {
                if ((event.target as Element).closest('[role="button"]')) return;
                const matrix = event.currentTarget.getScreenCTM();
                if (matrix) { const p = new DOMPoint(event.clientX, event.clientY).matrixTransform(matrix.inverse()); chooseGroup(p.x, p.y); }
              }}>
              <title>{isZh ? "固定实际坐标的性能图" : "Performance at actual, fixed coordinates"}</title>
              {[0, 20, 40, 60, 80, 100].map((tick) => {
                const x = left + (right - left) * tick / 100;
                return <g key={tick}><line x1={x} x2={x} y1={top} y2={bottom} stroke="#e2e8f0" strokeDasharray="3 3" /><text x={x} y={bottom + 23} textAnchor="middle" fontSize={11} fill="#64748b">{tick}</text></g>;
              })}
              {Array.from({ length: 6 }, (_, index) => {
                const y = bottom - (bottom - top) * index / 5;
                return <g key={index}><line x1={left} x2={right} y1={y} y2={y} stroke="#e2e8f0" strokeDasharray="3 3" /><text x={left - 12} y={y + 4} textAnchor="end" fontSize={11} fill="#64748b">{new Intl.NumberFormat("en", { maximumFractionDigits: 1 }).format(yMax * index / 5)}</text></g>;
              })}
              <text x={(left + right) / 2} y={height - 14} textAnchor="middle" fontSize={12} fill="#64748b">{isZh ? "能力评分（0–100）" : "Capability score (0–100)"}</text>
              <text transform={`translate(16 ${(top + bottom) / 2}) rotate(-90)`} textAnchor="middle" fontSize={12} fill="#64748b">TPS (tok/s)</text>
              {groups.map((group) => {
                const label = labels.get(group.key), first = group.points[0];
                const selected = group.points.some((point) => activeSet.has(point.key));
                const low = group.points.some((point) => point.validTpsCount < 3);
                const mixedSamples = low && group.points.some((point) => point.validTpsCount >= 3);
                return <g key={group.key} role="button" tabIndex={0} aria-label={`${isZh ? `${group.points.length} 个同坐标成员` : `${group.points.length} coincident members`}. ${group.points.map((point) => `${title(point)}; ${point.validTpsCount} ${isZh ? "个有效样本" : "valid samples"}${point.validTpsCount < 3 ? (isZh ? "，低样本量" : ", low sample count") : ""}`).join("; ")}. ${isZh ? "选择此区域所有候选点" : "Select all nearby candidates"}`}
                  aria-pressed={group.points.some((point) => pinned.includes(point.key))}
                  style={{ cursor: "pointer", opacity: active.length && !selected ? 0.22 : 1 }}
                  onPointerEnter={() => setHover(hit(group.x, group.y).map((p) => p.key))}
                  onFocus={() => setHover(hit(group.x, group.y).map((p) => p.key))} onBlur={() => setHover([])}
                  onClick={() => chooseGroup(group.x, group.y)} onKeyDown={(event) => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); chooseGroup(group.x, group.y); } }}>
                  <title>{group.points.map(title).join("\n")}</title>
                  {label && <g data-testid="performance-label"><line x1={group.x} y1={group.y} x2={label.x + label.width / 2} y2={label.y + label.height / 2} stroke={first.color} strokeOpacity={0.4} />
                    <rect {...label} rx={3} fill="white" stroke={selected ? first.color : "#e2e8f0"} />
                    <text x={label.x + label.width / 2} y={label.y + 12.5} textAnchor="middle" fontSize={11} fontWeight={600} fill={first.color}>{first.pointId}{group.points.length > 1 ? ` ×${group.points.length}` : ""}</text></g>}
                  <circle cx={group.x} cy={group.y} r={14} fill="transparent" />
                  <circle data-testid="performance-point" data-point-ids={group.points.map((p) => p.pointId).join(",")} data-member-count={group.points.length}
                    data-score={first.score} data-tps={first.tps} cx={group.x} cy={group.y} r={selected ? 7 : 5.5}
                    fill={low ? "white" : first.color} stroke={first.color} strokeWidth={selected ? 3 : 2} />
                  {mixedSamples && <circle cx={group.x} cy={group.y} r={2.5} fill={first.color} />}
                </g>;
              })}
            </svg>
          </div>
          {!!hiddenLabels && <p className="p-3 text-xs text-amber-700" role="status">{isZh ? `${hiddenLabels} 个密集位置的标签已隐藏以避免碰撞；可选择点或使用完整编号列表。` : `${hiddenLabels} dense-position labels omitted to prevent collisions; select the dots or use the complete numbered index.`}</p>}
          {candidates.length > 0 && <section className="border-t border-slate-200 p-3" aria-label={isZh ? "已选区域候选点" : "Selected region candidates"}>
            <h3 className="text-sm font-semibold">{isZh ? `已选区域：${candidates.length} 个候选点` : `Selected region: ${candidates.length} candidates`}</h3>
            <p className="my-2 text-xs text-slate-500">{isZh ? "下列均为真实坐标，不是聚合中心。选择单个编号以固定高亮。" : "Each coordinate below is actual, not a cluster centroid. Select an ID to pin one member."}</p>
            <div className="max-h-64 space-y-1 overflow-auto">{candidates.map((point) => <button key={point.key} className="block w-full rounded border border-slate-200 p-2 text-left text-xs focus-visible:outline-2 focus-visible:outline-blue-600"
              onClick={() => { setPinned([point.key]); setHover([]); }} onFocus={() => setHover([point.key])} onBlur={() => setHover([])} onPointerEnter={() => setHover([point.key])} onPointerLeave={() => setHover([])}>
              <strong>{point.pointId}</strong> · {point.score}/100 · {exactTps(point.tps)} · {point.validTpsCount < 3 ? "○ " : "● "}{isZh ? `${point.validTpsCount} 个有效样本` : `${point.validTpsCount} valid samples`}
              <span className="mt-1 block whitespace-pre-wrap break-all">{point.providerName} / {point.model} · {performanceTierLabel(point.tier, isZh)}</span>
            </button>)}</div>
          </section>}
        </div>
        <aside className="min-w-0 border-t border-slate-200 xl:border-t-0 xl:border-l" aria-label={isZh ? "完整编号索引" : "Complete numbered index"}>
          <h3 className="p-3 text-sm font-semibold">{isZh ? "编号与完整信息" : "Point IDs & full details"} ({points.length})</h3>
          <p className="px-3 pb-3 text-xs text-slate-500">{isZh ? "悬停或聚焦双向高亮；点击固定，Esc 清除。编号在完整快照中分配，筛选和缩放不改变编号。" : "Hover or focus to highlight both views; click to pin, Escape to clear. IDs come from the full snapshot and survive filters and zoom."}</p>
          <div className="max-h-[620px] space-y-2 overflow-y-auto p-3 pt-0">
            {points.map((point) => {
              const selected = activeSet.has(point.key);
              const coincident = groupSizes.get(point.key) ?? 1;
              return <button key={point.key} data-testid="performance-index-row" data-point-id={point.pointId} aria-pressed={pinned.includes(point.key)}
                className={`block w-full rounded-lg border p-3 text-left text-xs focus-visible:outline-2 focus-visible:outline-blue-600 ${selected ? "border-blue-500 bg-blue-50" : "border-slate-200"}`}
                style={{ opacity: active.length && !selected ? 0.45 : 1 }}
                onPointerEnter={() => setHover([point.key])} onPointerLeave={() => setHover([])} onFocus={() => setHover([point.key])} onBlur={() => setHover([])}
                onClick={() => { setPinned([point.key]); setHover([]); }}>
                <span className="font-bold" style={{ color: point.color }}>{point.validTpsCount < 3 ? "○" : "●"} {point.pointId}</span>
                {coincident > 1 && <span className="ml-2">{isZh ? `同坐标 ×${coincident}` : `Coincident ×${coincident}`}</span>}
                <span className="mt-1 block whitespace-pre-wrap break-all font-semibold">{point.providerName}</span>
                <span className="block break-all text-slate-500">{point.providerId}</span>
                <span className="mt-1 block whitespace-pre-wrap break-all font-mono">{point.model}</span>
                <span className="mt-2 block">{performanceTierLabel(point.tier, isZh)} · {sourceLabel(point.source)}</span>
                <span className="block font-semibold">{point.score}/100 · {exactTps(point.tps)}</span>
                <span className="block">{isZh ? `有效 TPS ${point.validTpsCount} / 已选请求 ${point.selectedRequestCount}` : `Valid TPS ${point.validTpsCount} / selected requests ${point.selectedRequestCount}`}</span>
                {point.validTpsCount < 3 && <span className="block text-amber-700">{isZh ? "低样本量" : "Low sample count"}</span>}
                <span className="mt-1 block text-slate-500">{isZh ? "样本起始" : "First sample"}: {formatLocalDateTime(point.firstSampleAt)}</span>
                <span className="block text-slate-500">{isZh ? "样本结束" : "Last sample"}: {formatLocalDateTime(point.lastSampleAt)}</span>
                <span className="block text-slate-500">{isZh ? "评分更新" : "Score updated"}: {formatLocalDateTime(point.scoreUpdatedAt)}</span>
                {!point.providerEnabled && <span className="block text-amber-700">{isZh ? "供应商已禁用或未知" : "Provider disabled or unknown"}</span>}
              </button>;
            })}
          </div>
        </aside>
      </div>
    </div>
  );
}
