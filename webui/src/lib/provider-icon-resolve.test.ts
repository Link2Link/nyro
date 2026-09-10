import { deepEqual, equal, ok } from "node:assert/strict";
import { test } from "node:test";
import {
  ICON_ALIASES, normalizeProviderSvg, resolveProviderIconKey,
} from "./provider-icon-resolve";

/** The shipped catalog keys these cases exercise, plus the predicate shape the app passes. */
const shipped = new Set(["nyro", "bailian", "doubao", "googlecloud", "kimi", "zhipu", "gemini",
  "openai", "anthropic", "grok", "opencode-logo-light", "zai", "qwen", "deepseek", "minimax"]);
const available = (iconKey: string) => shipped.has(iconKey);
const resolve = (source: Parameters<typeof resolveProviderIconKey>[0]) =>
  resolveProviderIconKey(source, available);

test("canonical provider identity outranks name and host, and maps through vendor aliases", () => {
  // preset_key ?? vendor is what every admin surface already reports as provider_icon.
  equal(resolve({ iconKey: "bailian", name: "阿里百炼" }), "bailian");
  // The custom preset ships no usable vector mark, so it must not override the identity
  // the provider's own name and host still provide.
  equal(resolve({ iconKey: "custom", name: "apinebula", baseUrl: "https://apinebula.ai/v1" }), null);
  equal(resolve({ iconKey: "custom", name: "UUAPI gemini" }), "gemini");
  equal(resolve({ iconKey: "ark-coding", name: "火山引擎" }), "doubao");
  equal(resolve({ iconKey: "opencode-go", name: "opencode go" }), "opencode-logo-light");
  equal(resolve({ iconKey: "zhipuai", name: "GLM" }), "zhipu");
  equal(resolve({ iconKey: "kimi-code", name: "Kimi" }), "kimi");
  equal(resolve({ iconKey: "vertexai" }), "googlecloud");
  // An unknown identity key falls through to the weaker signals instead of showing nothing.
  equal(resolve({ iconKey: "not-a-shipped-key", name: "SupurGrok", baseUrl: "https://cli-chat-proxy.grok.com/v1" }), "grok");
});

test("the wire protocol never brands a provider", () => {
  // "openai-compatible" and "openai-responses" are request formats, not vendors: no
  // provider is identified by them, so a relay keeps its own identity or no icon.
  for (const name of ["apinebula", "Some Relay", "中转 A"]) {
    equal(resolve({ name, baseUrl: "https://relay.example/v1" }), null);
  }
  equal(resolve({ iconKey: "custom", name: "Some Relay", baseUrl: "https://relay.example/v1" }), null);
});

test("name and host still resolve a brand when no canonical identity is stored", () => {
  equal(resolve({ name: "Beta Gemini" }), "gemini");
  equal(resolve({ name: "Alpha DeepSeek" }), "deepseek");
  equal(resolve({ name: "Disabled Kimi" }), "kimi");
  equal(resolve({ name: "GLM" }), "zhipu");
  equal(resolve({ name: "SupurGrok", baseUrl: "https://cli-chat-proxy.grok.com/v1" }), "grok");
  equal(resolve({ name: "relay", baseUrl: "https://api.deepseek.com/anthropic" }), "deepseek");
  equal(resolve({ name: "对象", baseUrl: "not a url at all" }), null);
});

test("an inherited object member is never mistaken for a shipped icon", () => {
  // The alias table and the loader map are plain objects; only own keys may resolve.
  const shippedKeys = new Set(["nyro"]);
  const only = (iconKey: string) => shippedKeys.has(iconKey);
  equal(resolveProviderIconKey({ name: "constructor" }, only), null);
  equal(resolveProviderIconKey({ iconKey: "constructor" }, only), null);
  equal(resolveProviderIconKey({ iconKey: "hasOwnProperty" }, only), null);
  equal(resolveProviderIconKey({ name: "Nyro" }, only), "nyro");
});

test("aliases always point at a real icon name, never at another alias", () => {
  for (const [key, alias] of Object.entries(ICON_ALIASES)) {
    ok(alias.length > 0 && alias === alias.toLowerCase(), `${key} -> ${alias}`);
    equal(alias in ICON_ALIASES && !shipped.has(alias), false, `${key} -> ${alias} must be a shipped icon, not another alias`);
  }
});

const ROOT = "<svg width='240' height='300' viewBox='0 0 240 300' fill='none'><path d='M0 0h240v300H0z' fill='#211E1E'/></svg>";

test("icon markup fills the box its caller reserves, whatever size the file declares", () => {
  const color = normalizeProviderSvg(ROOT, false);
  ok(color.startsWith('<svg viewBox="0 0 240 300" fill="none" width="100%" height="100%"'), color.slice(0, 120));
  ok(!/\swidth="240"/.test(color) && !/\sheight="300"/.test(color));
  ok(color.includes('class="provider-icon-svg"') && color.includes('viewBox="0 0 240 300"'));
  // 1em- and px-sized files normalize the same way, and the viewBox survives.
  const em = normalizeProviderSvg('<svg height="1em" style="flex:none;line-height:1" viewBox="0 0 24 24" width="1em"><title>Gemini</title><path d="M1 1"/></svg>', false);
  equal(em, '<svg viewBox="0 0 24 24" width="100%" height="100%" class="provider-icon-svg" aria-hidden="true" focusable="false"><path d="M1 1"/></svg>');
  const px = normalizeProviderSvg('<svg xmlns="http://www.w3.org/2000/svg" xml:space="preserve" width="1209px" height="1255px" viewBox="0 0 1209 1255"><path d="M1 1"/></svg>', false);
  ok(px.includes('width="100%" height="100%"') && !px.includes("1209px"));
});

test("monochrome normalization reaches single-quoted files too", () => {
  const mono = normalizeProviderSvg(ROOT, true);
  ok(mono.includes('fill="currentColor" stroke="currentColor" color="currentColor"'));
  ok(!mono.includes("#211E1E"), mono);
  // The shipped zai glyph keeps its dedicated outline treatment.
  const zai = normalizeProviderSvg("<svg width='1em' viewBox='0 0 30 30'><defs><linearGradient id='g'/></defs><path id='zai-bg' fill='#111' stroke='#222'/></svg>", true, "zai");
  ok(zai.includes('id="zai-bg" fill="none" stroke="currentColor"'), zai);
  ok(!zai.includes("<defs>"));
});

test("color markup keeps its own palette so brands stay recognizable", () => {
  const color = normalizeProviderSvg(ROOT, false);
  ok(color.includes('fill="#211E1E"'));
  deepEqual(resolve({ iconKey: "deepseek" }), "deepseek");
});
