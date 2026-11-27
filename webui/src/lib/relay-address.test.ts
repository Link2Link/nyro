import { deepEqual, equal } from "node:assert/strict";
import { test } from "node:test";
import { applyRelayAddress, relayModelsSource, relayRoot, relayRoute, withRelayAddress } from "./relay-address";
import type { ProviderRelayAddressing } from "./types";

// Mirrors the Sub2API preset metadata (crates/nyro-core/src/provider/sub2api).
const SUFFIXES = ["/antigravity/v1beta", "/antigravity/v1", "/antigravity", "/backend-api/codex", "/v1beta", "/v1"];
const antigravity: ProviderRelayAddressing = {
  modelsPath: "/v1/models",
  routePrefixes: { "anthropic-messages": "/antigravity", "google-gemini": "/antigravity" },
  rootSuffixes: SUFFIXES,
};
const gemini: ProviderRelayAddressing = { modelsPath: "/v1beta/models", rootSuffixes: SUFFIXES };

test("a pasted Sub2API address normalizes to the gateway root", () => {
  for (const [input, expected] of [
    ["https://s.example.com", "https://s.example.com"],
    ["  https://s.example.com/v1/  ", "https://s.example.com"],
    ["https://s.example.com/V1BETA", "https://s.example.com"],
    ["https://s.example.com/antigravity", "https://s.example.com"],
    ["https://s.example.com/antigravity/v1", "https://s.example.com"],
    ["https://s.example.com/antigravity/v1beta/", "https://s.example.com"],
    ["https://s.example.com/backend-api/codex", "https://s.example.com"],
    ["https://edge.example.com/sub2api/v1", "https://edge.example.com/sub2api"],
    ["https://edge.example.com/v2", "https://edge.example.com/v2"],
    ["http://v1", "http://v1"],
    ["http://antigravity", "http://antigravity"],
    ["", ""],
  ]) {
    equal(relayRoot(input, SUFFIXES), expected, input);
  }
});

test("routes and model discovery follow the platform's relay metadata", () => {
  equal(relayRoute("https://s.example.com/v1", antigravity, "anthropic-messages"), "https://s.example.com/antigravity/v1/messages");
  equal(relayRoute("https://s.example.com", antigravity, "google-gemini"), "https://s.example.com/antigravity/v1beta/models/{model}");
  equal(relayRoute("https://s.example.com", antigravity, "openai-compatible"), "https://s.example.com/v1/chat/completions");
  equal(relayRoute("  ", antigravity, "openai-compatible"), "");
  equal(relayModelsSource("https://s.example.com/antigravity", antigravity), "https://s.example.com/v1/models");
  equal(relayModelsSource("https://s.example.com/v1", gemini), "https://s.example.com/v1beta/models");
  equal(relayModelsSource("", gemini), "");
});

test("a typed address fans out to every endpoint and keeps a customized discovery URL", () => {
  const form = {
    base_url: "",
    models_source: "",
    protocol_endpoints: [
      { protocol: "anthropic-messages/messages/2023-06-01", base_url: "", api_key: "k" },
      { protocol: "google-gemini/generate-content/v1beta", base_url: "", api_key: "k" },
    ],
  };
  const next = applyRelayAddress(form, "https://s.example.com/antigravity/v1", antigravity);
  equal(next.base_url, "https://s.example.com/antigravity/v1");
  deepEqual(next.protocol_endpoints.map((endpoint) => endpoint.base_url), ["https://s.example.com", "https://s.example.com"]);
  equal(next.models_source, "https://s.example.com/v1/models");
  const custom = applyRelayAddress({ ...next, models_source: "https://other.example/v1/models" }, "https://b.example.com", antigravity);
  equal(custom.models_source, "https://other.example/v1/models");
  deepEqual(
    withRelayAddress([{ protocol: "google-gemini", baseUrl: "" }], "https://s.example.com/v1beta/", gemini),
    [{ protocol: "google-gemini", baseUrl: "https://s.example.com" }],
  );
});
