import assert from "node:assert/strict";
import test from "node:test";
import { effectiveOutcome, isLogRelatedQueryKey, outcomeLabel, outcomeQuery, parseErrorCauses, payloadDownload, payloadEvidence, payloadStateLabel, usageOutcomeRates, type PayloadMetadata } from "./log-observability.ts";
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

test("all destructive actions invalidate logs, details, correlation and usage/stat query families", () => {
  for (const family of ["logs", "log-detail", "request-log-attempts", "stats", "stats-overview", "stats-timeseries", "stats-models", "stats-apikeys", "stats-providers", "provider-usage-list", "provider-usage-detail", "provider-usage-logs", "api-key-usage-list", "api-key-usage-detail", "api-key-usage-logs", "model-usage-list", "model-usage-detail", "model-usage-logs", "model-performance"]) assert.equal(isLogRelatedQueryKey([family, 24]), true, family);
  assert.equal(isLogRelatedQueryKey(["setting", "proxy_enabled"]), false);
});
