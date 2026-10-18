import { deepEqual, equal } from "node:assert/strict";
import { test } from "node:test";
import { groupProviders, isGroupExpanded, isStandaloneGroup, parseGroupExpansion, providerGroupKey, providerNeedsAttention } from "./provider-groups";
import type { Provider, ProviderPreset } from "./types";

function provider(id: string, overrides: Partial<Provider> = {}): Provider {
  return {
    id, name: id, protocol: "openai-compatible", base_url: "https://relay.example/v1",
    is_enabled: true, use_proxy: false, fast_mode: false, created_at: "", updated_at: "",
    ...overrides,
  };
}
const presets: ProviderPreset[] = [
  { id: "google", label: { en: "Google", zh: "谷歌" }, defaultProtocol: "google-gemini" },
  { id: "openai", label: { en: "OpenAI", zh: "OpenAI" }, defaultProtocol: "openai-compatible" },
];

test("canonical preset wins; custom and blank presets fall back to explicit vendor", () => {
  equal(providerGroupKey({ preset_key: " OpenAI ", vendor: "google" }), "openai");
  equal(providerGroupKey({ preset_key: "custom", vendor: " Google " }), "google");
  equal(providerGroupKey({ preset_key: " ", vendor: "Google" }), "google");
  equal(providerGroupKey({ preset_key: "custom", vendor: " CUSTOM " }), "custom");
  equal(providerGroupKey({}), "custom");
});

test("same brand merges channels and protocols without branding compatible relays as OpenAI", () => {
  const groups = groupProviders([
    provider("pro", { preset_key: "openai", auth_mode: "oauth", channel: "codex" }),
    provider("UUAPI", { preset_key: "custom", protocol: "openai-responses" }),
    provider("api", { vendor: "OpenAI", auth_mode: "apikey" }),
    provider("GPT relay", { base_url: "https://api.openai.com/v1" }),
  ], presets, false);
  deepEqual(groups.map((g) => [g.key, g.providers.map((p) => p.id)]), [
    ["openai", ["pro", "api"]], ["custom", ["UUAPI", "GPT relay"]],
  ]);
});

test("group and member ordering remains stable across state updates", () => {
  const items = [provider("g", { vendor: "google" }), provider("o", { vendor: "openai" }), provider("g2", { vendor: "google" })];
  const ordering = (items: Provider[]) => groupProviders(items, presets, false).map((g) => [g.key, g.providers.map((p) => p.id)]);
  deepEqual(ordering(items), ordering(items.map((p) => ({ ...p, is_enabled: false, last_test_success: false }))));
});

test("search keeps totals and matches names, localized brands, channel and protocols case-insensitively", () => {
  const items = [provider("Pro", { vendor: "google", channel: "gemini-cli" }), provider("Backup", { vendor: "google" })];
  equal(groupProviders(items, presets, true, "谷歌")[0].matches.length, 2);
  const match = groupProviders(items, presets, false, "  PRO  ")[0];
  equal(match.providers.length, 2);
  deepEqual(match.matches.map((p) => p.id), ["Pro"]);
  equal(groupProviders(items, presets, false, "GEMINI-CLI")[0].matches.length, 1);
  equal(groupProviders(items, presets, false, "openai-compatible")[0].matches.length, 2);
  equal(groupProviders(items, presets, false, "missing")[0].matches.length, 0);
  equal(groupProviders(items, presets, false, "")[0].matches.length, 2);
});

test("labels use preset locale with English fallback and named custom vendors remain distinct", () => {
  equal(groupProviders([provider("g", { vendor: "google" })], presets, false)[0].label, "Google");
  equal(groupProviders([provider("c")], presets, false)[0].label, "Custom");
  equal(groupProviders([provider("c")], presets, true)[0].label, "自定义");
  deepEqual(groupProviders([provider("a", { vendor: "Relay A" }), provider("b", { vendor: "Relay B" })], [], false).map((g) => g.key), ["relay a", "relay b"]);
});

test("collapse persistence rejects corrupt values, supports special keys, and defaults to collapsed for multi-provider groups", () => {
  for (const raw of [null, "invalid", "null", "[]", "true", "42"]) deepEqual(parseGroupExpansion(raw), {});
  deepEqual(parseGroupExpansion('{"openai":false,"google":true,"bad":"yes"}'), { openai: false, google: true });
  const single = groupProviders([provider("one", { vendor: "openai" })], presets, false)[0];
  const multiple = groupProviders([provider("one"), provider("two")], presets, false)[0];
  equal(isStandaloneGroup(single), true);
  equal(isStandaloneGroup(multiple), false);
  equal(isGroupExpanded(multiple, {}), false);
  equal(isGroupExpanded(single, { openai: true }), true);
  equal(isGroupExpanded(multiple, { custom: true }), true);
  const unusual = groupProviders([provider("x", { vendor: "constructor" })], [], false)[0];
  equal(isGroupExpanded(unusual, {}), false);
  equal(isGroupExpanded(unusual, parseGroupExpansion('{"constructor":true}')), true);
});

test("attention counts failed tests and OAuth issues, never infers health from enabled state", () => {
  equal(providerNeedsAttention(provider("new")), false);
  equal(providerNeedsAttention(provider("off", { is_enabled: false })), false);
  equal(providerNeedsAttention(provider("failed", { last_test_success: false })), true);
  equal(providerNeedsAttention(provider("passed", { last_test_success: false }), { success: true, latency_ms: 1 }), false);
  equal(providerNeedsAttention(provider("failed"), { success: false, latency_ms: 1 }), true);
  for (const oauth_status of ["error", "disconnected", "not_connected", "unavailable", "quota_exhausted"] as const) {
    equal(providerNeedsAttention(provider("oauth", { auth_mode: "oauth", oauth_status })), true);
  }
  equal(providerNeedsAttention(provider("pending", { auth_mode: "oauth", oauth_status: "pending" })), false);
  equal(providerNeedsAttention(provider("connected", { auth_mode: "oauth", oauth_status: "connected" })), false);
});
