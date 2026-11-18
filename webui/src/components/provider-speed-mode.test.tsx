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
        match(html, isZh ? /客户端已指定的 service_tier 不会被覆盖/ : /client-supplied service_tier is never overwritten/);
      }
    }
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
