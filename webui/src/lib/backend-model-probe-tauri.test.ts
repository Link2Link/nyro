import { deepEqual, equal } from "node:assert/strict";
import { test } from "node:test";
import type { ModelProbeOutcome } from "./types";

test("Tauri model probe passes camelCase accountId to the real invoke bridge", async () => {
  const previous = Object.getOwnPropertyDescriptor(globalThis, "window");
  const requests: { cmd: string; args: unknown }[] = [];
  const outcome: ModelProbeOutcome = {
    meta: { protocol: "test", base_url: "https://example.test", account_id: "selected", account_label: "Work" }, results: [],
  };
  Object.defineProperty(globalThis, "window", { configurable: true, value: {
    __TAURI_INTERNALS__: {
      invoke: async (cmd: string, args: unknown) => { requests.push({ cmd, args }); return outcome; },
    },
  } });
  try {
    // Import after installing the desktop marker; each test file runs in its own Node process.
    const { backend, IS_TAURI } = await import("./backend");
    equal(IS_TAURI, true);
    const args = { id: "provider", models: ["model"], prompt: "hello", accountId: "selected" };
    deepEqual(await backend<ModelProbeOutcome>("probe_provider_models", args), outcome);
    await backend("probe_provider_models", { id: "legacy" });
    deepEqual(requests, [
      { cmd: "probe_provider_models", args },
      { cmd: "probe_provider_models", args: { id: "legacy" } },
    ]);
  } finally {
    if (previous) Object.defineProperty(globalThis, "window", previous);
    else Reflect.deleteProperty(globalThis, "window");
  }
});
