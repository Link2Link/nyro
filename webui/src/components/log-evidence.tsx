import { useEffect, useState } from "react";
import { Check, Copy } from "lucide-react";
import type { RequestLog, RequestLogAttempts } from "@/lib/types";
import { outcomeLabel, parseErrorCauses, payloadEvidence, payloadStateLabel, type PayloadField } from "@/lib/log-observability";
import { formatLogTime } from "@/lib/format";
import { copyToClipboard } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { ResultBadge } from "@/components/log-outcome";

export function AttemptResultBanner({ log, correlation, correlationLoading, correlationError, onSelect, isZh }: {
  log: RequestLog;
  correlation?: RequestLogAttempts;
  correlationLoading: boolean;
  correlationError: boolean;
  onSelect: (log: RequestLog) => void;
  isZh: boolean;
}) {
  const evidence = parseErrorCauses(log.error_causes);
  const result = correlation ? correlation.result : log.request_result;
  return (
    <section className="space-y-2 rounded-lg border border-slate-200 bg-slate-50 p-3 text-xs text-slate-600">
      <div className="flex flex-wrap items-center gap-2"><strong>{isZh ? "本次尝试结果" : "Attempt result"}</strong><ResultBadge log={log} isZh={isZh} /><span>{isZh ? "权威完成状态" : "Authoritative completion"}: {outcomeLabel(log.attempt_outcome ?? "unknown", isZh)} · v{log.outcome_version ?? 0}</span></div>
      <p>{isZh ? "原始 HTTP 状态" : "Actual HTTP status"}: {isZh ? "客户端" : "client"} {log.client_status_code ?? "–"} / {isZh ? "上游" : "upstream"} {log.upstream_status_code ?? "–"}</p>
      {log.failure_stage && <p>{isZh ? "失败阶段" : "Failure stage"}: <code>{log.failure_stage}</code></p>}
      {log.failure_kind && <p>{isZh ? "失败类型" : "Failure kind"}: <code>{log.failure_kind}</code></p>}
      {log.error_message && <p className="whitespace-pre-wrap break-all text-red-700">{log.error_message}</p>}
      {evidence.causes.length > 0 && <div><strong>{isZh ? "原因链" : "Error causes"}</strong>{evidence.malformed && <p className="text-amber-700">{isZh ? "原因 JSON 无效；以下为原始文本。" : "Malformed causes JSON; raw text follows."}</p>}<ol className="list-decimal space-y-1 pl-5">{evidence.causes.map((cause, index) => <li key={index} className="whitespace-pre-wrap break-all">{cause}</li>)}</ol></div>}
      <p className="text-slate-500">{isZh ? "HTTP 200 不代表完整成功；未知历史结果不推断为错误。" : "HTTP 200 does not establish full success; unknown historical results are not inferred errors."}</p>
      {log.client_request_id ? <div className="space-y-2 border-t border-slate-200 pt-2">
        <p>{isZh ? "关联客户端请求 ID" : "Correlated client request ID"}: <code className="select-all break-all">{log.client_request_id}</code> · {isZh ? "尝试序号" : "Attempt index"}: {log.attempt_index ?? "–"}</p>
        {result ? <div className="space-y-1">
          <p><strong>{isZh ? "最终客户端结果（摘要，不计作额外尝试）" : "Final client result (summary, not an additional attempt)"}</strong>: {outcomeLabel(result.final_outcome, isZh)}</p>
          <p>{isZh ? "尝试总数" : "Attempt count"}: {result.attempt_count} · {isZh ? "完成于" : "Finished"}: {formatLogTime(result.finished_at)}</p>
          <p>{isZh ? "最终尝试 ID" : "Final attempt ID"}: <code className="select-all break-all">{result.final_attempt_id ?? "–"}</code></p>
        </div> : <p>{isZh ? "最终结果摘要尚不可用。" : "Final result summary is not available."}</p>}
        {correlationLoading && <p role="status">{isZh ? "正在加载关联尝试…" : "Loading correlated attempts…"}</p>}
        {correlationError && <p role="alert" className="text-amber-700">{isZh ? "关联尝试不可用；不代表没有其他尝试。" : "Correlated attempts unavailable; this does not mean no other attempts exist."}</p>}
        {correlation && <div className="space-y-1">
          <strong>{isZh ? "此请求的已保存尝试" : "Stored attempts for this request"}</strong>
          {correlation.attempts.length === 0 && <p>{isZh ? "没有可用的已保存尝试（可能已删除或丢失）。" : "No stored attempts available (they may have been deleted or lost)."}</p>}
          {correlation.attempts.map((attempt) => <button type="button" key={attempt.id} onClick={() => onSelect(attempt)} className={`flex w-full flex-wrap items-center gap-2 rounded border p-2 text-left hover:bg-white ${attempt.id === log.id ? "border-sky-300" : "border-slate-200"}`}>
            <span>#{attempt.attempt_index ?? "–"}</span><code className="break-all">{attempt.id}</code><span>{attempt.provider_name ?? attempt.provider_id} · {attempt.upstream_model}</span><span>HTTP {attempt.client_status_code ?? "–"} / {attempt.upstream_status_code ?? "–"}</span><ResultBadge log={attempt} isZh={isZh} />{attempt.id === result?.final_attempt_id && <span>{isZh ? "最终尝试" : "Final attempt"}</span>}
          </button>)}
        </div>}
      </div> : <p>{isZh ? "无关联请求 ID（历史记录或不可用）。" : "No correlation ID (historical or unavailable)."}</p>}
    </section>
  );
}

