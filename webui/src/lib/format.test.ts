import { equal, ok } from "node:assert/strict";
import { test } from "node:test";
import { readFileSync } from "node:fs";
import path from "node:path";
import {
  computeAggregateGrossTps,
  computeAggregateTps,
  computeGrossTps,
  computeTps,
  contentTokensOf,
  endToEndMsOf,
  formatTokenCount,
  formatTps,
  readAverageTpsField,
  tpsAggregateStatusTitle,
  type AggregateTpsSums,
  type TpsInput,
} from "./format";

/** 从仓库根读取后端共享的公式契约 fixture(后端 Rust 测试使用同一文件)。 */
function loadTpsContract(): { name: string; log: TpsInput; content_tokens: number; tps: number | null; gross_tps: number | null }[] {
  let dir = process.cwd();
  for (let i = 0; i < 6; i++) {
    const candidate = path.join(dir, "tests", "fixtures", "tps-contract.json");
    try {
      return JSON.parse(readFileSync(candidate, "utf8"));
    } catch {
      dir = path.dirname(dir);
    }
  }
  throw new Error("tests/fixtures/tps-contract.json not found; run from the webui or repo root");
}

test("token count formatting scales across K, M, and B thresholds with proper decimals", () => {
  equal(formatTokenCount(null), "0");
  equal(formatTokenCount(undefined), "0");
  equal(formatTokenCount(NaN), "0");
  equal(formatTokenCount(-10), "0");
  equal(formatTokenCount(0), "0");
  equal(formatTokenCount(42), "42");
  equal(formatTokenCount(999), "999");
  equal(formatTokenCount(1000), "1.0K");
  equal(formatTokenCount(1500), "1.5K");
  equal(formatTokenCount(999_999), "1000.0K");
  equal(formatTokenCount(1_000_000), "1.00M");
  equal(formatTokenCount(12_340_000), "12.34M");
  equal(formatTokenCount(999_999_999), "1000.00M");
  equal(formatTokenCount(1_000_000_000), "1.00B");
  equal(formatTokenCount(1_500_000_000), "1.50B");
  equal(formatTokenCount(12_345_678_901), "12.35B");
});

test("TPS display uses one decimal; zero is valid, only null/non-finite/negative go missing", () => {
  for (const [value, expected] of [
    [58.333333333333, "58.3 tok/s"],
    [99.96, "100.0 tok/s"],
    [100, "100.0 tok/s"],
    [225.678, "225.7 tok/s"],
    [1000, "1000.0 tok/s"],
    [0.01, "0.0 tok/s"],
    [0, "0.0 tok/s"],
  ] as const) equal(formatTps(value), expected);
  for (const value of [null, undefined, NaN, Infinity, -Infinity, -1, -0.0001]) {
    equal(formatTps(value), "–");
  }
});

test("shared tps-contract fixture: content, main TPS and gross TPS all match the backend oracle", () => {
  const cases = loadTpsContract();
  equal(cases.length, 17);
  for (const item of cases) {
    equal(contentTokensOf(item.log), item.content_tokens, `${item.name}: content tokens`);
    equal(computeTps(item.log), item.tps, `${item.name}: main TPS`);
    equal(computeGrossTps(item.log), item.gross_tps, `${item.name}: gross TPS`);
    if (item.tps !== null) equal(formatTps(item.tps), `${item.tps.toFixed(1)} tok/s`, `${item.name}: display`);
  }
});

test("duration uses upstream latency and only falls back on null/undefined, never subtracting TTFT", () => {
  // TTFT / stream fields are irrelevant now: upstream latency is used verbatim.
  equal(endToEndMsOf({ output_tokens: 100, latency_upstream_ms: 1000, latency_total_ms: 9000 }), 1000);
  equal(computeTps({ output_tokens: 100, latency_upstream_ms: 1000, stream_first_chunk_ms: 800 } as TpsInput), 100);
  // Only null/undefined upstream falls back to total latency.
  equal(computeTps({ output_tokens: 50, reasoning_tokens: 10, latency_upstream_ms: null, latency_total_ms: 2000 }), 20);
  equal(computeTps({ output_tokens: 100, latency_total_ms: 2000 }), 50);
  // Zero/negative/non-finite upstream or fallback values are invalid, no second fallback.
  equal(computeTps({ output_tokens: 100, latency_upstream_ms: 0, latency_total_ms: 5000 }), null);
  equal(computeTps({ output_tokens: 100, latency_upstream_ms: -100, latency_total_ms: 2000 }), null);
  equal(computeTps({ output_tokens: 100, latency_upstream_ms: null, latency_total_ms: 0 }), null);
  equal(computeTps({ output_tokens: 100 }), null);
});

test("no-usage rows are invalid; pure reasoning with output > 0 is a valid zero", () => {
  equal(computeTps({ output_tokens: 0, reasoning_tokens: 0, latency_upstream_ms: 1000 }), null);
  equal(computeTps({ output_tokens: null, latency_upstream_ms: 1000 }), null);
  equal(computeTps({ output_tokens: -5, reasoning_tokens: -10, latency_upstream_ms: 1000 }), null);
  equal(computeGrossTps({ output_tokens: 0, latency_upstream_ms: 1000 }), null);
  // output > 0 with all reasoning: content 0 counts as a valid sample with 0 tok/s.
  equal(computeTps({ output_tokens: 100, reasoning_tokens: 100, latency_upstream_ms: 1000 }), 0);
  equal(formatTps(computeTps({ output_tokens: 100, reasoning_tokens: 120, latency_upstream_ms: 500 })), "0.0 tok/s");
  equal(computeGrossTps({ output_tokens: 50, reasoning_tokens: 80, latency_upstream_ms: 500 }), 100);
});

