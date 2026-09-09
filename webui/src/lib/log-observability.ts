import type { EffectiveOutcome, LogQuery, RequestLog } from "./types.ts";

export const EFFECTIVE_OUTCOMES = ["error", "completed", "cancelled", "output_limited", "unknown"] as const;

/** Only the backend-derived result is authoritative. Legacy HTTP/performance hints never classify a result. */
export function effectiveOutcome(log: Partial<RequestLog> | null | undefined): EffectiveOutcome {
  return EFFECTIVE_OUTCOMES.includes(log?.effective_outcome as EffectiveOutcome)
    ? log!.effective_outcome as EffectiveOutcome
    : "unknown";
}

export function outcomeLabel(outcome: string, isZh = false): string {
  const labels: Record<string, [string, string]> = {
    error: ["Error", "错误"], completed: ["Completed", "已完成"], failed: ["Failed", "失败"],
    timed_out: ["Timed out", "超时"], cancelled: ["Cancelled", "已取消"],
    output_limited: ["Output limited", "输出受限"], unknown: ["Unknown", "未知"],
  };
  return labels[outcome]?.[isZh ? 1 : 0] ?? outcome;
}

export function outcomeQuery(value: string): Pick<LogQuery, "is_error" | "outcome"> {
  if (value === "error") return { is_error: true, outcome: undefined };
  return { is_error: undefined, outcome: EFFECTIVE_OUTCOMES.includes(value as EffectiveOutcome) ? value as EffectiveOutcome : undefined };
}

export function outcomeFilterValue(query: LogQuery): string {
  return query.is_error === true ? "error" : query.outcome ?? "all";
}

interface OutcomeCounts {
  request_count: number;
  success_count: number;
  error_count: number;
  unknown_count: number;
  cancelled_count: number;
  output_limited_count: number;
  outcome_stats_version: number;
}

