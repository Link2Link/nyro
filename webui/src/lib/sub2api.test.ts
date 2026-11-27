import { equal } from "node:assert/strict";
import { test } from "node:test";
import { speedModeForChannel, supportsSpeedMode } from "./provider-speed-mode";
import {
  detectionKey, evidenceText, isLegacySub2ApiChannel, isSub2ApiPreset, isSub2ApiProvider, platformLabel,
  platformMismatch, relaysSub2Api, sub2apiHintText, sub2apiUpstreamUsage,
} from "./sub2api";
import type { Sub2ApiPlatformDetection } from "./types";

test("Sub2API identity follows the preset or vendor key, not a channel name", () => {
  equal(isSub2ApiPreset(" Sub2API "), true);
  equal(isSub2ApiPreset("openai"), false);
  equal(isSub2ApiProvider({ vendor: "sub2api", preset_key: null }), true);
  equal(isSub2ApiProvider({ vendor: null, preset_key: "sub2api" }), true);
  equal(isSub2ApiProvider({ vendor: "openai", preset_key: "openai" }), false);
  equal(isLegacySub2ApiChannel(" Sub2API "), true);
  equal(isLegacySub2ApiChannel("openai"), false);
});

test("the speed tier covers Codex consumer channels only", () => {
  equal(supportsSpeedMode("codex"), true);
  equal(supportsSpeedMode("sub2api"), true);
  equal(supportsSpeedMode("openai", "sub2api"), true);
  // Same channel name on other presets, and other Sub2API platforms, carry no tier.
  equal(supportsSpeedMode("openai"), false);
  equal(supportsSpeedMode("openai", "vertexai"), false);
  equal(supportsSpeedMode("grok", "sub2api"), false);
  equal(speedModeForChannel("openai", "ultrafast", "sub2api"), "ultrafast");
  equal(speedModeForChannel("openai", true, "sub2api"), "fast");
  equal(speedModeForChannel("anthropic", "fast", "sub2api"), "normal");
  equal(speedModeForChannel("openai", "fast"), "normal");
});

test("one usable key reads Sub2API quota upstream; pools and keyless rows stay local", () => {
  const relay = { vendor: "sub2api", preset_key: "sub2api", channel: "antigravity" };
  equal(relaysSub2Api(relay), true);
  equal(relaysSub2Api({ vendor: "openai", preset_key: "openai", channel: "sub2api" }), true);
  equal(relaysSub2Api({ vendor: "nyro-gateway", preset_key: "nyro-gateway", channel: "sub2api" }), false);
  equal(relaysSub2Api({ vendor: "google", preset_key: "google", channel: "antigravity" }), false);
  equal(sub2apiUpstreamUsage({ ...relay, api_key: "sk" }), true);
  equal(sub2apiUpstreamUsage({ ...relay, api_key: " " }), false);
  equal(sub2apiUpstreamUsage({ ...relay, api_key: "", protocol_endpoints: [{ api_key: "sk", is_enabled: true }] }), true);
  equal(sub2apiUpstreamUsage({ ...relay, api_key: "", protocol_endpoints: [{ api_key: "sk", is_enabled: false }] }), false);
  equal(sub2apiUpstreamUsage({ ...relay, api_key: "sk", keys: [{ id: "k1" }] }), false);
  equal(sub2apiUpstreamUsage({ vendor: "openai", preset_key: "openai", channel: "default", api_key: "sk" }), false);
});

function detection(platform: string | null, confidence: Sub2ApiPlatformDetection["confidence"]): Sub2ApiPlatformDetection {
  return { platform, confidence, candidates: [], model_count: 0, sample_models: [], evidence: [] };
}

test("only a confident, different suggestion warns at save time", () => {
  equal(platformMismatch(detection("antigravity", "high"), "anthropic"), true);
  equal(platformMismatch(detection("antigravity", "medium"), " Antigravity "), false);
  equal(platformMismatch(detection("antigravity", "low"), "anthropic"), false);
  equal(platformMismatch(detection(null, "medium"), "anthropic"), false);
  equal(platformMismatch(null, "anthropic"), false);
  // A changed address or key invalidates a remembered result.
  equal(detectionKey(" https://s.example ", "sk "), detectionKey("https://s.example", "sk"));
  equal(detectionKey("https://s.example", "sk") === detectionKey("https://s.example", "sk2"), false);
});

test("probe and attempt hints preserve the original HTTP diagnosis", () => {
  const hint = "This Sub2API OpenAI group does not allow /v1/messages. Uncheck Anthropic Messages.";
  const error = "HTTP 403: upstream refused — " + hint;
  equal(sub2apiHintText(error, false), error);
  equal(sub2apiHintText(error, true), "HTTP 403: upstream refused — " + sub2apiHintText(hint, true));
  equal(sub2apiHintText(hint, true).includes("取消勾选"), true);
  equal(sub2apiHintText("HTTP 404", true), "HTTP 404");
});

test("evidence and platform labels are localized", () => {
  equal(evidenceText("The key is rejected on /v1beta (not a Gemini group).", true), "该 Key 无法访问 /v1beta（不是 Gemini 分组）。");
  equal(evidenceText("Only DeepSeek models are listed.", true), "只列出了 DeepSeek 模型。");
  equal(evidenceText("The model list uses Sub2API's OpenAI format.", true), "模型列表是 Sub2API 的 OpenAI 格式。");
  equal(evidenceText("Something new.", true), "Something new.");
  equal(evidenceText("Only DeepSeek models are listed.", false), "Only DeepSeek models are listed.");
  const channels = [{ id: "zhipu", label: { zh: "智谱 GLM", en: "Zhipu GLM" }, baseUrls: {} }];
  equal(platformLabel(channels, "zhipu", true), "智谱 GLM");
  equal(platformLabel(channels, "zhipu", false), "Zhipu GLM");
  equal(platformLabel(channels, "unknown", false), "unknown");
});