test("reasoning details must be finite when present; NaN/Infinity never fabricate zero throughput", () => {
  equal(computeTps({ output_tokens: 100, reasoning_tokens: NaN, latency_upstream_ms: 1000 }), null);
  equal(computeTps({ output_tokens: 100, reasoning_tokens: Infinity, latency_upstream_ms: 1000 }), null);
  equal(computeTps({ output_tokens: 100, reasoning_tokens: -30, latency_upstream_ms: 1000 }), 100);
});

test("aggregate TPS uses backend valid-sample sums and never falls back to whole-history totals", () => {
  // Missing or partial sums stay unavailable; no gross fallback from old total fields.
  equal(computeAggregateTps(null), null);
  equal(computeAggregateTps({}), null);
  equal(computeAggregateTps({ tps_content_tokens: 100 }), null);
  equal(computeAggregateTps({ tps_content_tokens: 100, tps_elapsed_ms: 0 }), null);
  equal(computeAggregateTps({ tps_content_tokens: 100, tps_elapsed_ms: -5 }), null);
  equal(computeAggregateGrossTps({ tps_output_tokens: 100, tps_elapsed_ms: 0 }), null);
  // Same pool division: pure-reasoning-only aggregate is a valid zero.
  equal(computeAggregateTps({ tps_content_tokens: 0, tps_elapsed_ms: 2000 }), 0);
  equal(computeAggregateGrossTps({ tps_output_tokens: 1100, tps_elapsed_ms: 12500 }), 88);
});

test("aggregating the shared fixture reproduces the fixed Σcontent/Σoutput/Σelapsed oracle", () => {
  const sums = loadTpsContract().reduce((acc, item) => {
    const output = item.log.output_tokens;
    if (typeof output !== "number" || !(output > 0)) return acc;
    const ms = endToEndMsOf(item.log);
    if (ms == null) return acc;
    acc.tps_content_tokens += contentTokensOf(item.log);
    acc.tps_output_tokens += output;
    acc.tps_elapsed_ms += ms;
    return acc;
  }, { tps_content_tokens: 0, tps_output_tokens: 0, tps_elapsed_ms: 0 } satisfies AggregateTpsSums);
  equal(sums.tps_content_tokens, 760);
  equal(sums.tps_output_tokens, 1100);
  equal(sums.tps_elapsed_ms, 12500);
  equal(computeAggregateTps(sums), 60.8);
  equal(computeAggregateGrossTps(sums), 88);
});

test("aggregate status title distinguishes unsupported backends from empty pools", () => {
  // Missing/partial sums → compatibility hint about the server, never a fabricated value.
  ok(tpsAggregateStatusTitle(null, false).includes("does not yet report"));
  ok(tpsAggregateStatusTitle({}, false).includes("does not yet report"));
  ok(tpsAggregateStatusTitle({ tps_content_tokens: 100 }, false).includes("does not yet report"));
  // Complete sums with zero elapsed time → genuinely no valid samples in the window.
  const empty = tpsAggregateStatusTitle({ tps_content_tokens: 0, tps_output_tokens: 0, tps_elapsed_ms: 0 }, false);
  ok(empty.includes("No valid requests"));
  ok(!empty.includes("does not yet report"));
  // Live sums render the content value and the gross auxiliary figure.
  const live = tpsAggregateStatusTitle({ tps_content_tokens: 760, tps_output_tokens: 1100, tps_elapsed_ms: 12500 }, false);
  ok(live.includes("Content TPS: 60.8 tok/s"));
  ok(live.includes("Gross TPS: 88.0 tok/s"));
});

test("average_tps field gate refuses legacy-only payloads instead of relabeling them", () => {
  // Legacy servers send only average_tps; without a new-contract companion the value
  // must not be displayed as the unified content metric.
  equal(readAverageTpsField({ average_tps: 58.3 }), null);
  equal(readAverageTpsField(null), null);
  // Either companion field proves the new contract; zero stays a valid value.
  equal(readAverageTpsField({ average_tps: 58.3, overall_tps: 58.3 }), 58.3);
  equal(readAverageTpsField({ average_tps: 0, valid_tps_count: 3 }), 0);
  equal(readAverageTpsField({ average_tps: 12.5, overall_tps: null, valid_tps_count: 4 }), 12.5);
  // Invalid values stay unavailable even when companions exist.
  equal(readAverageTpsField({ average_tps: null, valid_tps_count: 0 }), null);
  equal(readAverageTpsField({ average_tps: -5, valid_tps_count: 2 }), null);
  equal(readAverageTpsField({ average_tps: NaN, overall_tps: NaN }), null);
});

test("effective TPS excludes reasoning tokens while gross TPS preserves total", () => {
  const log: TpsInput = {
    output_tokens: 100,
    reasoning_tokens: 80,
    latency_upstream_ms: 2000,
    latency_total_ms: 2100,
  };
  equal(contentTokensOf(log), 20);
  equal(computeTps(log), 10);
  equal(computeGrossTps(log), 50);
});

test("legacy formula expectations are replaced by the end-to-end contract", () => {
  // Old net-generation timing subtracted TTFT (2007 tokens / (20617-1798) ms = 106.6).
  // The unified main metric divides by the full upstream duration instead.
  equal(computeTps({ output_tokens: 2007, reasoning_tokens: 0, latency_upstream_ms: 20617, latency_total_ms: 21000 }), 2007 / 20.617);
  equal(formatTps(computeTps({ output_tokens: 2007, reasoning_tokens: 0, latency_upstream_ms: 20617, latency_total_ms: 21000 })), "97.3 tok/s");
});
