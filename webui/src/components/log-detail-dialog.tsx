import { useQuery } from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useState } from "react";
import { Check, Copy, Download, Loader2, Trash2 } from "lucide-react";

import { backend } from "@/lib/backend";
import { useLocale } from "@/lib/i18n";
import type { Provider, RequestLog, RequestLogAttempts } from "@/lib/types";
import { computeTps, formatDuration, formatLogTime, formatTokenCount, formatTps, generationMsOf } from "@/lib/format";
import { effectiveOutcome, payloadDownload } from "@/lib/log-observability";
import { ResultBadge } from "@/components/log-outcome";
import { AttemptResultBanner, PayloadBlock } from "@/components/log-evidence";
import { prettyName } from "@/lib/protocol";
import { copyToClipboard } from "@/lib/utils";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";

function protocolLabel(raw: string | null | undefined): string {
  return prettyName(raw) ?? raw ?? "–";
}

interface LogDetailDialogProps {
  logId: string | null;
  summary?: RequestLog | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** When provided, renders a per-row delete button next to Download. */
  onDelete?: (id: string) => void;
}

export function LogDetailDialog(props: LogDetailDialogProps) {
  // A fresh navigation scope when a different source row opens; correlation navigation stays internal.
  return <LogDetailContent key={`${props.logId}:${props.open}`} {...props} />;
}

