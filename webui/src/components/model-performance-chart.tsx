import { useEffect, useId, useMemo, useRef, useState } from "react";
import {
  buildPerformanceEnvelope, buildPerformanceHitIndex, envelopeLabelGroups, groupPerformancePoints, layoutPerformanceLabels, PERFORMANCE_CHART,
  PERFORMANCE_COINCIDENT_MARKER_SIZE, PERFORMANCE_MARKER_SIZE, performanceTpsMaximum, performanceScoreDomain, performanceXSpan,
  pointCoordinates, scoreX, type PerformancePoint,
} from "@/lib/model-performance";
import { createPortal } from "react-dom";
import { formatLocalDateTime, formatTps } from "@/lib/format";
import { useProviderIconMarkup } from "@/lib/provider-icon";

/** A point is drawn as its provider's icon, never as a plain dot. */
function PerformancePointMarker({ point, x, y, offsetX, size, lowSample, selected }: {
  point: PerformancePoint; x: number; y: number; offsetX: number; size: number; lowSample: boolean; selected: boolean;
}) {
  const { iconKey, iconMarkup } = useProviderIconMarkup({
    iconKey: point.providerIcon || undefined, name: point.providerName, baseUrl: point.providerBaseUrl,
  });
  const glyph = size * 0.7;
  const markerX = x + offsetX;
  return (
    <g data-testid="performance-point-icon" data-point-ids={point.pointId} data-provider-id={point.providerId}
      data-icon-key={iconKey ?? ""} data-low-sample={lowSample ? "true" : "false"}
      data-x={x} data-y={y} data-marker-x={markerX} data-marker-y={y}
      style={{ color: point.color }}>
      <rect x={markerX - size / 2} y={y - size / 2} width={size} height={size} rx={size * 0.28}
        fill={point.color} fillOpacity={selected ? 0.24 : 0.1} stroke={point.color}
        strokeWidth={selected ? 2.4 : 1.4} strokeDasharray={lowSample ? "3 2" : undefined} />
      {iconMarkup
        ? <svg x={markerX - glyph / 2} y={y - glyph / 2} width={glyph} height={glyph} overflow="hidden"
          opacity={lowSample ? 0.75 : 1} aria-hidden="true" dangerouslySetInnerHTML={{ __html: iconMarkup }} />
        : <text x={markerX} y={y + size * 0.23} textAnchor="middle" fontSize={size * 0.52} fontWeight={700}
          fill={point.color} opacity={lowSample ? 0.75 : 1}>
          {(point.providerName || "?").slice(0, 1).toUpperCase()}
        </text>}
    </g>
  );
}

