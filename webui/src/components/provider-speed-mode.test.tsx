import { deepEqual, equal, match } from "node:assert/strict";
import { test } from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import { speedModeForChannel } from "../lib/provider-speed-mode";
import type { OpenAiSpeedMode } from "../lib/types";
import { ProviderSpeedMode } from "./provider-speed-mode";

function render(channel?: string, value?: OpenAiSpeedMode, isZh = false) {
  return renderToStaticMarkup(<ProviderSpeedMode channel={channel} value={value} isZh={isZh} onChange={() => {}} />);
}

test("Codex and sub2api show the three localized options and select the saved tier", () => {
  for (const channel of ["codex", "sub2api"]) {
    for (const mode of [undefined, "normal", "fast", "ultrafast"] as const) {
      for (const isZh of [false, true]) {
        const html = render(channel, mode, isZh);
        const options = [...html.matchAll(/<option value="([^"]+)"( selected="")?>([^<]+)<\/option>/g)];
        deepEqual(options.map(([, value, , label]) => [value, label]), [
          ["normal", isZh ? "普通" : "Normal"], ["fast", "Fast"], ["ultrafast", "Ultrafast"],
        ]);
        deepEqual(options.filter(([, , selected]) => selected).map(([, value]) => value), [mode ?? "normal"]);
      }
    }
  }
});

test("Codex and sub2api explain tier preservation and overrides in English and Chinese", () => {
  for (const channel of ["codex", "sub2api"]) {
    const en = render(channel);
    match(en, /OpenAI Responses requests to Codex or Sub2API/);
    match(en, /Normal neither adds nor changes service_tier/);
    match(en, /Fast and Ultrafast force the upstream service_tier to fast and ultrafast, respectively, even if the client already supplied a tier/);

    const zh = render(channel, undefined, true);
    match(zh, /发往 Codex 或 Sub2API 的 OpenAI Responses 请求/);
    match(zh, /普通模式既不添加也不改写 service_tier/);
    match(zh, /Fast 和 Ultrafast 分别将上游请求的 service_tier 强制设为 fast 和 ultrafast，即使客户端已指定档位/);
  }
});

test("new and legacy provider modes produce only canonical request strings", () => {
  for (const channel of ["codex", "sub2api"]) {
    for (const [saved, mode] of [[true, "fast"], [false, "normal"], [undefined, "normal"], ["normal", "normal"],
      ["fast", "fast"], ["ultrafast", "ultrafast"], ["unexpected", "normal"]] as const) {
      equal(speedModeForChannel(channel, saved), mode);
    }
  }
});

test("switching to an ineligible channel hides the control and clears the previous tier", () => {
  const tier = speedModeForChannel("codex", "ultrafast");
  equal(speedModeForChannel("sub2api", tier), "ultrafast");
  for (const channel of [undefined, "", "default", "claude-code"]) {
    equal(render(channel, tier), "");
    const cleared = speedModeForChannel(channel, tier);
    equal(cleared, "normal");
    equal(speedModeForChannel("codex", cleared), "normal");
  }
});
