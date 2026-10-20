import assert from "node:assert/strict";
import test from "node:test";
import { effectiveOutcome, isLogRelatedQueryKey, outcomeLabel, outcomeQuery, parseErrorCauses, parseDevinUpstreamDiagnostic, payloadDownload, payloadEvidence, payloadStateLabel, usageOutcomeRates, type PayloadMetadata } from "./log-observability.ts";
import type { RequestLog } from "./types.ts";

const field = "upstream_response_body";
function evidence(raw: string | undefined, metadata: Partial<PayloadMetadata> = {}) {
  const bytes = new TextEncoder().encode(raw ?? "").length;
  return {
    [field]: raw,
    payload_metadata: JSON.stringify({ [field]: {
      total_observed_bytes: bytes, retained_bytes: bytes, head_bytes: bytes, tail_bytes: 0,
      truncated: false, complete: true, encoding: "utf8", capture_state: bytes ? "captured" : "empty",
      ...metadata,
    } }),
  };
}

test("only backend effective outcome classifies; HTTP 200 and old data cannot invent success", () => {
  assert.equal(effectiveOutcome({ client_status_code: 200, effective_outcome: "error", is_error: true, attempt_outcome: "timed_out" }), "error");
  assert.equal(effectiveOutcome({ client_status_code: 200 }), "unknown");
  assert.equal(effectiveOutcome({ client_status_code: 500 }), "unknown");
  assert.equal(effectiveOutcome({ effective_outcome: "unknown", error_message: "historical timeout" }), "unknown");
  assert.equal(effectiveOutcome({ effective_outcome: "cancelled", attempt_outcome: "cancelled" }), "cancelled");
  assert.equal(effectiveOutcome({ effective_outcome: "output_limited" }), "output_limited");
});

test("error filter delegates is_error to core and remains AND-composable with raw HTTP 200", () => {
  assert.deepEqual({ status_min: 200, status_max: 200, ...outcomeQuery("error") }, { status_min: 200, status_max: 200, is_error: true, outcome: undefined });
  assert.deepEqual(outcomeQuery("unknown"), { is_error: undefined, outcome: "unknown" });
  assert.deepEqual(outcomeQuery("all"), { is_error: undefined, outcome: undefined });
});

test("completed and unknown rates use total attempts, never one minus error rate", () => {
  const detail = { request_count: 10, success_count: 3, error_count: 2, unknown_count: 3, cancelled_count: 1, output_limited_count: 1, outcome_stats_version: 1 };
  assert.deepEqual(usageOutcomeRates(detail), { authoritative: true, completedRate: 30, unknownRate: 30 });
  for (const invalid of [{ ...detail, outcome_stats_version: 0 }, { ...detail, unknown_count: undefined }, { ...detail, success_count: NaN }, { ...detail, error_count: -1 }, { ...detail, success_count: 9 }]) {
    assert.deepEqual(usageOutcomeRates(invalid), { authoritative: false, completedRate: null, unknownRate: null });
  }
  assert.deepEqual(usageOutcomeRates(undefined), { authoritative: false, completedRate: null, unknownRate: null });
  assert.deepEqual(usageOutcomeRates({ ...detail, request_count: 0, success_count: 0, error_count: 0, unknown_count: 0, cancelled_count: 0, output_limited_count: 0 }), { authoritative: true, completedRate: null, unknownRate: null });
  assert.equal(outcomeLabel("completed"), "Completed");
  assert.equal(outcomeLabel("unknown", true), "未知");
});

test("error causes are a string array or untrusted malformed plain text", () => {
  assert.deepEqual(parseErrorCauses('["outer","<script>alert(1)</script>"]'), { causes: ["outer", "<script>alert(1)</script>"], malformed: false });
  assert.deepEqual(parseErrorCauses('{"oops":1}'), { causes: ['{"oops":1}'], malformed: true });
  assert.deepEqual(parseErrorCauses('broken'), { causes: ["broken"], malformed: true });
});

test("pretty JSON only when capture is complete, untruncated UTF-8", () => {
  const raw = '{"a":1}';
  assert.equal(payloadEvidence(evidence(raw), field).segments[0].text, '{\n  "a": 1\n}');
  assert.equal(payloadEvidence(evidence(raw, { complete: false }), field).segments[0].text, raw);
  assert.equal(payloadEvidence({ [field]: raw }, field).segments[0].text, raw);
  assert.equal(payloadEvidence({ [field]: raw }, field).state, "unknown");
  assert.equal(payloadEvidence({}, field).state, "unknown");
  assert.equal(payloadEvidence(evidence(""), field).state, "empty");
});

test("truncated UTF-8 splits at byte offset rather than character count", () => {
  const parsed = payloadEvidence(evidence("你好END", { total_observed_bytes: 100, retained_bytes: 9, head_bytes: 6, tail_bytes: 3, truncated: true }), field);
  assert.equal(parsed.state, "captured");
  assert.equal(parsed.missingBytes, 91);
  assert.deepEqual(parsed.segments, [{ label: "head", text: "你好", encoding: "utf8" }, { label: "tail", text: "END", encoding: "utf8" }]);
});

