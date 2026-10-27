import { deepEqual, equal } from "node:assert/strict";
import { test } from "node:test";
import {
  applyNyroGatewayAddress, isNyroGatewayPreset, nyroGatewayModelsSource, nyroGatewayRoot,
  withNyroGatewayAddress,
} from "./nyro-gateway";

test("only the nyro-gateway preset is the chain preset, never the retired `nyro` id", () => {
  equal(isNyroGatewayPreset("nyro-gateway"), true);
  equal(isNyroGatewayPreset(" Nyro-Gateway "), true);
  equal(isNyroGatewayPreset("nyro"), false);
  equal(isNyroGatewayPreset("custom"), false);
  equal(isNyroGatewayPreset(undefined), false);
});

test("the gateway root drops trailing slashes and one remote version segment", () => {
  for (const [input, expected] of [
    ["http://10.0.0.2:19530", "http://10.0.0.2:19530"],
    ["http://10.0.0.2:19530/", "http://10.0.0.2:19530"],
    ["http://10.0.0.2:19530/v1", "http://10.0.0.2:19530"],
    ["http://10.0.0.2:19530/v1/", "http://10.0.0.2:19530"],
    ["http://10.0.0.2:19530/V1BETA", "http://10.0.0.2:19530"],
    ["  https://nyro.example.com/v1  ", "https://nyro.example.com"],
    ["https://edge.example.com/nyro/v1", "https://edge.example.com/nyro"],
    ["https://edge.example.com/v2", "https://edge.example.com/v2"],
    ["http://v1", "http://v1"],
    ["", ""],
  ]) {
    equal(nyroGatewayRoot(input), expected, input);
  }
  equal(nyroGatewayModelsSource("http://nyro:19530/v1/"), "http://nyro:19530/v1/models");
  equal(nyroGatewayModelsSource("   "), "");
});

test("a new address fans out to every endpoint and keeps the raw text on the form", () => {
  const form = {
    base_url: "",
    models_source: "",
    protocol_endpoints: [
      { protocol: "openai-compatible", base_url: "", api_key: "k" },
      { protocol: "google-gemini", base_url: "", api_key: "k" },
    ],
  };
  const next = applyNyroGatewayAddress(form, "http://nyro:19530/v1");
  equal(next.base_url, "http://nyro:19530/v1");
  deepEqual(next.protocol_endpoints.map((endpoint) => endpoint.base_url), ["http://nyro:19530", "http://nyro:19530"]);
  equal(next.protocol_endpoints[0].api_key, "k");
  equal(next.models_source, "http://nyro:19530/v1/models");
});

test("model discovery follows the address until the user customizes it", () => {
  const derived = applyNyroGatewayAddress({ base_url: "http://a:19530", models_source: "http://a:19530/v1/models" }, "http://b:19530");
  equal(derived.models_source, "http://b:19530/v1/models");
  const custom = applyNyroGatewayAddress({ base_url: "http://a:19530", models_source: "ai://models.dev/openai" }, "http://b:19530");
  equal(custom.models_source, "ai://models.dev/openai");
});

test("picker entries inherit the normalized address", () => {
  deepEqual(
    withNyroGatewayAddress([{ protocol: "anthropic-messages", baseUrl: "" }], "http://nyro:19530/"),
    [{ protocol: "anthropic-messages", baseUrl: "http://nyro:19530" }],
  );
});
