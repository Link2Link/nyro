import { equal } from "node:assert/strict";
import { test } from "node:test";
import { computeTps, formatTps, generationMsOf } from "./format";

test("TPS display consistently uses one decimal, including values above 100", () => {
  for (const [value, expected] of [
    [58.333333333333, "58.3 tok/s"],
    [99.96, "100.0 tok/s"],
    [100, "100.0 tok/s"],
    [225.678, "225.7 tok/s"],
    [1000, "1000.0 tok/s"],
    [0.01, "0.0 tok/s"],
  ] as const) equal(formatTps(value), expected);
});

test("log TPS follows model-statistics stream detection and duration fallback", () => {
  equal(computeTps({ output_tokens: 100, is_stream: false, stream_chunks_count: 3, latency_upstream_ms: 2000, stream_first_chunk_ms: 500 }), 100 / 1.5);
  equal(computeTps({ output_tokens: 50, is_stream: false, stream_chunks_count: 0, latency_upstream_ms: 1000 }), 50);
  equal(computeTps({ output_tokens: 100, latency_total_ms: 2000 }), 50);
  equal(computeTps({ output_tokens: 100, is_stream: true, latency_upstream_ms: 1000, stream_first_chunk_ms: 950 }), 100);
  equal(generationMsOf({ is_stream: true, latency_upstream_ms: 0, stream_first_chunk_ms: -100 }), 0);
  equal(computeTps({ output_tokens: 100, is_stream: true, latency_upstream_ms: 0, stream_first_chunk_ms: -100 }), null);
  equal(formatTps(computeTps({ output_tokens: 2007, is_stream: true, latency_upstream_ms: 20617, stream_first_chunk_ms: 1798 })), "106.6 tok/s");
});

test("unavailable and invalid TPS retain the missing-value marker", () => {
  for (const value of [null, undefined, NaN, Infinity, -Infinity, -1, 0]) {
    equal(formatTps(value), "–");
  }
});
