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
import type { StatsTimeSeries } from "@/lib/types";

export interface TokenTimeSeriesChartProps {
  /** Backend time series; renders the "No data" state when absent or empty. */
  series: StatsTimeSeries | null | undefined;
  zh: boolean;
  /** Prefix MM/DD on axis ticks; used when the range spans more than a day. */
  showDateOnAxis?: boolean;
}

// Chart tooltip numbers use the same K/M compaction as the tables.
function tooltipFormatter(value: number | string, name: string): [string, string] {
  return [formatTokenCount(Number(value)), name];
}

/**
 * Token time-series line chart (input / cache / output) over adaptive buckets.
 * Shared by the Stats page card and the model usage detail dialog.
 */
export function TokenTimeSeriesChart({
  series,
  zh,
  showDateOnAxis = false,
}: TokenTimeSeriesChartProps) {
  if (!series || !series.has_data) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-slate-400">
        {zh ? "暂无数据" : "No data"}
      </div>
    );
  }

  const data = series.points.map((point) => ({
    timestamp: point.bucket_start,
    input: point.total_input_tokens,
    cache: point.total_cache_read_tokens,
    output: point.total_output_tokens,
  }));

  return (
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
              series.bucket_minutes,
              series.start_at,
              series.end_at,
            )
          }
        />
        <Line
          type="monotone"
          dataKey="input"
          name={zh ? "输入" : "Input"}
          stroke="#3b82f6"
          strokeWidth={2}
          dot={false}
        />
        <Line
          type="monotone"
          dataKey="cache"
          name={zh ? "缓存命中" : "Cache"}
          stroke="#f59e0b"
          strokeWidth={2}
          dot={false}
        />
        <Line
          type="monotone"
          dataKey="output"
          name={zh ? "输出" : "Output"}
          stroke="#10b981"
          strokeWidth={2}
          dot={false}
        />
      </LineChart>
    </ResponsiveContainer>
  );
}