export function PayloadBlock({ title, log, field, isZh }: { title: string; log: Partial<RequestLog> | null; field: PayloadField; isZh: boolean }) {
  const [copied, setCopied] = useState(false);
  const [collapsed, setCollapsed] = useState(true);
  const evidence = payloadEvidence(log ?? {}, field);
  const metadata = evidence.metadata;
  const hasContent = evidence.segments.some((part) => part.text.length > 0);
  const pretty = evidence.segments.map((part) => `${part.label} (${part.encoding})\n${part.text}`).join(`\n\n[${evidence.missingBytes ?? "unknown"} observed bytes omitted]\n\n`);
  useEffect(() => {
    if (!copied) return;
    const timeout = window.setTimeout(() => setCopied(false), 1500);
    return () => window.clearTimeout(timeout);
  }, [copied]);
  return <div className="rounded-lg border border-slate-200 bg-slate-50/60">
    <div className="flex items-center justify-between border-b border-slate-200 px-3 py-1.5">
      <button type="button" className="text-left text-xs font-medium text-slate-600" onClick={() => setCollapsed((value) => !value)} aria-expanded={!collapsed}>{title}</button>
      <Button type="button" size="sm" variant="ghost" disabled={!hasContent} onClick={async () => setCopied(await copyToClipboard(pretty))} className="h-7 gap-1 px-2 text-xs">{copied ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />}{copied ? (isZh ? "已复制" : "Copied") : (isZh ? "复制" : "Copy")}</Button>
    </div>
    <div className="space-y-1 px-3 py-2 text-[11px] text-slate-500">
      <p>{payloadStateLabel(evidence.state, isZh)}{log?.payload_cleared_at != null && ` · ${formatLogTime(log.payload_cleared_at)}`}</p>
      {metadata && <p>{isZh ? "已观测" : "Observed"} {metadata.total_observed_bytes} B · {isZh ? "保留" : "Retained"} {metadata.retained_bytes} B · {metadata.encoding} · {metadata.complete ? (isZh ? "传输采集结束" : "Capture complete") : (isZh ? "传输采集不完整，最终大小未知" : "Capture incomplete; final size unknown")} · {metadata.truncated ? (isZh ? "已截断" : "Truncated") : (isZh ? "未截断" : "Not truncated")}</p>}
      {metadata?.omitted_headers != null && <p>{isZh ? "省略请求头" : "Omitted headers"}: {metadata.omitted_headers}</p>}
      {evidence.missingBytes != null && evidence.missingBytes > 0 && <p className="text-amber-700">{isZh ? `中间缺失 ${evidence.missingBytes} 个已观测字节；下方头尾片段不连续。` : `${evidence.missingBytes} observed bytes missing from the middle; head and tail below are not contiguous.`}</p>}
      {evidence.issue && <p role="alert" className="text-amber-700">{isZh ? "元数据或存储内容不一致；显示原始文本，不进行 JSON 格式化。" : evidence.issue}</p>}
    </div>
    {collapsed ? <button type="button" onClick={() => setCollapsed(false)} className="px-3 pb-2 text-[11px] text-slate-500">{isZh ? "点击展开" : "Click to expand"}</button> : <div className="space-y-2 px-3 pb-2">
      {evidence.segments.map((part) => <div key={part.label}><p className="text-[11px] font-semibold text-slate-500">{part.label === "head" ? (isZh ? "头部" : "HEAD") : part.label === "tail" ? (isZh ? "尾部（与头部不连续）" : "TAIL (not contiguous with head)") : (isZh ? "存储内容" : "Stored content")} · {part.encoding === "base64" ? (isZh ? "Base64 原始字节（不解析 JSON）" : "Base64 raw bytes (not parsed as JSON)") : part.encoding === "unverified" ? (isZh ? "原始存储文本（编码未经验证）" : "Raw stored text (encoding unverified)") : "UTF-8"}</p><pre className="max-h-80 overflow-auto whitespace-pre-wrap break-all font-mono text-[11px] leading-relaxed text-slate-700">{part.text}</pre></div>)}
      {evidence.segments.length === 0 && <p className="text-[11px] text-slate-400">{payloadStateLabel(evidence.state, isZh)}</p>}
    </div>}
  </div>;
}
