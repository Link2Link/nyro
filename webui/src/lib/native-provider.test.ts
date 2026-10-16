import { equal } from "node:assert/strict";
import { test } from "node:test";
import { DEVIN_PROTOCOL, isDevinProvider, nativePresetProtocol } from "./native-provider";
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