function LogDetailContent({ logId, summary, open, onOpenChange, onDelete }: LogDetailDialogProps) {
  const [attemptSelection, setAttemptSelection] = useState<RequestLog | null>(null);
  const selectedId = attemptSelection?.id ?? logId;
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";

  const { data, isLoading, error } = useQuery<RequestLog | null>({
    queryKey: ["log-detail", selectedId],
    queryFn: () => backend("get_log", { id: selectedId! }),
    enabled: open && !!selectedId,
  });
  const log = data === undefined ? attemptSelection ?? summary ?? null : data;
  const correlation = useQuery<RequestLogAttempts>({
    queryKey: ["request-log-attempts", data?.client_request_id],
    queryFn: () => backend("get_request_log_attempts", { requestId: data!.client_request_id }),
    enabled: open && !!data?.client_request_id,
    retry: false,
  });

  // Shared ["providers"] cache with the logs page. Route-decision snapshots
  // store opaque provider ids; older rows have no embedded name, so resolve
  // id → display name here for readable labels.
  const { data: providers = [] } = useQuery<Provider[]>({
    queryKey: ["providers"],
    queryFn: () => backend("get_providers"),
    enabled: open,
  });
  const providerNameById = useMemo(
    () => new Map(providers.map((p) => [p.id, p.name])),
    [providers],
  );

  const [downloaded, setDownloaded] = useState(false);
  const [idCopied, setIdCopied] = useState(false);
  const idCodeRef = useRef<HTMLElement | null>(null);

  useEffect(() => {
    if (!idCopied) return;
    const t = window.setTimeout(() => setIdCopied(false), 1500);
    return () => window.clearTimeout(t);
  }, [idCopied]);

  const handleCopyId = async () => {
    const id = log?.id ?? selectedId;
    if (!id) return;
    setIdCopied(await copyToClipboard(id));
  };

  // Click-to-select-all on the visible ID — the manual-copy fallback that
  // works even when every programmatic clipboard path is unavailable
  // (insecure HTTP contexts, locked-down webviews).
  const handleSelectId = () => {
    const el = idCodeRef.current;
    if (!el) return;
    const selection = window.getSelection();
    if (!selection) return;
    const range = document.createRange();
    range.selectNodeContents(el);
    selection.removeAllRanges();
    selection.addRange(range);
  };

  const method = log?.method ?? "–";
  const path = log?.path ?? "–";
  const clientStatus = log?.client_status_code;
  // is_stream is the canonical flag (declared by the client). Fall back to
  // stream_chunks_count for older log rows that pre-date the field.
  const isStream = log?.is_stream ?? (log?.stream_chunks_count ?? 0) > 0;

  const generationMs = generationMsOf(log);
  const tps = computeTps(log);
  const isCrossProtocol =
    log?.client_protocol &&
    log?.upstream_protocol &&
    log.client_protocol !== log.upstream_protocol;

  useEffect(() => {
    if (!downloaded) return;
    const t = window.setTimeout(() => setDownloaded(false), 1500);
    return () => window.clearTimeout(t);
  }, [downloaded]);

  const handleDownload = () => {
    if (!log) return;
    const ts = formatLogTime(log.created_at);
    const proto = isCrossProtocol
      ? `${log.client_protocol ?? "–"} → ${log.upstream_protocol ?? "–"} (cross-protocol)`
      : (log.client_protocol ?? "–");
    const lines: string[] = [
      `# Nyro Request Log`,
      `# Attempt log ID: ${log.id}`,
      `# Client request ID: ${log.client_request_id ?? "unknown"}  Attempt index: ${log.attempt_index ?? "unknown"}`,
      `# Effective result: ${effectiveOutcome(log)}  Attempt outcome: ${log.attempt_outcome ?? "unknown"}  Version: ${log.outcome_version ?? 0}`,
      `# Failure kind: ${log.failure_kind ?? "–"}  Stage: ${log.failure_stage ?? "–"}`,
      `# Error summary: ${log.error_message ?? "–"}`,
      `# Error causes (raw JSON): ${log.error_causes ?? "–"}`,
      `# Final client result (not another attempt): ${JSON.stringify(correlation.data ? correlation.data.result : log.request_result ?? null)}`,
      `# Time: ${ts}`,
      `# Method: ${method}  Path: ${path}`,
      `# Client Status: ${log.client_status_code ?? "–"}  Upstream Status: ${log.upstream_status_code ?? "–"}`,
      `# Latency Total: ${formatDuration(log.latency_total_ms)}  Upstream: ${formatDuration(log.latency_upstream_ms)}`,
      `# TPS: ${tps != null ? formatTps(tps) : "–"}  (gen ${formatDuration(generationMs)})`,
      `# Provider: ${log.provider_name ?? log.provider_id ?? "–"}  Model: ${log.model_name ?? log.model_id ?? "–"}  ApiKey: ${log.api_key_name ?? log.api_key_id ?? "–"}`,
      `# Client Model: ${log.client_model ?? "–"}  Upstream Model: ${log.upstream_model ?? "–"}`,
      `# Reasoning Effort: ${log.reasoning_effort ?? "–"}`,
      `# Protocol: ${proto}`,
      `# Tokens: IN=${log.input_tokens} OUT=${log.output_tokens}`,
      isStream ? `# Stream: chunks=${log.stream_chunks_count} ttfb=${log.stream_first_chunk_ms ?? "–"}ms` : `# Stream: false`,
      "",
      payloadDownload(log),
    ];
    const blob = new Blob([lines.join("\n")], { type: "text/plain" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `nyro-log-${log.id}.log`;
    a.click();
    URL.revokeObjectURL(url);
    setDownloaded(true);
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        className="w-[min(92vw,960px)] max-h-[88vh] overflow-hidden flex flex-col gap-4"
        onOpenAutoFocus={(e) => e.preventDefault()}
      >
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <span>{isZh ? "请求详情" : "Request Detail"}</span>
            {isLoading ? <Loader2 className="h-3.5 w-3.5 animate-spin text-slate-400" /> : null}
          </DialogTitle>
          <DialogDescription className="flex flex-wrap items-center gap-2">
            <span>{log ? formatLogTime(log.created_at) : ""}</span>
            {(log?.id ?? selectedId) ? (
              <span className="inline-flex max-w-full items-center gap-1">
                <code
                  ref={idCodeRef}
                  title={isZh
                    ? "日志 ID（唯一）。点击全选，或手动选中复制。"
                    : "Log ID (unique). Click to select all, or select & copy manually."}
                  onClick={handleSelectId}
                  className="cursor-pointer select-all rounded border border-slate-200 bg-slate-50 px-1.5 py-0.5 font-mono text-[11px] text-slate-600 transition-colors hover:border-slate-300 hover:text-slate-800 break-all"
                >
                  {log?.id ?? selectedId}
                </code>
                <button
                  type="button"
                  onClick={handleCopyId}
                  title={isZh ? "复制完整日志 ID" : "Copy the full log ID"}
                  className="inline-flex shrink-0 items-center rounded p-0.5 text-slate-400 transition-colors hover:text-slate-600"
                >
                  {idCopied ? (
                    <Check className="h-3 w-3 text-green-600" />
                  ) : (
                    <Copy className="h-3 w-3" />
                  )}
                </button>
              </span>
            ) : null}
          </DialogDescription>
        </DialogHeader>

        <div className="flex flex-wrap items-center gap-2 text-xs">
          <Badge variant="outline" className="font-mono">{method}</Badge>
          <span className="font-mono text-slate-600 break-all">{path}</span>
          <span className="font-mono text-slate-600">HTTP {clientStatus ?? "–"}</span>
          {log && <ResultBadge log={log} isZh={isZh} />}
          {isStream ? (
            <Badge variant="outline" className="border-green-200 bg-green-50 text-green-700">SSE</Badge>
          ) : (
            <Badge variant="outline" className="border-sky-200 bg-sky-50 text-sky-700">JSON</Badge>
          )}
          {isCrossProtocol ? (
            <Badge variant="outline" className="border-purple-200 bg-purple-50 text-purple-700">
              {isZh ? "跨协议" : "Cross-Protocol"}
            </Badge>
          ) : null}
          {(log?.provider_name ?? log?.provider_id) ? (
            <Badge variant="outline">{log.provider_name ?? log.provider_id}</Badge>
          ) : null}
          {log?.model_name ? (
            <Badge variant="outline" className="border-slate-200 bg-slate-50 text-slate-500">{log.model_name}</Badge>
          ) : null}
          {log?.api_key_name ? (
            <Badge variant="outline" className="border-amber-200 bg-amber-50 text-amber-700">{log.api_key_name}</Badge>
          ) : null}
          {log?.upstream_model ? (
            <span className="text-slate-500 font-mono">{log.upstream_model}</span>
          ) : null}
          {log?.reasoning_effort ? (
            <Badge
              variant="outline"
              className="border-violet-200 bg-violet-50 text-violet-700"
              title={isZh ? "推理强度" : "Reasoning Effort"}
            >
              {log.reasoning_effort}
            </Badge>
          ) : null}
          {log?.latency_total_ms != null ? (
            <span className="text-slate-500" title={isZh ? "端到端耗时" : "End-to-end latency"}>
              {formatDuration(log.latency_total_ms)}
            </span>
          ) : null}
          {log?.stream_first_chunk_ms != null ? (
            <span
              className="text-slate-500"
              title={isZh ? "首字延迟（首个流式 chunk）" : "Time to first token"}
            >
              {isZh ? "首字" : "TTFT"} {formatDuration(log.stream_first_chunk_ms)}
            </span>
          ) : null}
          {tps != null ? (
            <span
              className="text-slate-500"
              title={isZh ? "净生成速度(剥离首字节前等待)" : "Net generation speed (excludes prefill wait)"}
            >
              {formatTps(tps)}
            </span>
          ) : null}
          {log ? (
            <span className="inline-flex items-center gap-2">
              <span className="inline-flex items-center gap-1 text-sky-600">
                <span className="text-[10px] font-semibold tracking-wide">NEW</span>
                <span
                  title={`net input (input_tokens ${log.input_tokens} − cache_read_tokens ${log.cache_read_tokens ?? 0})`}
                >
                  {formatTokenCount(
                    Math.max(log.input_tokens - (log.cache_read_tokens ?? 0), 0),
                  )}
                </span>
              </span>
              <span className="inline-flex items-center gap-1 text-emerald-600">
                <span className="text-[10px] font-semibold tracking-wide">OUT</span>
                <span title={String(log.output_tokens)}>{formatTokenCount(log.output_tokens)}</span>
              </span>
              <Button
                type="button"
                size="sm"
                variant="ghost"
                onClick={handleDownload}
                disabled={isLoading || !!error || !data}
                className="h-7 gap-1 px-2 text-xs"
              >
                {downloaded ? (
                  <><Check className="h-3.5 w-3.5 text-green-600" /><span className="text-green-600">{isZh ? "已保存" : "Saved"}</span></>
                ) : (
                  <><Download className="h-3.5 w-3.5" />{isZh ? "下载" : "Download"}</>
                )}
              </Button>
              {onDelete && log?.id ? (
                <Button
                  type="button"
                  size="sm"
                  variant="ghost"
                  onClick={() => onDelete(log.id!)}
                  className="h-7 gap-1 px-2 text-xs text-red-500 hover:bg-red-50 hover:text-red-600"
                >
                  <Trash2 className="h-3.5 w-3.5" />
                  {isZh ? "删除" : "Delete"}
                </Button>
              ) : null}
            </span>
          ) : null}
        </div>

        <div className="flex-1 space-y-3 overflow-y-auto pr-1">
          {error && <p role="alert" className="text-xs text-red-700">{isZh ? "详细日志加载失败，摘要不代表完整载荷：" : "Failed to load detail; the summary does not contain complete payloads: "}{String(error)}</p>}
          {!isLoading && data === null && <p className="text-xs text-amber-700">{isZh ? "此日志不存在或已删除。" : "This log is unavailable or has been deleted."}</p>}
          {log && <AttemptResultBanner log={log} correlation={correlation.data} correlationLoading={correlation.isLoading} correlationError={correlation.isError} onSelect={setAttemptSelection} isZh={isZh} />}
          <p className="rounded-lg border border-amber-200 bg-amber-50 p-2 text-xs text-amber-800">{isZh ? "仅管理员诊断：四种正文各保留最多 1 MiB（头尾），请求头各最多 64 KiB 并脱敏凭据。正文为原始数据，可能包含敏感信息，不做推测性脱敏；历史记录的采集与脱敏状态可能未知。" : "Admin diagnostics only: each of four bodies retains up to 1 MiB (head/tail); each header capture is capped at 64 KiB with credential redaction. Bodies are raw and may contain sensitive data, without guessed redaction. Historical capture/redaction state may be unknown."}</p>
          {log?.route_decision ? (
            <>
              <SectionHeader
                title={isZh ? "0. 路由决策" : "0. Route Decision"}
                hint={isZh ? "本次请求为何选择该上游" : "Why this upstream was chosen"}
              />
              <RouteDecisionBlock raw={log.route_decision} isZh={isZh} providerNameById={providerNameById} />
            </>
          ) : null}
          <SectionHeader
            title={isZh ? "1. 客户端请求" : "1. Client Request"}
            hint={isZh ? `协议：${protocolLabel(log?.client_protocol)}` : `Protocol: ${protocolLabel(log?.client_protocol)}`}
          />
          <PayloadBlock
            title={isZh ? "客户端请求头" : "Client Request Headers"}
            log={data ?? null}
            field="client_request_headers"
            isZh={isZh}
          />
          <PayloadBlock
            title={isZh ? "客户端请求体" : "Client Request Body"}
            log={data ?? null}
            field="client_request_body"
            isZh={isZh}
          />

          <SectionHeader
            title={isZh ? "2. 上游请求" : "2. Upstream Request"}
            hint={isCrossProtocol
              ? (isZh
                  ? `Nyro 转换输出 → ${protocolLabel(log?.upstream_protocol)}`
                  : `Nyro converted → ${protocolLabel(log?.upstream_protocol)}`)
              : undefined}
          />
          <PayloadBlock
            title={isZh ? "上游请求头" : "Upstream Request Headers"}
            log={data ?? null}
            field="upstream_request_headers"
            isZh={isZh}
          />
          <PayloadBlock
            title={isZh ? "上游请求体" : "Upstream Request Body"}
            log={data ?? null}
            field="upstream_request_body"
            isZh={isZh}
          />

          <SectionHeader
            title={isZh ? "3. 上游响应" : "3. Upstream Response"}
            hint={isZh ? `协议：${protocolLabel(log?.upstream_protocol)}` : `Protocol: ${protocolLabel(log?.upstream_protocol)}`}
          />
          <PayloadBlock
            title={isZh ? "上游响应头" : "Upstream Response Headers"}
            log={data ?? null}
            field="upstream_response_headers"
            isZh={isZh}
          />
          <PayloadBlock
            title={isZh ? "上游响应体" : "Upstream Response Body"}
            log={data ?? null}
            field="upstream_response_body"
            isZh={isZh}
          />

          <SectionHeader
            title={isZh ? "4. 客户端响应" : "4. Client Response"}
            hint={isCrossProtocol
              ? (isZh
                  ? `Nyro 转换输出 → ${protocolLabel(log?.client_protocol)}`
                  : `Nyro converted → ${protocolLabel(log?.client_protocol)}`)
              : undefined}
          />
          <PayloadBlock
            title={isZh ? "客户端响应头" : "Client Response Headers"}
            log={data ?? null}
            field="client_response_headers"
            isZh={isZh}
          />
          <PayloadBlock
            title={isZh ? "客户端响应体" : "Client Response Body"}
            log={data ?? null}
            field="client_response_body"
            isZh={isZh}
          />
        </div>
      </DialogContent>
    </Dialog>
  );
}

// ── Route decision snapshot ────────────────────────────────────────────────

interface RouteDecisionData {
  balance: string;
  candidates: Array<{
    provider: string;
    /** Embedded display name (newer snapshots); older rows resolve via providers. */
    provider_name?: string;
    target: string;
    rank?: number;
    weight?: number;
    share?: number;
    score?: Record<string, unknown>;
    skipped?: { reason: string; window?: string; retry_in_secs?: number } | null;
  }>;
}

function scoreText(score: Record<string, unknown> | undefined): string {
  if (!score) return "–";
  const parts: string[] = [];
  if (typeof score.rate === "number") parts.push(`r=${Number(score.rate.toFixed(2))}`);
  if (typeof score.window === "string") parts.push(score.window);
  if (typeof score.window_boost === "number") parts.push(`×${score.window_boost}`);
  if (typeof score.remaining_quota_pct === "number") parts.push(`⌀${score.remaining_quota_pct}%`);
  if (typeof score.remaining_time_pct === "number") parts.push(`t ${score.remaining_time_pct}%`);
  if (typeof score.ttft_ms === "number") parts.push(`${score.ttft_ms}ms`);
  if (typeof score.state === "string") parts.push(score.state);
  if (typeof score.static_weight === "number") parts.push(`w=${score.static_weight}`);
  if (typeof score.group === "number") parts.push(`G${score.group}`);
  if (typeof score.in_group_rank === "number") parts.push(`#${score.in_group_rank}`);
  return parts.length ? parts.join(" · ") : "–";
}

function RouteDecisionBlock({
  raw,
  isZh,
  providerNameById,
}: {
  raw: string;
  isZh: boolean;
  providerNameById: Map<string, string>;
}) {
  let dec: RouteDecisionData | null = null;
  try {
    dec = JSON.parse(raw) as RouteDecisionData;
  } catch {
    dec = null;
  }
  if (!dec?.candidates?.length) return null;

  const balanceLabel: Record<string, string> = {
    weighted: isZh ? "加权轮询" : "Weighted",
    priority: isZh ? "优先分级" : "Priority",
    latency: isZh ? "延迟优先" : "Latency",
    usage: isZh ? "用量优先" : "Usage",
  };

  return (
    <div className="space-y-1.5">
      {dec.candidates.map((c, i) => {
        const skipped = c.skipped;
        // Prefer the name embedded in the snapshot, then the providers list;
        // only fall back to the raw id when neither is available.
        const providerLabel = c.provider_name ?? providerNameById.get(c.provider) ?? c.provider;
        return (
          <div
            key={i}
            className={"flex items-center gap-2 rounded-lg border px-2.5 py-1.5 text-xs " + (skipped ? "border-red-100 bg-red-50/50" : "border-slate-200 bg-slate-50/60")}
          >
            <span
              className={"inline-flex h-5 w-5 shrink-0 items-center justify-center rounded font-semibold " + (skipped ? "bg-red-100 text-red-500" : "bg-emerald-100 text-emerald-700")}
              title={skipped ? (isZh ? "未参与" : "skipped") : isZh ? "尝试顺序" : "attempt order"}
            >
              {skipped ? "✕" : c.rank}
            </span>
            <span
              className="min-w-0 flex-1 truncate font-medium text-slate-700"
              title={`${providerLabel} · ${c.target}\n${c.provider}`}
            >
              {c.target}
              <span className="ml-1 inline-flex max-w-[7rem] items-center truncate rounded bg-slate-200/70 px-1.5 py-0.5 align-middle text-[10px] font-normal text-slate-500">
                {providerLabel}
              </span>
            </span>
            <span className="shrink-0 text-[10px] text-slate-500">{scoreText(c.score)}</span>
            {typeof c.share === "number" ? (
              <span className="shrink-0 rounded bg-sky-50 px-1.5 py-0.5 text-[10px] font-medium text-sky-600">
                {(c.share * 100).toFixed(0)}%
              </span>
            ) : null}
            {skipped ? (
              <span
                className="shrink-0 rounded bg-red-100 px-1.5 py-0.5 text-[10px] font-medium text-red-600"
                title={skipped.retry_in_secs != null ? (isZh ? `约 ${skipped.retry_in_secs}s 后重试` : `retry in ~${skipped.retry_in_secs}s`) : undefined}
              >
                {skipped.reason}
                {skipped.window ? `:${skipped.window}` : ""}
              </span>
            ) : null}
          </div>
        );
      })}
      <p className="px-1 text-[10px] text-slate-400">
        {isZh
          ? `策略：${balanceLabel[dec.balance] ?? dec.balance} · 采集于路由选择时点`
          : `Strategy: ${balanceLabel[dec.balance] ?? dec.balance} · captured at selection time`}
      </p>
    </div>
  );
}

function SectionHeader({ title, hint }: { title: string; hint?: string }) {
  return (
    <div className="flex items-center gap-2 pt-1">
      <span className="text-xs font-semibold text-slate-500 uppercase tracking-wider">{title}</span>
      {hint ? (
        <span className="text-[10px] text-slate-400 font-normal normal-case tracking-normal shrink-0">{hint}</span>
      ) : null}
      <div className="flex-1 border-t border-slate-200" />
    </div>
  );
}