/** Labels may move, but point centers always retain their actual score/TPS coordinates. */
export function ModelPerformanceChart({ points, isZh }: { points: PerformancePoint[]; isZh: boolean }) {
  const [active, setActive] = useState<string[]>([]);
  const [zoom, setZoom] = useState(1);
  const [position, setPosition] = useState({ left: 0, top: 0 });
  const closeTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const cancelClose = () => { if (closeTimer.current) clearTimeout(closeTimer.current); };
  const closeSoon = () => { cancelClose(); closeTimer.current = setTimeout(() => setActive([]), 200); };
  useEffect(() => () => { if (closeTimer.current) clearTimeout(closeTimer.current); }, []);
  const tooltipId = useId();
  const yMax = performanceTpsMaximum(points);
  const xSpan = performanceXSpan();
  const xDomain = useMemo(() => performanceScoreDomain(points), [points]);
  const groups = useMemo(() => groupPerformancePoints(points, yMax, xDomain), [points, yMax, xDomain]);
  const envelope = useMemo(() => buildPerformanceEnvelope(points), [points]);
  const envelopePath = envelope.nodes.map((node) => {
    const { x, y } = pointCoordinates(node, yMax, xDomain);
    return `${x},${y}`;
  }).join(" ");
  const envelopeLabel = isZh ? "当前可见模型的能力–速度包络线" : "Capability–speed envelope of visible models";
  const membershipLabel = isZh ? "位于当前可见模型的包络线" : "On the visible-model envelope";
  const envelopeGroups = useMemo(() => envelopeLabelGroups(groups, envelope), [groups, envelope]);
  const labels = useMemo(() => layoutPerformanceLabels(envelopeGroups), [envelopeGroups]);
  const hit = useMemo(() => buildPerformanceHitIndex(groups), [groups]);
  const details = points.filter((point) => active.includes(point.key));
  const hiddenLabels = envelopeGroups.filter((group) => !labels.has(group.key)).length;
  const { width, height, left, right, top, bottom } = PERFORMANCE_CHART;
  const title = (point: PerformancePoint) => `${point.model} · ${point.providerName} · ${point.score}/100 · ${formatTps(point.tps)}${envelope.memberKeys.has(point.key) ? ` · ${membershipLabel}` : ""}`;
  const show = (x: number, y: number, element: SVGGElement) => {
    cancelClose();
    const rect = element.getBoundingClientRect();
    setPosition({ left: Math.max(8, Math.min(window.innerWidth - 328, rect.left)),
      top: Math.max(8, Math.min(window.innerHeight - 328, rect.bottom + 8)) });
    setActive(hit(x, y).map((point) => point.key));
  };
  return (
    <div className="relative min-w-0" onPointerLeave={closeSoon}
      onBlur={(event) => { if (!event.currentTarget.contains(event.relatedTarget as Node | null) && (event.relatedTarget as Element | null)?.id !== tooltipId) setActive([]); }}
      onKeyDown={(event) => { if (event.key === "Escape") { setActive([]); event.stopPropagation(); } }}>
      <div className="flex flex-wrap items-center gap-3 border-b border-slate-200 p-3 text-xs text-slate-600">
        <label className="flex items-center gap-2">{isZh ? "缩放" : "Zoom"}
          <select className="rounded border bg-white p-1" value={zoom} onChange={(event) => setZoom(Number(event.target.value))}>
            <option value={1}>100%</option><option value={1.5}>150%</option><option value={2}>200%</option>
          </select>
        </label>
        <span>{isZh ? "每个点显示该供应商的图标；虚线边框表示有效样本少于 3。悬停、聚焦或轻触模型查看详情，Esc 关闭。仅包络线上的点直接显示名称，其余点悬停、聚焦或轻触查看。" : "Each point shows its provider's icon; a dashed border means fewer than 3 valid samples. Hover, focus or tap a model for details; Escape dismisses. Only envelope points are labeled directly; hover, focus or tap any other icon for its name."}</span>
        <span className="inline-flex items-center gap-2" data-testid="performance-envelope-legend">
          <svg width="28" height="10" aria-hidden="true"><line x1="0" x2="28" y1="5" y2="5" stroke="#475569" strokeWidth="2" strokeDasharray="6 4" /></svg>
          {envelopeLabel}
        </span>
        <span className="w-full text-slate-500">{isZh ? "虚线仅表示当前观测值的外边界，线段中间不代表实际模型，也不保证性能稳定性。" : "The dashed line marks the boundary of current observations. Segment interiors are not actual models or a guarantee of stable performance."}</span>
      </div>
      <div className="overflow-auto" tabIndex={0} role="region" aria-label={isZh ? "能力与速度散点图，可滚动" : "Capability and speed scatter plot, scrollable"}>
        <svg viewBox={`0 0 ${width} ${height}`} style={{ width: `${zoom * 100}%`, minWidth: 660 * zoom, height: "auto" }}
          data-testid="performance-chart" data-x-min={xDomain.min} data-x-max={xDomain.max} data-y-max={yMax}
          data-point-left={xSpan.left} data-point-right={xSpan.right}
          data-envelope-member-keys={JSON.stringify([...envelope.memberKeys])}
          data-plot-left={left} data-plot-right={right} data-plot-top={top} data-plot-bottom={bottom}
          aria-label={isZh ? `评分 ${xDomain.min}–${xDomain.max} 与平均 TPS，仅包络线上的点显示模型名称` : `Score ${xDomain.min}–${xDomain.max} versus average TPS, envelope points labeled by model name`}
          onClick={(event) => { if (!(event.target as Element).closest('[role="button"]')) setActive([]); }}>
          <title>{isZh ? "按当前可见评分自动缩放的性能图" : "Performance chart automatically scaled to visible scores"}</title>
          <line data-testid="performance-axis" x1={left} x2={right} y1={bottom} y2={bottom} stroke="#94a3b8" vectorEffect="non-scaling-stroke" />
          <line data-testid="performance-axis" x1={left} x2={left} y1={top} y2={bottom} stroke="#94a3b8" vectorEffect="non-scaling-stroke" />
          {Array.from({ length: (xDomain.max - xDomain.min) / 10 + 1 }, (_, index) => xDomain.min + index * 10).map((tick) => {
            const x = scoreX(tick, xDomain);
            return <g key={tick}><line data-testid="performance-tick" x1={x} x2={x} y1={bottom} y2={bottom + 4} stroke="#94a3b8" vectorEffect="non-scaling-stroke" /><text x={x} y={bottom + 23} textAnchor="middle" fontSize={11} fill="#64748b">{tick}</text></g>;
          })}
          {Array.from({ length: 6 }, (_, index) => {
            const y = bottom - (bottom - top) * index / 5;
            return <g key={index}><line data-testid="performance-tick" x1={left - 4} x2={left} y1={y} y2={y} stroke="#94a3b8" vectorEffect="non-scaling-stroke" /><text x={left - 12} y={y + 4} textAnchor="end" fontSize={11} fill="#64748b">{new Intl.NumberFormat("en", { maximumFractionDigits: 1 }).format(yMax * index / 5)}</text></g>;
          })}
          <text x={(left + right) / 2} y={height - 14} textAnchor="middle" fontSize={12} fill="#64748b">{isZh ? `能力评分（${xDomain.min}–${xDomain.max}，满分 100）` : `Capability score (${xDomain.min}–${xDomain.max}, out of 100)`}</text>
          <text transform={`translate(16 ${(top + bottom) / 2}) rotate(-90)`} textAnchor="middle" fontSize={12} fill="#64748b">TPS (tok/s)</text>
          {envelope.nodes.length >= 2 && <polyline data-testid="performance-envelope" points={envelopePath}
            data-member-keys={JSON.stringify([...envelope.memberKeys])}
            fill="none" stroke="#475569" strokeWidth={2} strokeDasharray="6 4"
            vectorEffect="non-scaling-stroke" pointerEvents="none" aria-label={envelopeLabel} />}
          {groups.map((group) => {
            const label = labels.get(group.key), first = group.points[0];
            const members = group.points;
            const selected = members.some((point) => active.includes(point.key));
            const size = members.length > 1 ? PERFORMANCE_COINCIDENT_MARKER_SIZE : PERFORMANCE_MARKER_SIZE;
            const step = size + 2;
            return <g key={group.key} role="button" tabIndex={0} aria-label={members.map(title).join("; ")}
              aria-describedby={selected && details.length ? tooltipId : undefined}
              style={{ cursor: "pointer", opacity: details.length && !selected ? 0.35 : 1 }}
              onPointerEnter={(event) => show(group.x, group.y, event.currentTarget)} onPointerLeave={closeSoon}
              onFocus={(event) => show(group.x, group.y, event.currentTarget)} onBlur={closeSoon}
              onClick={(event) => show(group.x, group.y, event.currentTarget)} onKeyDown={(event) => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); show(group.x, group.y, event.currentTarget); } }}>
              <title>{members.map(title).join("\n")}</title>
              {label && <g data-testid="performance-label" data-point-ids={members.map((p) => p.pointId).join(",")}>
                <line data-testid="performance-label-leader" x1={group.x} y1={group.y} x2={label.x + label.width / 2} y2={label.y + label.height / 2} stroke={first.color} strokeOpacity={0.4} />
                <rect x={label.x} y={label.y} width={label.width} height={label.height} rx={3} fill="white" stroke={selected ? first.color : "#e2e8f0"} />
                <text x={label.x + 8} y={label.y + 16} fontSize={11} fontWeight={600} fill={first.color}>
                  {label.lines.map((line, index) => <tspan key={index} x={label.x + 8} dy={index ? 15 : 0}>{line}</tspan>)}
                </text>
              </g>}
              <circle cx={group.x} cy={group.y} r={14} fill="transparent" />
              {/* Invisible geometry carrier per point: exact coordinates, no painted dot. */}
              {members.map((point) => <circle key={point.key} data-testid="performance-point" data-point-ids={point.pointId}
                data-member-count={members.length} data-score={point.score} data-tps={point.tps}
                data-low-sample={point.validTpsCount < 3 ? "true" : "false"} cx={group.x} cy={group.y} r={size / 2}
                fill="none" stroke="none" pointerEvents="none" />)}
              {members.map((point, index) => <PerformancePointMarker key={point.pointId} point={point}
                x={group.x} y={group.y} offsetX={(index - (members.length - 1) / 2) * step} size={size}
                lowSample={point.validTpsCount < 3} selected={active.includes(point.key)} />)}
            </g>;
          })}
        </svg>
      </div>
      {!!hiddenLabels && <p className="p-3 text-xs text-amber-700" role="status">{isZh ? `${hiddenLabels} 个包络线位置无法容纳完整标签；悬停或聚焦图标查看。` : `${hiddenLabels} envelope positions cannot fit full labels; hover or focus their icons.`}</p>}
      {!!details.length && createPortal(<div id={tooltipId} role="tooltip" tabIndex={0} data-testid="performance-tooltip"
        onPointerEnter={cancelClose} onPointerLeave={closeSoon} onFocus={cancelClose}
        onBlur={() => setActive([])} onKeyDown={(event) => { if (event.key === "Escape") setActive([]); }}
        style={position} className="fixed z-50 max-h-80 w-80 max-w-[calc(100vw-1rem)] overflow-auto rounded-lg border border-slate-200 bg-white p-3 text-xs shadow-lg">
        {details.map((point) => <div key={point.key} data-point-id={point.pointId} className="space-y-1 border-b border-slate-100 py-2 last:border-0">
          <strong className="block whitespace-pre-wrap break-all" style={{ color: point.color }}>{point.model}</strong>
          <span className="block break-all">{point.providerName} · {point.providerId}</span>
          <span className="block font-semibold">{point.score}/100 · {formatTps(point.tps)}</span>
          {envelope.memberKeys.has(point.key) && <span data-testid="performance-envelope-member" data-point-key={point.key}
            className="block font-medium text-slate-600">{membershipLabel}</span>}
          <span className="block">{isZh ? `有效 TPS ${point.validTpsCount} / 已选请求 ${point.selectedRequestCount}` : `Valid TPS ${point.validTpsCount} / selected requests ${point.selectedRequestCount}`}</span>
          {point.validTpsCount < 3 && <span className="block text-amber-700">{isZh ? "低样本量" : "Low sample count"}</span>}
          <span className="block">{isZh ? "样本起始" : "First sample"}: {formatLocalDateTime(point.firstSampleAt)}</span>
          <span className="block">{isZh ? "样本结束" : "Last sample"}: {formatLocalDateTime(point.lastSampleAt)}</span>
          <span className="block">{isZh ? "评分更新" : "Score updated"}: {formatLocalDateTime(point.scoreUpdatedAt)}</span>
          {!point.providerEnabled && <span className="block text-amber-700">{isZh ? "供应商已禁用或未知" : "Provider disabled or unknown"}</span>}
        </div>)}
      </div>, document.body)}
    </div>
  );
}
