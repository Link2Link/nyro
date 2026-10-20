import assert from "node:assert/strict";
import test from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import { AttemptResultBanner, PayloadBlock } from "./log-evidence";
import type { RequestLog } from "../lib/types";

const sha256 = "6ed79909fffda6b662a4094b5c3d9a31f0fc44b791353b286cd7dc636c68f0cd";
const metadata = {
  upstream_response_body: {
    capture_state: "digest_only", encoding: "binary", total_observed_bytes: 152,
    retained_bytes: 0, head_bytes: 0, tail_bytes: 0, truncated: false,
    complete: false, chunks: 1, sha256,
  },
  devin_upstream_error: {
    source: "connect_trailer", code: "permission_denied",
    message: "an internal error occurred <script>not executable</script>", sanitized: true,
  },
};

function banner(payload: unknown, isZh = false) {
  return renderToStaticMarkup(<AttemptResultBanner
    log={{ payload_metadata: JSON.stringify(payload), client_status_code: 403, upstream_status_code: 200 } as RequestLog}
    correlationLoading={false} correlationError={false} onSelect={() => {}} isZh={isZh}
  />);
}

test("native response block shows digest and disables body copy instead of invalid evidence", () => {
  for (const isZh of [false, true]) {
    const html = renderToStaticMarkup(<PayloadBlock title="Upstream response" log={{ payload_metadata: JSON.stringify(metadata) }} field="upstream_response_body" isZh={isZh} />);
    assert.match(html, new RegExp(sha256));
    assert.match(html, /disabled=""/);
    assert.match(html, isZh ? /仅保存摘要，未保留原始正文/ : /Digest only/);
    assert.match(html, isZh ? /摘要仅覆盖已观测字节/ : /digest covers observed bytes only/);
    assert.doesNotMatch(html, /role="alert"|采集元数据或证据无效|Invalid capture metadata|head and tail below/);
  }
});

test("Devin diagnostic banner displays escaped text with source distinct from local classification", () => {
  const html = banner(metadata);
  assert.match(html, /permission_denied/);
  assert.match(html, /Connect error trailer/);
  assert.match(html, /not the raw response body/);
  assert.match(html, /&lt;script&gt;not executable&lt;\/script&gt;/);
  assert.doesNotMatch(html, /<script>/);
  assert.match(banner(metadata, true), /Devin 上游错误诊断（已脱敏）/);
  assert.match(banner({ devin_upstream_error: { ...metadata.devin_upstream_error, source: "http_error" } }), /HTTP error response/);
});

test("invalid diagnostics are not echoed and legacy entries get no diagnostic section", () => {
  const html = banner({ devin_upstream_error: { ...metadata.devin_upstream_error, sanitized: false, message: "do-not-display" } });
  assert.match(html, /Invalid upstream diagnostic metadata/);
  assert.doesNotMatch(html, /do-not-display/);
  assert.doesNotMatch(banner({}), /Devin upstream error diagnostic|Invalid upstream diagnostic/);
});
