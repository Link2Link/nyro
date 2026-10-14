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

/** 自适应时序桶标签:HH:mm,长时间范围附带 MM/DD。 */
export function formatLocalBucketLabel(
  ts: number | string | null | undefined,
  withDate = false,
): string {
  const date = parseBackendTime(ts);
  if (!date) return "";
  const time = `${pad2(date.getHours())}:${pad2(date.getMinutes())}`;
  if (!withDate) return time;
  return `${pad2(date.getMonth() + 1)}/${pad2(date.getDate())} ${time}`;
}

/** Tooltip 使用的本地桶起止范围,首尾桶裁剪到实际查询窗口。 */
export function formatLocalBucketRange(
  ts: number | string | null | undefined,
  bucketMinutes: number,
  windowStart?: number,
  windowEnd?: number,
): string {
  const bucketStart = parseBackendTime(ts);
  if (!bucketStart) return "";
  const start = Math.max(bucketStart.getTime(), windowStart ?? Number.NEGATIVE_INFINITY);
  const end = Math.min(
    bucketStart.getTime() + bucketMinutes * 60_000,
    windowEnd ?? Number.POSITIVE_INFINITY,
  );
  return `${formatLocalBucketLabel(start, true)} – ${formatLocalBucketLabel(end, true)}`;
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
  if (n < 1_000_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  return `${(n / 1_000_000_000).toFixed(2)}B`;
}

/**
 * TPS 展示:0 仍显示 0.0 tok/s;
 * null/非有限/负值才表示不可用,显示 "–"。
 */
export function formatTps(tps: number | null | undefined): string {
  if (tps == null || !Number.isFinite(tps) || tps < 0) return "–";
  return `${tps.toFixed(1)} tok/s`;
}

/** 计算单条请求 TPS 所需的最小字段集(结构兼容 `RequestLog`)。 */
export interface TpsInput {
  output_tokens?: number | null;
  reasoning_tokens?: number | null;
  latency_upstream_ms?: number | null;
  latency_total_ms?: number | null;
}

/**
 * 端到端耗时(ms):取上游耗时,仅当其为 null/undefined 时回退总耗时。
 * 所选来源 ≤ 0 或非有限值一律视为无效,返回 null(绝不减去首字延迟)。
 */
export function endToEndMsOf(log: TpsInput | null | undefined): number | null {
  if (!log) return null;
  const ms = log.latency_upstream_ms === null || log.latency_upstream_ms === undefined
    ? log.latency_total_ms
    : log.latency_upstream_ms;
  if (typeof ms !== "number" || !Number.isFinite(ms) || ms <= 0) return null;
  return ms;
}

/** 正文 Token 数 = max(output − max(reasoning, 0), 0);推理明细缺失时按 0 兼容。 */
export function contentTokensOf(log: TpsInput | null | undefined): number {
  const out = Math.max(0, log?.output_tokens ?? 0);
  const reasoning = Math.max(0, log?.reasoning_tokens ?? 0);
  return Math.max(0, out - reasoning);
}

const hasUsage = (log: TpsInput | null | undefined): log is TpsInput =>
  typeof log?.output_tokens === "number" && Number.isFinite(log.output_tokens) && log.output_tokens > 0;

/**
 * 体感 TPS (tok/s) = 总输出 Token ÷ 端到端耗时。
 * 推理 Token 计入输出,首字延迟计入耗时(耗时本身就是端到端,不扣 TTFT)。
 * output ≤ 0(含缺失)视为无用量 → null。耗时无效 → null。
 */
export function computeTps(log: TpsInput | null | undefined): number | null {
  if (!hasUsage(log)) return null;
  const ms = endToEndMsOf(log);
  if (ms == null) return null;
  const tps = log.output_tokens! / (ms / 1000);
  return Number.isFinite(tps) ? tps : null;
}

/** 后端聚合 DTO 携带的有效样本汇总字段(与全历史总量字段相互独立)。 */
export interface AggregateTpsSums {
  tps_content_tokens?: number | null;
  tps_output_tokens?: number | null;
  tps_elapsed_ms?: number | null;
}

const validSum = (value: number | null | undefined, allowZero: boolean): value is number =>
  typeof value === "number" && Number.isFinite(value) && (allowZero ? value >= 0 : value > 0);

/**
 * 聚合 TPS = Σ输出 Token ÷ Σ有效请求耗时。汇总字段缺失或非法时返回
 * null(显示 "–"),绝不回退用全历史总量凑值。推理 Token 已含在输出里。
 */
export function computeAggregateTps(sums: AggregateTpsSums | null | undefined): number | null {
  if (!sums || !validSum(sums.tps_output_tokens, true) || !validSum(sums.tps_elapsed_ms, false)) return null;
  const tps = sums.tps_output_tokens / (sums.tps_elapsed_ms / 1000);
  return Number.isFinite(tps) ? tps : null;
}

/**
 * 字段在场门控:新契约服务器在返回 average_tps 的同时必然携带伴生字段
 * (overall_tps 别名或 valid_tps_count)。只有旧 average 字段的响应不能按新
 * 口径展示——返回 null 由调用方显示 "–" 与旧服务器提示,而不是把旧口径
 * 数值(如历史扣除 TTFT 或推理 Token 的算法结果)误标为当前输出 TPS。
 */
export interface AverageTpsContractFields {
  average_tps?: number | null;
  overall_tps?: number | null;
  valid_tps_count?: number | null;
}
export function readAverageTpsField(stats: AverageTpsContractFields | null | undefined): number | null {
  if (!stats) return null;
  if (stats.overall_tps === undefined && stats.valid_tps_count === undefined) return null;
  const tps = stats.average_tps;
  if (tps == null || !Number.isFinite(tps) || tps < 0) return null;
  return tps;
}

/** 仅含旧 TPS 字段的服务器提示:宁可不可用,不误标新口径。 */
export function tpsLegacyServerTitle(zh: boolean): string {
  return zh
    ? "此服务器仅返回旧版 TPS 字段（缺少新口径伴随字段 valid_tps_count/overall_tps），为避免口径混淆按不可用显示，不会按新口径重新解释。"
    : "This server only reports the legacy TPS field without the new-contract companions (valid_tps_count/overall_tps); it is shown as unavailable rather than relabeled as the new metric.";
}

/** 输出 TPS 口径说明:总输出 Token 的端到端体感吞吐,含首字延迟与推理 Token。 */
export function tpsMetricTitle(zh: boolean): string {
  return zh
    ? "输出 TPS = 总输出 Token ÷ 请求耗时（上游耗时，仅未记录时回退总耗时；含首字延迟，推理 Token 计入输出）。输出 ≤ 0 视为无用量，不计入。"
    : "Output TPS = total output tokens ÷ request duration (upstream latency, falling back to total latency only when unrecorded; first-token latency included, reasoning tokens counted in the output). Output ≤ 0 counts as no usage.";
}

/** 聚合 TPS 的时间窗口说明:按页面时间筛选汇总,区别于性能页最近 50 次采样。 */
export function tpsAggregateWindowTitle(zh: boolean): string {
  return zh
    ? "TPS 按当前所选时间范围内有效请求汇总（ΣToken ÷ Σ耗时），与性能页“最近 50 次调用”采样窗口不同。"
    : "TPS sums tokens and durations over valid requests in the currently selected time range (Σtokens ÷ Σduration), unlike the Performance page's latest-50-call sampling window.";
}

/** 后端尚未提供聚合汇总字段时的兼容提示。 */
export function tpsSumsUnavailableTitle(zh: boolean): string {
  return zh
    ? "此服务器版本尚未提供输出 TPS 所需的有效样本汇总字段，暂不可用。"
    : "This server version does not yet report the valid-sample sums required for output TPS, so it is unavailable.";
}

/**
 * 聚合 TPS 单元格的完整悬浮说明,区分三种状态:
 *  - 汇总字段缺失/非法 → 兼容提示(旧服务器);
 *  - 字段齐全但 Σ耗时 = 0 → 所选时间范围内没有可计入的有效请求(有输出且耗时有效);
 *  - 正常 → 输出 TPS 数值 + 时间窗口。
 */
export function tpsAggregateStatusTitle(sums: AggregateTpsSums | null | undefined, zh: boolean): string {
  const content = sums?.tps_content_tokens;
  const output = sums?.tps_output_tokens;
  const elapsed = sums?.tps_elapsed_ms;
  const present = [content, output, elapsed].every((value) => typeof value === "number" && Number.isFinite(value));
  if (!present) return tpsSumsUnavailableTitle(zh);
  const lines = [tpsMetricTitle(zh)];
  if (elapsed! > 0) {
    lines.push(`${zh ? "输出 TPS" : "Output TPS"}: ${formatTps(computeAggregateTps(sums))}`);
  } else {
    lines.push(zh
      ? "所选时间范围内没有可计入 TPS 的有效请求（需要输出 > 0 且耗时有效）。"
      : "No valid requests count toward TPS in the selected time range (output > 0 and usable timing required).");
  }
  lines.push(tpsAggregateWindowTitle(zh));
  return lines.join("\n");
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
