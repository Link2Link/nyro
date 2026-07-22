export function formatDuration(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms)) return "–";
  if (ms < 1000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(2)} s`;
  if (ms < 3_600_000) return `${(ms / 60_000).toFixed(1)} m`;
  return `${(ms / 3_600_000).toFixed(1)} h`;
}

/**
 * 把后端返回的时间解析为 Date(本地时区视图)。
 * 后端统一用 UTC 存储/返回:
 *  - 数字: UTC 毫秒时间戳;
 *  - 字符串: RFC3339(含 "T")直接解析;空格分隔且无时区后缀的(如 "2026-07-15 03:00:00")
 *    按 UTC 处理,补 "Z" 后解析。
 * 返回的 Date 经本地方法(getHours 等)读取即为浏览器本地时区。
 * 解析失败返回 null。
 */
export function parseBackendTime(ts: number | string | null | undefined): Date | null {
  if (ts == null) return null;
  const date = typeof ts === "number" ? new Date(ts) : (() => {
    const normalized = ts.includes("T") ? ts : ts.replace(" ", "T") + "Z";
    return new Date(normalized);
  })();
  return Number.isNaN(date.getTime()) ? null : date;
}

const pad2 = (n: number) => String(n).padStart(2, "0");

/** 日志时间:MM/DD HH:MM:SS(本地时区)。 */
export function formatLogTime(ts: number | string | null | undefined): string {
  const date = parseBackendTime(ts);
  if (!date) return ts == null ? "–" : String(ts);
  const mm = pad2(date.getMonth() + 1);
  const dd = pad2(date.getDate());
  const hh = pad2(date.getHours());
  const mi = pad2(date.getMinutes());
  const ss = pad2(date.getSeconds());
  return `${mm}/${dd} ${hh}:${mi}:${ss}`;
}

/** 完整日期时间:YYYY-MM-DD HH:MM:SS(本地时区),用于过期时间等。 */
export function formatLocalDateTime(ts: number | string | null | undefined): string {
  const date = parseBackendTime(ts);
  if (!date) return "–";
  return `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())} `
    + `${pad2(date.getHours())}:${pad2(date.getMinutes())}:${pad2(date.getSeconds())}`;
}

/** 按小时聚合的图表 X 轴标签(本地时区)。后端按 UTC 整点分桶,
 *  这里把 UTC 桶标签转成本地小时显示。
 *  withDate=true 时附带日期 "MM/DD HH:00",用于跨度超过 24 小时、
 *  仅显示小时会出现重复的场景。 */
export function formatLocalHourLabel(ts: string | null | undefined, withDate = false): string {
  const date = parseBackendTime(ts);
  if (!date) return "";
  const hh = pad2(date.getHours());
  if (!withDate) return `${hh}:00`;
  const mm = pad2(date.getMonth() + 1);
  const dd = pad2(date.getDate());
  return `${mm}/${dd} ${hh}:00`;
}

/** 日期戳 YYYYMMDD(本地时区),用于导出文件名等。 */
export function formatLocalDateStamp(ts: number | string | null | undefined): string {
  const date = parseBackendTime(ts);
  if (!date) return "";
  return `${date.getFullYear()}${pad2(date.getMonth() + 1)}${pad2(date.getDate())}`;
}

export function formatTokenCount(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return "0";
  const n = Math.max(0, Math.floor(value));
  if (n < 1000) return String(n);
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)}K`;
  return `${(n / 1_000_000).toFixed(2)}M`;
}

export function formatTps(tps: number | null | undefined): string {
  if (tps == null || !Number.isFinite(tps) || tps <= 0) return "–";
  if (tps < 100) return `${tps.toFixed(1)} tok/s`;
  return `${Math.round(tps)} tok/s`;
}

/** 计算 TPS 所需的最小字段集(结构兼容 `RequestLog`)。 */
export interface TpsInput {
  output_tokens?: number | null;
  is_stream?: boolean | null;
  stream_chunks_count?: number | null;
  latency_upstream_ms?: number | null;
  latency_total_ms?: number | null;
  stream_first_chunk_ms?: number | null;
}

/**
 * 净生成耗时(ms):流式 = 上游耗时 − 首字节延迟;非流式 = 上游往返耗时;
 * 缺失时回退到端到端总耗时。无法确定时返回 null。
 */
export function generationMsOf(log: TpsInput | null | undefined): number | null {
  if (!log) return null;
  const isStream = log.is_stream ?? (log.stream_chunks_count ?? 0) > 0;
  const upstream = log.latency_upstream_ms ?? null;
  const ttfb = log.stream_first_chunk_ms ?? null;
  if (isStream && upstream != null && ttfb != null) {
    const gen = upstream - ttfb;
    // 净生成耗时必须真实反映增量解码阶段。当首字节延迟占上游耗时比例过高
    // (上游未真正增量流式,而是在服务端算完后一口气 flush),gen 会趋近于 0,
    // 导致 TPS 被放大成荒诞的数值。此时回退到上游往返耗时作为生成耗时。
    const TTFB_RATIO_THRESHOLD = 0.8;
    const GEN_MIN_MS = 50;
    const looksNonIncremental = gen <= 0
      || ttfb / upstream >= TTFB_RATIO_THRESHOLD
      || gen < GEN_MIN_MS;
    if (looksNonIncremental) return upstream > 0 ? upstream : null;
    return gen;
  }
  return upstream ?? log.latency_total_ms ?? null;
}

/** 净生成速度(tok/s);output ≤ 0 或净生成耗时无效时返回 null。 */
export function computeTps(log: TpsInput | null | undefined): number | null {
  const gen = generationMsOf(log);
  const out = log?.output_tokens ?? 0;
  if (out > 0 && gen && gen > 0) return out / (gen / 1000);
  return null;
}

export function tryPrettyJson(raw: string | null | undefined): string {
  if (raw == null) return "";
  if (typeof raw !== "string") {
    try {
      return JSON.stringify(raw, null, 2);
    } catch {
      return String(raw);
    }
  }
  const trimmed = raw.trim();
  if (!trimmed) return raw;
  try {
    const parsed = JSON.parse(trimmed);
    return JSON.stringify(parsed, null, 2);
  } catch {
    return raw;
  }
}
