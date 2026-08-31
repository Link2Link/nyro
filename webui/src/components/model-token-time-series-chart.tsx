import { useMemo, useState } from "react";
import {
  CartesianGrid,
  Line,
  LineChart,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts";
import {
  formatLocalBucketLabel,
  formatLocalBucketRange,
  formatTokenCount,
} from "@/lib/format";
import type { ApiKeyModelTimeSeries } from "@/lib/types";

export interface ModelTokenTimeSeriesChartProps {
  /** Per-model series over a shared window; sorted by token volume desc. */
  series: ApiKeyModelTimeSeries[];
  zh: boolean;
  /** Prefix MM/DD on axis ticks; used when the range spans more than a day. */
  showDateOnAxis?: boolean;
}

const COLORS = [
  "#3b82f6",
  "#10b981",
  "#f59e0b",
  "#ef4444",
  "#8b5cf6",
  "#ec4899",
  "#06b6d4",
  "#84cc16",
];
const MAX_SERIES = 8;

type Metric = "total" | "input" | "cache" | "output";

const METRICS: { value: Metric; zh: string; en: string }[] = [
  { value: "total", zh: "总计", en: "Total" },
  { value: "input", zh: "输入", en: "Input" },
  { value: "cache", zh: "缓存", en: "Cache" },
  { value: "output", zh: "输出", en: "Output" },
];

const metricValue = (
  metric: Metric,
  point: { total_input_tokens: number; total_output_tokens: number; total_cache_read_tokens: number },
): number => {
  switch (metric) {
    case "input":
      return point.total_input_tokens;
    case "cache":
      return point.total_cache_read_tokens;
    case "output":
      return point.total_output_tokens;
    // Input already includes cache; total = input + output is the safe
    // upper bound that never double-counts cache reads.
    case "total":
    default:
      return point.total_input_tokens + point.total_output_tokens;
  }
};

function tooltipFormatter(value: number | string, name: string): [string, string] {
  return [formatTokenCount(Number(value)), name];
}

/**
 * Multi-model token time-series chart: one line per upstream model sharing a
 * common Y axis, with a token-metric switch. Top N models by token volume
 * are drawn; the rest are summarized in the footer note.
 */
export function ModelTokenTimeSeriesChart({
  series,
  zh,
  showDateOnAxis = false,
}: ModelTokenTimeSeriesChartProps) {
  const [metric, setMetric] = useState<Metric>("total");

  const visible = series.slice(0, MAX_SERIES);
  const hiddenCount = series.length - visible.length;

  const data = useMemo(() => {
    // Bucket starts are identical across models (same window + bucket width)
    // and fill_time_buckets guarantees zero-filled, evenly spaced points, so
    // the first series defines the shared x axis and the rest merge by key.
    const byTimestamp = new Map<number, Record<string, number>>();
    for (const item of visible) {
      for (const point of item.series.points) {
        const row = byTimestamp.get(point.bucket_start) ?? { timestamp: point.bucket_start };
        row[item.upstream_model] = metricValue(metric, point);
        byTimestamp.set(point.bucket_start, row);
      }
    }
    return [...byTimestamp.values()].sort((a, b) => a.timestamp - b.timestamp);
  }, [visible, metric]);

  if (series.length === 0) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-slate-400">
        {zh ? "暂无数据" : "No data"}
      </div>
    );
  }

  const reference = visible[0].series;

  return (
    <div className="flex h-full flex-col">
      <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
        <div className="flex flex-wrap gap-1.5">
          {METRICS.map((item) => (
            <button
              key={item.value}
              type="button"
              onClick={() => setMetric(item.value)}
              className={
                metric === item.value
                  ? "rounded-md bg-blue-600 px-2 py-0.5 text-xs font-medium text-white"
                  : "rounded-md px-2 py-0.5 text-xs font-medium text-slate-500 hover:bg-slate-100"
              }
            >
              {zh ? item.zh : item.en}
            </button>
          ))}
        </div>
        <span className="shrink-0 text-xs text-slate-400">
          {hiddenCount > 0
            ? zh
              ? `+${hiddenCount} 个模型未显示`
              : `+${hiddenCount} more model${hiddenCount === 1 ? "" : "s"}`
            : ""}
        </span>
      </div>
      <div className="min-h-0 flex-1">
        <ResponsiveContainer width="100%" height="100%">
          <LineChart data={data}>
            <CartesianGrid strokeDasharray="3 3" vertical={false} stroke="#e2e8f0" />
            <XAxis
              dataKey="timestamp"
              type="number"
              scale="time"
              domain={["dataMin", "dataMax"]}
              interval="preserveStartEnd"
              minTickGap={28}
              tick={{ fill: "#64748b", fontSize: 11 }}
              tickFormatter={(value) => formatLocalBucketLabel(value, showDateOnAxis)}
              axisLine={false}
              tickLine={false}
            />
            <YAxis
              tick={{ fill: "#64748b", fontSize: 11 }}
              axisLine={false}
              tickLine={false}
              width={50}
              tickFormatter={formatTokenCount}
            />
            <Tooltip
              formatter={tooltipFormatter}
              labelFormatter={(value) =>
                formatLocalBucketRange(
                  Number(value),
                  reference.bucket_minutes,
                  reference.start_at,
                  reference.end_at,
                )
              }
            />
            {visible.map((item, index) => (
              <Line
                key={item.upstream_model}
                type="monotone"
                dataKey={item.upstream_model}
                name={item.upstream_model}
                stroke={COLORS[index % COLORS.length]}
                strokeWidth={2}
                dot={false}
              />
            ))}
          </LineChart>
        </ResponsiveContainer>
      </div>
    </div>
  );
}