test("base64 split retains exact raw bytes and broken UTF-8 stays base64", () => {
  const bytes = new Uint8Array([0xe4, 0xbd, 0xa0, 0xff]);
  const raw = Buffer.from(bytes).toString("base64");
  const parsed = payloadEvidence(evidence(raw, { total_observed_bytes: 11, retained_bytes: 4, head_bytes: 2, tail_bytes: 2, encoding: "base64", truncated: true }), field);
  assert.equal(parsed.state, "captured");
  assert.deepEqual(parsed.segments.map((part) => Array.from(Buffer.from(part.text, "base64"))), [[0xe4, 0xbd], [0xa0, 0xff]]);
  assert.ok(parsed.segments.every((part) => part.encoding === "base64"));
  const falseUtf8 = payloadEvidence(evidence("你好", { total_observed_bytes: 9, head_bytes: 2, tail_bytes: 4, truncated: true }), field);
  assert.equal(falseUtf8.state, "invalid");
  assert.equal(falseUtf8.segments[0].text, "你好");
});

test("malformed metadata, missing capture and invalid base64 cannot display false empty/complete", () => {
  assert.equal(payloadEvidence({ payload_metadata: "not-json" }, field).state, "invalid");
  assert.equal(payloadEvidence(evidence(undefined, { capture_state: "captured", total_observed_bytes: 5, retained_bytes: 5, head_bytes: 5 }), field).state, "missing");
  assert.equal(payloadEvidence(evidence(undefined, { capture_state: "absent", encoding: "none" }), field).state, "absent");
  assert.equal(payloadEvidence(evidence("@@", { encoding: "base64" }), field).state, "invalid");
  assert.equal(payloadEvidence(evidence("abc", { head_bytes: 4 }), field).state, "invalid");
  assert.equal(payloadEvidence(evidence("abc", { capture_state: "empty", retained_bytes: 0, total_observed_bytes: 0, head_bytes: 0 }), field).state, "invalid");
  assert.equal(payloadEvidence(evidence("abc", { capture_state: "absent", retained_bytes: 0, total_observed_bytes: 0, head_bytes: 0, encoding: "none" }), field).state, "invalid");
});

