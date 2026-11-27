// Network call behind the Sub2API "Detect platform" action. Kept apart from the
// pure rules in ./sub2api so those stay importable by plain Node unit tests.

import { backend } from "./backend";
import { detectionKey, type Sub2ApiDetectionState } from "./sub2api";
import type { Sub2ApiPlatformDetection } from "./types";

/** Ask the gateway to read the key's listing routes and suggest a platform. */
export async function detectSub2ApiPlatform(
  address: string,
  apiKey: string,
  useProxy: boolean,
): Promise<Sub2ApiDetectionState> {
  const result = await backend<Sub2ApiPlatformDetection>("detect_sub2api_platform", {
    input: { base_url: address.trim(), api_key: apiKey.trim(), use_proxy: useProxy },
  });
  return { key: detectionKey(address, apiKey), result };
}
