import { useState } from "react";
import { Clock3 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { useLocale } from "@/lib/i18n";
import {
  MAX_CUSTOM_DAYS, PRESET_HOURS, fromDatetimeLocal, rangeStart, toDatetimeLocal, validateCustomRange,
  type StatsRange, type StatsRangeError,
} from "@/lib/stats-range";
import { cn } from "@/lib/utils";

const PRESET_LABELS: Record<number, [string, string]> = {
  6: ["最近 6 小时", "Last 6h"],
  24: ["最近 24 小时", "Last 24h"],
  72: ["最近 3 天", "Last 3d"],
  168: ["最近 7 天", "Last 7d"],
  720: ["最近 30 天", "Last 30d"],
};

function errorText(code: StatsRangeError, zh: boolean): string {
  switch (code) {
    case "start_required": return zh ? "请选择开始时间" : "Choose a start time";
    case "end_invalid": return zh ? "结束时间无效" : "The end time is invalid";
    case "start_future": return zh ? "开始时间不能晚于当前时间" : "The start time cannot be in the future";
    case "order": return zh ? "开始时间必须早于结束时间" : "The start time must be before the end time";
    case "too_long": return zh ? `时间跨度不能超过 ${MAX_CUSTOM_DAYS} 天` : `The range cannot exceed ${MAX_CUSTOM_DAYS} days`;
  }
}

/**
 * Range selector shared by the statistics page and the usage dialogs:
 * rolling presets, month-to-date (local time), and a custom start/end range.
 * A custom range only takes effect on "Apply"; an empty end means "until now".
 */
export function StatsRangePicker({ value, onChange, triggerClassName, className, showIcon = false }: {
  value: StatsRange;
  onChange: (range: StatsRange) => void;
  triggerClassName?: string;
  className?: string;
  showIcon?: boolean;
}) {
  const { locale } = useLocale();
  const zh = locale === "zh-CN";
  const [editing, setEditing] = useState(false);
  const [startText, setStartText] = useState(() => toDatetimeLocal(rangeStart(value)));
  const [endText, setEndText] = useState(() => value.kind === "custom" && value.endAt != null ? toDatetimeLocal(value.endAt) : "");
  const [error, setError] = useState<StatsRangeError | null>(null);
  const showCustom = value.kind === "custom" || editing;
  const selectValue = showCustom ? "custom" : value.kind === "month" ? "month" : String(value.hours);

  function select(next: string) {
    setError(null);
    if (next === "custom") {
      setStartText(toDatetimeLocal(rangeStart(value)));
      setEndText(value.kind === "custom" && value.endAt != null ? toDatetimeLocal(value.endAt) : "");
      setEditing(true);
      return;
    }
    setEditing(false);
    onChange(next === "month" ? { kind: "month" } : { kind: "hours", hours: Number(next) });
  }

  function apply() {
    const startAt = fromDatetimeLocal(startText);
    const endAt = fromDatetimeLocal(endText);
    const code = endText.trim() && endAt === null ? "end_invalid" : validateCustomRange(startAt, endAt);
    setError(code);
    if (code === null && startAt !== null) onChange({ kind: "custom", startAt, endAt });
  }

  const monthHint = zh ? "本月 1 日 00:00（本地时区）至今" : "From 00:00 on the 1st (local time) until now";
  const customHint = zh
    ? `本地时区；结束时间留空表示至今（持续更新）；最长 ${MAX_CUSTOM_DAYS} 天`
    : `Local time zone. Empty end = until now (keeps updating). Up to ${MAX_CUSTOM_DAYS} days.`;
  const bareInput = "!h-8 !w-[172px] !border-0 !bg-transparent !px-1.5 !py-0 !text-xs !shadow-none focus-visible:!ring-0";

  return (
    <div className={cn("flex flex-wrap items-center justify-end gap-2", className)}>
      {showCustom && (
        <>
          <div
            title={customHint}
            className="flex h-9 items-center gap-1 rounded-md border border-border bg-background px-1.5 focus-within:ring-2 focus-within:ring-slate-300"
          >
            <Input type="datetime-local" className={bareInput} value={startText}
              onChange={(event) => setStartText(event.target.value)} aria-label={zh ? "开始时间" : "Start time"} />
            <span className="text-xs text-slate-400" aria-hidden>→</span>
            <Input type="datetime-local" className={bareInput} value={endText}
              onChange={(event) => setEndText(event.target.value)}
              aria-label={zh ? "结束时间（留空表示至今）" : "End time (empty means until now)"} />
          </div>
          <Button className="h-9" onClick={apply}>{zh ? "应用" : "Apply"}</Button>
        </>
      )}
      <Select value={selectValue} onValueChange={select}>
        <SelectTrigger
          className={cn("w-40", triggerClassName)}
          aria-label={zh ? "时间范围" : "Time range"}
          title={!showCustom && value.kind === "month" ? monthHint : undefined}
        >
          {showIcon && <Clock3 className="mr-1 h-3.5 w-3.5" />}
          <SelectValue placeholder={zh ? "选择时间范围" : "Select range"} />
        </SelectTrigger>
        <SelectContent>
          {PRESET_HOURS.map((hours) => <SelectItem key={hours} value={String(hours)}>{PRESET_LABELS[hours][zh ? 0 : 1]}</SelectItem>)}
          <SelectItem value="month">{zh ? "本月" : "This month"}</SelectItem>
          <SelectItem value="custom">{zh ? "自定义…" : "Custom…"}</SelectItem>
        </SelectContent>
      </Select>
      {/* w-0 + min-w-full: wraps onto its own row without widening the picker */}
      {error && <p role="alert" className="w-0 min-w-full text-right text-xs text-red-600">{errorText(error, zh)}</p>}
    </div>
  );
}
