import { equal } from "node:assert/strict";
import { test } from "node:test";
import { DEVIN_PROTOCOL, KIRO_PROTOCOL, isDevinProvider, isKiroProvider, nativePresetProtocol, nativeProviderProtocol, hasProviderModelCatalog } from "./native-provider";
import { PROTOCOL_ENDPOINT_TABLE, PROTOCOL_TABLE, resolveProtocol } from "./protocol";

test("Devin canonical and persisted suite identity select only its native protocol", () => {
  for (const defaultProtocol of [DEVIN_PROTOCOL, "devin-chat"]) {
    equal(nativePresetProtocol({ id: "devin", defaultProtocol }), DEVIN_PROTOCOL);
  }
  equal(isDevinProvider({ vendor: "Devin" }), true);
  equal(isDevinProvider({ preset_key: "devin" }), true);
  equal(isDevinProvider({ vendor: "custom" }), false);
});

test("Generic presets cannot select the native Devin transport", () => {
  equal(nativePresetProtocol({ id: "custom", defaultProtocol: DEVIN_PROTOCOL }), null);
  equal(nativePresetProtocol({ id: "openai", defaultProtocol: "openai-compatible" }), null);
  equal(nativePresetProtocol(null), null);
  equal(PROTOCOL_TABLE.some((p) => String(p.id) === DEVIN_PROTOCOL), false);
  equal(PROTOCOL_ENDPOINT_TABLE.some((p) => p.id === DEVIN_PROTOCOL), false);
  equal(resolveProtocol(DEVIN_PROTOCOL), null);
});

test("Kiro canonical and persisted suite identity select only its native protocol", () => {
  for (const defaultProtocol of [KIRO_PROTOCOL, "kiro-chat"]) {
    equal(nativePresetProtocol({ id: "kiro", defaultProtocol }), KIRO_PROTOCOL);
  }
  equal(isKiroProvider({ vendor: "Kiro" }), true);
  equal(isKiroProvider({ preset_key: "kiro" }), true);
  equal(isKiroProvider({ vendor: "custom" }), false);
  equal(isKiroProvider({ vendor: "devin" }), false);
});

test("Native identity survives missing presets and supplies model discovery", () => {
  for (const identity of [{ vendor: " KIRO " }, { preset_key: "Kiro" }]) {
    equal(nativeProviderProtocol(identity), KIRO_PROTOCOL);
    equal(hasProviderModelCatalog({ ...identity, models_source: "" }), true);
  }
  equal(nativeProviderProtocol({ vendor: "Devin" }), DEVIN_PROTOCOL);
  equal(nativeProviderProtocol({ vendor: "custom" }), null);
  equal(hasProviderModelCatalog({ vendor: "custom", models_source: " " }), false);
  equal(hasProviderModelCatalog({ vendor: "custom", models_source: "https://example.com/models" }), true);
  equal(hasProviderModelCatalog(null), false);
});

test("Generic presets cannot select the native Kiro transport", () => {
  equal(nativePresetProtocol({ id: "custom", defaultProtocol: KIRO_PROTOCOL }), null);
  equal(nativePresetProtocol({ id: "openai", defaultProtocol: KIRO_PROTOCOL }), null);
  equal(PROTOCOL_TABLE.some((p) => String(p.id) === KIRO_PROTOCOL), false);
  equal(PROTOCOL_ENDPOINT_TABLE.some((p) => p.id === KIRO_PROTOCOL), false);
  equal(resolveProtocol(KIRO_PROTOCOL), null);
});
