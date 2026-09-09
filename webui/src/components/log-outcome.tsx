import type { LogQuery, RequestLog } from "@/lib/types";
import { EFFECTIVE_OUTCOMES, effectiveOutcome, outcomeFilterValue, outcomeLabel, outcomeQuery } from "@/lib/log-observability";
import { Badge } from "@/components/ui/badge";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";

export function ResultBadge({ log, isZh = false }: { log: Partial<RequestLog>; isZh?: boolean }) {
  const outcome = effectiveOutcome(log);
  const colors = {
    error: "border-red-200 bg-red-50 text-red-700", completed: "border-green-200 bg-green-50 text-green-700",
    cancelled: "border-amber-200 bg-amber-50 text-amber-700", output_limited: "border-orange-200 bg-orange-50 text-orange-700",
    unknown: "border-slate-200 bg-slate-50 text-slate-500",
  };
  return <Badge variant="outline" className={`text-[10px] ${colors[outcome]}`} title={isZh ? "核心判定的尝试结果；独立于 HTTP 状态" : "Core-derived attempt result; independent of HTTP status"}>{outcomeLabel(outcome, isZh)}</Badge>;
}

export function OutcomeFilter({ value, onChange, isZh = false }: { value: LogQuery; onChange: (next: LogQuery) => void; isZh?: boolean }) {
  return (
    <Select value={outcomeFilterValue(value)} onValueChange={(next) => onChange({ ...value, ...outcomeQuery(next) })}>
      <SelectTrigger className="h-9 w-44 text-xs" aria-label={isZh ? "尝试结果过滤" : "Attempt result filter"}><SelectValue /></SelectTrigger>
      <SelectContent>
        <SelectItem value="all">{isZh ? "全部尝试结果" : "All attempt results"}</SelectItem>
        {EFFECTIVE_OUTCOMES.map((outcome) => <SelectItem key={outcome} value={outcome}>{outcomeLabel(outcome, isZh)}</SelectItem>)}
      </SelectContent>
    </Select>
  );
}
