import { useId } from "react";
import {
  Area, AreaChart, CartesianGrid, ResponsiveContainer, Tooltip, XAxis, YAxis,
} from "recharts";
import { formatBytes, formatPercent, type MonitorPoint } from "@/lib/system-monitor";

type Series = { key: "cpu" | "memory" | "receive" | "transmit" | "latency"; label: string; color: string };

export function SystemMonitorChart({
  history, series, unit, locale, emptyLabel,
}: {
  history: MonitorPoint[];
  series: Series[];
  unit: "percent" | "bytes" | "ms";
  locale: string;
  emptyLabel: string;
}) {
  const id = useId().replace(/:/g, "");
  const formatValue = (value: number) => unit === "percent" ? formatPercent(value)
    : unit === "bytes" ? formatBytes(value, true) : `${value.toFixed(1)} ms`;
  const formatTime = (value: number) => new Date(value).toLocaleTimeString(locale, {
    hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false,
  });
  const hasData = history.some((point) => series.some((item) => point[item.key] != null));

  return (
    <div className="relative h-52 min-w-0" role="img" aria-label={series.map((item) => item.label).join(" / ")}>
      {!hasData ? <div className="flex h-full items-center justify-center text-xs text-muted-foreground">{emptyLabel}</div>
        : <ResponsiveContainer width="100%" height="100%">
          <AreaChart data={history} margin={{ top: 12, right: 8, left: 0, bottom: 0 }} accessibilityLayer>
            <defs>{series.map((item) => <linearGradient key={item.key} id={`${id}-${item.key}`} x1="0" y1="0" x2="0" y2="1">
              <stop offset="0%" stopColor={item.color} stopOpacity={0.2} />
              <stop offset="100%" stopColor={item.color} stopOpacity={0.01} />
            </linearGradient>)}</defs>
            <CartesianGrid vertical={false} stroke="var(--monitor-grid)" strokeDasharray="3 5" />
            <XAxis dataKey="timestamp" type="number" scale="time" domain={["dataMin", "dataMax"]}
              tickFormatter={formatTime} minTickGap={45} axisLine={false} tickLine={false}
              tick={{ fill: "var(--monitor-muted)", fontSize: 10 }} dy={8} />
            <YAxis domain={unit === "percent" ? [0, 100] : [0, "auto"]} width={unit === "bytes" ? 78 : 48}
              tickFormatter={(value: number) => unit === "percent" ? `${value}%` : unit === "bytes" ? formatBytes(value, true) : `${value} ms`}
              axisLine={false} tickLine={false} tick={{ fill: "var(--monitor-muted)", fontSize: 10 }} tickCount={4} />
            <Tooltip labelFormatter={(value) => formatTime(Number(value))}
              formatter={(value: number, name: string) => [formatValue(value), name]}
              contentStyle={{ background: "var(--color-popover)", color: "var(--color-popover-foreground)", border: "1px solid var(--monitor-grid)", borderRadius: 12, fontSize: 12 }}
              labelStyle={{ color: "var(--monitor-muted)", marginBottom: 4 }} />
            {series.map((item) => <Area key={item.key} dataKey={item.key} name={item.label}
              type="linear" stroke={item.color} strokeWidth={2} fill={`url(#${id}-${item.key})`}
              isAnimationActive={false} connectNulls={false}
              dot={history.filter((point) => point[item.key] != null).length === 1 ? { r: 3 } : false}
              activeDot={{ r: 4, strokeWidth: 2 }} />)}
          </AreaChart>
        </ResponsiveContainer>}
    </div>
  );
}