test("download keeps complete JSON bytes raw while carrying metadata and truncation marks", () => {
  const complete = { id: "id", ...evidence('{"x": 1}') } as RequestLog;
  const output = payloadDownload(complete);
  assert.match(output, /\{"x": 1\}/);
  assert.match(output, /"truncated":false/);
  const truncated = { id: "id", ...evidence("HEADTAIL", { total_observed_bytes: 99, head_bytes: 4, tail_bytes: 4, truncated: true }) } as RequestLog;
  assert.match(payloadDownload(truncated), /Missing observed bytes: 91/);
  assert.match(payloadDownload(truncated), /# head \(utf8\)\nHEAD/);
  assert.match(payloadDownload(truncated), /# tail \(utf8\)\nTAIL/);
});

test("recording-disabled metadata remains distinct from missing, empty and manual clear", () => {
  for (const lengths of [{ retained_bytes: 0, head_bytes: 0, tail_bytes: 0 }, { retained_bytes: 8, head_bytes: 4, tail_bytes: 4 }]) {
    const log = evidence(undefined, { total_observed_bytes: 8, ...lengths, truncated: false, capture_state: "not_retained" });
    const parsed = payloadEvidence(log, field);
    assert.equal(parsed.state, "not_retained");
    assert.equal(parsed.missingBytes, null);
    assert.deepEqual(parsed.segments, []);
    assert.equal(payloadEvidence({ ...log, payload_cleared_at: 3 }, field).state, "cleared");
  }
});

test("header metadata has distinct raw/redacted accounting and omission markers", () => {
  const raw = '{"authorization":"***"}';
  const log = { upstream_request_headers: raw, payload_metadata: JSON.stringify({ upstream_request_headers: { total_observed_bytes: 100, retained_bytes: raw.length, complete: true, truncated: true, encoding: "utf8", capture_state: "captured", omitted_headers: 2 } }) };
  const parsed = payloadEvidence(log, "upstream_request_headers");
  assert.equal(parsed.state, "captured");
  assert.equal(parsed.missingBytes, null);
  assert.equal(parsed.metadata?.omitted_headers, 2);
  assert.equal(parsed.segments[0].text, raw);
});

test("manual clear overrides every retained body/header without altering result classification", () => {
  const log = { id: "attempt-id", ...evidence("secret"), payload_cleared_at: 1, effective_outcome: "error", is_error: true, client_request_headers: "credentials" } as RequestLog;
  assert.equal(payloadEvidence(log, field).state, "cleared");
  assert.equal(payloadEvidence(log, "client_request_headers").state, "cleared");
  assert.equal(effectiveOutcome(log), "error");
  assert.equal(payloadStateLabel("cleared"), "Manually cleared");
  assert.equal(payloadStateLabel("cleared", true), "已手动清除");
  const output = payloadDownload(log);
  assert.match(output, /Payload cleared at: 1/);
  assert.match(output, /Payload metadata/);
  assert.doesNotMatch(output, /secret|credentials/);
});

const nativeDigest: Partial<PayloadMetadata> = {
  capture_state: "digest_only", encoding: "binary", total_observed_bytes: 152,
  retained_bytes: 0, head_bytes: 0, tail_bytes: 0, truncated: false,
  complete: false, chunks: 1, sha256: "6ed79909fffda6b662a4094b5c3d9a31f0fc44b791353b286cd7dc636c68f0cd",
};

test("live Devin digest-only response is valid evidence, not a missing or truncated body", () => {
  for (const complete of [false, true]) {
    const log = evidence(undefined, { ...nativeDigest, complete });
    const parsed = payloadEvidence(log, field);
    assert.equal(parsed.state, "digest_only");
    assert.equal(parsed.issue, undefined);
    assert.equal(parsed.missingBytes, null);
    assert.deepEqual(parsed.segments, []);
    assert.equal(parsed.metadata?.complete, complete);
    assert.equal(parsed.metadata?.sha256, nativeDigest.sha256);
    const downloaded = payloadDownload(log as RequestLog);
    assert.match(downloaded, /Digest only/);
    assert.match(downloaded, new RegExp(nativeDigest.sha256!));
    assert.doesNotMatch(downloaded, /Invalid payload|Missing observed bytes: 152/);
    assert.equal(payloadEvidence({ ...log, payload_cleared_at: 1 }, field).state, "cleared");
  }
  assert.equal(payloadStateLabel("digest_only", true), "仅保存摘要，未保留原始正文");
  assert.equal(payloadEvidence(evidence(undefined, { ...nativeDigest, capture_state: "not_retained" }), field).state, "not_retained");
  assert.equal(payloadEvidence(evidence(undefined, { ...nativeDigest, total_observed_bytes: 0, chunks: 0, sha256: null, capture_state: "absent" }), field).state, "absent");
  assert.equal(payloadEvidence(evidence(undefined, { ...nativeDigest, total_observed_bytes: 0, chunks: 0 }), field).state, "digest_only");
});

test("native digest contradictions fail closed without loosening ordinary payload validation", () => {
  for (const patch of [
    { retained_bytes: 1 }, { head_bytes: 1 }, { tail_bytes: 1 }, { truncated: true },
    { encoding: "utf8" }, { capture_state: "captured" }, { chunks: -1 }, { chunks: 0 },
    { sha256: null }, { sha256: "invalid" }, { total_observed_bytes: -1 },
  ] as Partial<PayloadMetadata>[]) {
    assert.equal(payloadEvidence(evidence(undefined, { ...nativeDigest, ...patch }), field).state, "invalid", JSON.stringify(patch));
  }
  assert.equal(payloadEvidence(evidence("unexpected bytes", nativeDigest), field).state, "invalid");
  assert.equal(payloadEvidence({ payload_metadata: JSON.stringify({ upstream_response_headers: nativeDigest }) }, "upstream_response_headers").state, "invalid");
});

test("Devin diagnostic parser accepts bounded sanitized admin metadata only", () => {
  const diagnostic = { source: "connect_trailer", code: "permission_denied", message: "an internal error occurred (trace ID: fixture)", sanitized: true };
  const wrap = (value: unknown) => JSON.stringify({ devin_upstream_error: value });
  assert.deepEqual(parseDevinUpstreamDiagnostic(wrap({ ...diagnostic, details: "not displayed" })), {
    diagnostic: { source: diagnostic.source, code: diagnostic.code, message: diagnostic.message }, malformed: false,
  });
  assert.equal(parseDevinUpstreamDiagnostic(wrap({ ...diagnostic, source: "http_error", code: null })).diagnostic?.source, "http_error");
  assert.deepEqual(parseDevinUpstreamDiagnostic(undefined), { diagnostic: null, malformed: false });
  assert.deepEqual(parseDevinUpstreamDiagnostic('{}'), { diagnostic: null, malformed: false });
  for (const value of [false, [], "raw", { ...diagnostic, sanitized: false }, { ...diagnostic, source: "unknown" },
    { ...diagnostic, message: "x".repeat(513) }, { ...diagnostic, message: "错".repeat(171) },
    { ...diagnostic, code: "x".repeat(65) }, { ...diagnostic, code: null, message: null }]) {
    assert.deepEqual(parseDevinUpstreamDiagnostic(wrap(value)), { diagnostic: null, malformed: true });
  }
  assert.equal(parseDevinUpstreamDiagnostic('broken').malformed, true);
});

test("all destructive actions invalidate logs, details, correlation and usage/stat query families", () => {
  for (const family of ["logs", "log-detail", "request-log-attempts", "stats", "stats-overview", "stats-timeseries", "stats-models", "stats-apikeys", "stats-providers", "provider-usage-list", "provider-usage-detail", "provider-usage-logs", "api-key-usage-list", "api-key-usage-detail", "api-key-usage-logs", "model-usage-list", "model-usage-detail", "model-usage-logs", "model-performance"]) assert.equal(isLogRelatedQueryKey([family, 24]), true, family);
  assert.equal(isLogRelatedQueryKey(["setting", "proxy_enabled"]), false);
});