function byteCount(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

export function usageOutcomeRates(detail: Partial<OutcomeCounts> | null | undefined) {
  const counts = detail && [detail.request_count, detail.success_count, detail.error_count, detail.unknown_count, detail.cancelled_count, detail.output_limited_count];
  const authoritative = !!detail && (detail.outcome_stats_version ?? 0) >= 1 && !!counts?.every(byteCount)
    && counts.slice(1).reduce<number>((sum, count) => sum + count!, 0) === detail.request_count;
  const total = detail?.request_count ?? 0;
  return {
    authoritative,
    completedRate: authoritative && total > 0 ? detail!.success_count! / total * 100 : null,
    unknownRate: authoritative && total > 0 ? detail!.unknown_count! / total * 100 : null,
  };
}

export function parseErrorCauses(raw: string | null | undefined): { causes: string[]; malformed: boolean } {
  if (!raw) return { causes: [], malformed: false };
  try {
    const value: unknown = JSON.parse(raw);
    if (Array.isArray(value) && value.every((item) => typeof item === "string")) return { causes: value, malformed: false };
  } catch { /* Preserve malformed evidence as safe plain text. */ }
  return { causes: [raw], malformed: true };
}

export const PAYLOAD_FIELDS = [
  "client_request_headers", "client_request_body", "upstream_request_headers", "upstream_request_body",
  "upstream_response_headers", "upstream_response_body", "client_response_headers", "client_response_body",
] as const;
export type PayloadField = typeof PAYLOAD_FIELDS[number];
export interface PayloadMetadata {
  total_observed_bytes: number;
  retained_bytes: number;
  head_bytes?: number;
  tail_bytes?: number;
  truncated: boolean;
  complete: boolean;
  encoding: "utf8" | "base64" | "none";
  capture_state: "absent" | "empty" | "captured" | "not_retained";
  omitted_headers?: number;
  [key: string]: unknown;
}
export interface PayloadEvidence {
  state: "cleared" | "unknown" | "invalid" | "absent" | "empty" | "missing" | "captured" | "not_retained";
  metadata?: PayloadMetadata;
  segments: { label: "content" | "head" | "tail"; text: string; encoding: "utf8" | "base64" | "unverified" }[];
  missingBytes: number | null;
  issue?: string;
}

export function parsePayloadMetadata(raw: string | null | undefined): { entries: Record<string, unknown>; malformed: boolean } {
  if (!raw) return { entries: {}, malformed: false };
  try {
    const value: unknown = JSON.parse(raw);
    if (value && typeof value === "object" && !Array.isArray(value)) return { entries: value as Record<string, unknown>, malformed: false };
  } catch { /* Invalid metadata must not imply a complete payload. */ }
  return { entries: {}, malformed: true };
}

function validMetadata(value: unknown, header: boolean): value is PayloadMetadata {
  if (!value || typeof value !== "object") return false;
  const m = value as PayloadMetadata;
  if (!byteCount(m.total_observed_bytes) || !byteCount(m.retained_bytes)
    || typeof m.truncated !== "boolean" || typeof m.complete !== "boolean"
    || !["utf8", "base64", "none"].includes(m.encoding) || !["absent", "empty", "captured", "not_retained"].includes(m.capture_state)) return false;
  // Recording-disabled entries can preserve original capture lengths or zero them;
  // availability is explicit, so neither lengths nor original truncation imply stored bytes.
  if (m.capture_state === "not_retained") return (m.head_bytes == null || byteCount(m.head_bytes)) && (m.tail_bytes == null || byteCount(m.tail_bytes));
  // Header observed bytes are raw pairs; retained bytes are credential-redacted JSON.
  if (header) return true;
  return byteCount(m.head_bytes) && byteCount(m.tail_bytes)
    && m.head_bytes + m.tail_bytes === m.retained_bytes
    && m.total_observed_bytes >= m.retained_bytes
    && m.truncated === (m.total_observed_bytes > m.retained_bytes)
    && (m.capture_state === "captured" ? m.retained_bytes > 0 : m.retained_bytes === 0);
}

function encodeBase64(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}

export function payloadEvidence(log: Partial<RequestLog>, field: PayloadField, prettyJson = true): PayloadEvidence {
  if (log.payload_cleared_at != null) return { state: "cleared", segments: [], missingBytes: null };
  const raw = log[field];
  const fallback = raw != null ? [{ label: "content" as const, text: raw, encoding: "unverified" as const }] : [];
  const parsed = parsePayloadMetadata(log.payload_metadata);
  const value = parsed.entries[field];
  const header = field.endsWith("_headers");
  if (parsed.malformed || (value != null && !validMetadata(value, header))) {
    return { state: "invalid", segments: fallback, missingBytes: null, issue: "Invalid payload metadata; raw stored text shown without interpretation." };
  }
  if (!value) return { state: "unknown", segments: fallback, missingBytes: null };
  const metadata = value as PayloadMetadata;
  const base = { metadata, missingBytes: header || metadata.capture_state === "not_retained" ? null : metadata.total_observed_bytes - metadata.retained_bytes };
  if (metadata.capture_state === "not_retained") return { ...base, state: "not_retained", segments: [] };
  if (metadata.capture_state === "absent" && raw == null) return { ...base, state: "absent", segments: [] };
  if (raw == null) return { ...base, state: "missing", segments: [] };
  try {
    if (metadata.capture_state === "absent") throw new Error("Stored payload contradicts absent capture metadata.");
    const bytes = metadata.encoding === "base64"
      ? Uint8Array.from(atob(raw), (char) => char.charCodeAt(0))
      : new TextEncoder().encode(raw);
    if (metadata.encoding === "none" || bytes.length !== metadata.retained_bytes) throw new Error("Stored payload length or encoding disagrees with metadata.");
    if (metadata.truncated && !header) {
      const head = bytes.slice(0, metadata.head_bytes);
      const tail = bytes.slice(metadata.head_bytes);
      const segment = (label: "head" | "tail", data: Uint8Array) => {
        if (metadata.encoding === "base64") return { label, text: encodeBase64(data), encoding: "base64" as const };
        // Fatal decoding avoids silently replacing a split UTF-8 edge with U+FFFD.
        return { label, text: new TextDecoder("utf-8", { fatal: true }).decode(data), encoding: "utf8" as const };
      };
      return { ...base, state: "captured", segments: [segment("head", head), segment("tail", tail)] };
    }
    let text = raw;
    if (prettyJson && metadata.complete && !metadata.truncated && metadata.encoding === "utf8") {
      try { text = JSON.stringify(JSON.parse(raw), null, 2); } catch { /* Not JSON: preserve raw bytes as text. */ }
    }
    return { ...base, state: metadata.capture_state === "empty" ? "empty" : "captured", segments: [{ label: "content", text, encoding: metadata.encoding }] };
  } catch (error) {
    return { ...base, state: "invalid", segments: fallback, issue: error instanceof Error ? error.message : "Invalid payload evidence" };
  }
}

export function payloadStateLabel(state: PayloadEvidence["state"], isZh = false): string {
  const labels: Record<PayloadEvidence["state"], [string, string]> = {
    cleared: ["Manually cleared", "已手动清除"], unknown: ["Capture state unknown (legacy / metadata unavailable)", "采集状态未知（历史记录或无元数据）"],
    invalid: ["Invalid capture metadata or evidence", "采集元数据或证据无效"], absent: ["Not captured", "未采集"],
    empty: ["Observed empty", "已观测为空"], missing: ["Stored payload missing", "已存载荷缺失"], captured: ["Captured", "已采集"],
    not_retained: ["Not retained (payload recording disabled)", "未保留（已禁用载荷记录）"],
  };
  return labels[state][isZh ? 1 : 0];
}

export function payloadDownload(log: RequestLog): string {
  return [
    "# ADMIN EVIDENCE: bodies are raw and may contain sensitive data; credential header redaction is backend-owned.",
    `# Payload cleared at: ${log.payload_cleared_at ?? "not marked cleared"}`,
    `# Payload metadata (raw JSON): ${log.payload_metadata ?? "unavailable (legacy / unknown)"}`,
    ...PAYLOAD_FIELDS.flatMap((field) => {
      // Downloads preserve stored evidence bytes; pretty JSON is a display-only option.
      const evidence = payloadEvidence(log, field, false);
      return ["", `## ${field}`, `# ${payloadStateLabel(evidence.state)}`,
        `# Metadata: ${JSON.stringify(evidence.metadata ?? null)}`,
        `# Missing observed bytes: ${evidence.missingBytes ?? "unknown / not applicable"}`,
        ...(evidence.issue ? [`# ${evidence.issue}`] : []),
        ...evidence.segments.flatMap((part) => [`# ${part.label} (${part.encoding})`, part.text]),
      ];
    }),
  ].join("\n");
}

/** Shared prefix inventory: every destructive action invalidates statistics and correlated detail too. */
export function isLogRelatedQueryKey(key: readonly unknown[]): boolean {
  const family = key[0];
  return typeof family === "string" && (
    ["logs", "log-detail", "request-log-attempts", "model-performance"].includes(family)
    || family === "stats" || family.startsWith("stats-")
    || family.startsWith("provider-usage") || family.startsWith("api-key-usage") || family.startsWith("model-usage")
  );
}
