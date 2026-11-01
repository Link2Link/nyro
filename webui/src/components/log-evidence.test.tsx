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

test("Devin and legacy native blocks explain digest-only evidence without claiming every native body uses it", () => {
  for (const isZh of [false, true]) {
    const html = renderToStaticMarkup(<PayloadBlock title="Upstream response" log={{ payload_metadata: JSON.stringify(metadata) }} field="upstream_response_body" isZh={isZh} />);
    assert.match(html, new RegExp(sha256));
    assert.match(html, /disabled=""/);
    assert.match(html, isZh ? /仅保存摘要，未保留原始正文/ : /Digest only/);
    assert.match(html, isZh ? /摘要仅覆盖已观测字节/ : /digest covers observed bytes only/);
    assert.match(html, isZh ? /旧版原生捕获或供应商安全策略/ : /legacy native capture or a provider safety policy/);
    assert.match(html, isZh ? /Kiro 新记录支持有界原文捕获/ : /New Kiro records support bounded raw capture/);
    assert.doesNotMatch(html, /native payload retains only a digest by policy|此原生载荷按策略仅记录摘要/);
    assert.doesNotMatch(html, /role="alert"|采集元数据或证据无效|Invalid capture metadata|head and tail below/);
  }
});

test("captured Kiro UTF-8/Base64 bodies show hash/chunks, enable copy and never claim digest-only", () => {
  for (const isZh of [false, true]) {
    for (const encoding of ["utf8", "base64"] as const) {
      const bytes = encoding === "utf8" ? Buffer.from('{"message":"你好"}') : Buffer.from([0, 0xff, 0x80, 10]);
      const raw = bytes.toString(encoding === "utf8" ? "utf8" : "base64");
      const captured = { ...metadata.upstream_response_body, capture_state: "captured", encoding,
        total_observed_bytes: bytes.length, retained_bytes: bytes.length, head_bytes: bytes.length, complete: false, chunks: 2 };
      const html = renderToStaticMarkup(<PayloadBlock title="Kiro response" log={{ upstream_response_body: raw,
        payload_metadata: JSON.stringify({ upstream_response_body: captured }) }} field="upstream_response_body" isZh={isZh} />);
      assert.match(html, new RegExp(sha256));
      assert.match(html, isZh ? /已观测非空数据块.*2/ : /Observed non-empty chunks.*2/);
      assert.match(html, isZh ? /传输采集不完整/ : /Capture incomplete/);
      assert.match(html, isZh ? /全部已观测原始字节/ : /all observed raw bytes/);
      assert.match(html, new RegExp(encoding));
      assert.doesNotMatch(html, /disabled=""|Digest only|仅保存摘要|role="alert"/);
    }
  }
});

test("truncated native block marks the missing middle without presenting a full body", () => {
  const captured = { ...metadata.upstream_response_body, capture_state: "captured", encoding: "base64", retained_bytes: 4,
    head_bytes: 2, tail_bytes: 2, truncated: true, complete: true };
  const html = renderToStaticMarkup(<PayloadBlock title="Kiro response" log={{ upstream_response_body: "AP8QgA==",
    payload_metadata: JSON.stringify({ upstream_response_body: captured }) }} field="upstream_response_body" isZh={false} />);
  assert.match(html, /148 observed bytes missing from the middle/);
  assert.match(html, /head and tail below are not contiguous/);
  assert.match(html, /Truncated/);
  assert.match(html, /including any omitted middle/);
  assert.doesNotMatch(html, /Digest only|disabled=""|role="alert"/);
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
