// Sub2API preset (vendor id `sub2api`): relay to a Sub2API gateway, one channel per
// Sub2API group platform. Mirrors identity helpers in
// crates/nyro-core/src/provider/sub2api; routing rules arrive through the preset's
// relay metadata (see ./relay-address).
//
// Kept free of React so the rules are unit tested directly.

import type { ProviderChannelPreset, ProviderKey, ProviderProtocolEndpoint, Sub2ApiPlatformDetection } from "./types";

export const SUB2API_PRESET_ID = "sub2api";

export function isSub2ApiPreset(presetId?: string | null): boolean {
  return presetId?.trim().toLowerCase() === SUB2API_PRESET_ID;
}

/** Sub2API preset by vendor or preset key. Mirrors `provider::sub2api::is_sub2api`. */
export function isSub2ApiProvider(provider: { vendor?: string | null; preset_key?: string | null }): boolean {
  return isSub2ApiPreset(provider.vendor) || isSub2ApiPreset(provider.preset_key);
}

/** The legacy `sub2api` channel of the OpenAI preset, superseded by `sub2api/openai`. */
export function isLegacySub2ApiChannel(channel?: string | null): boolean {
  return channel?.trim().toLowerCase() === "sub2api";
}

type RelayIdentity = {
  vendor?: string | null;
  preset_key?: string | null;
  channel?: string | null;
};

/** The Sub2API preset or the legacy channel (never a chained Nyro). Mirrors `errors::relays_sub2api`. */
export function relaysSub2Api(provider: RelayIdentity): boolean {
  const nyro = [provider.vendor, provider.preset_key]
    .some((value) => value?.trim().toLowerCase() === "nyro-gateway");
  return isSub2ApiProvider(provider) || (!nyro && isLegacySub2ApiChannel(provider.channel));
}

type KeyIdentity = RelayIdentity & {
  api_key?: string | null;
  keys?: Pick<ProviderKey, "id">[] | null;
  protocol_endpoints?: Pick<ProviderProtocolEndpoint, "api_key" | "is_enabled">[] | null;
};

/**
 * One key speaks for the whole relay, so its quota is read from Sub2API's
 * `/v1/usage`: a usable provider key and no key pool. Mirrors nyro-core
 * `admin::usage::sub2api_upstream_usage`.
 */
export function sub2apiUpstreamUsage(provider: KeyIdentity): boolean {
  if (!relaysSub2Api(provider) || (provider.keys?.length ?? 0) > 0) return false;
  if (provider.api_key?.trim()) return true;
  return (provider.protocol_endpoints ?? []).some((endpoint) => endpoint.is_enabled && Boolean(endpoint.api_key?.trim()));
}

/** Address + key a detection result belongs to; a changed form invalidates it. */
export function detectionKey(address?: string | null, apiKey?: string | null): string {
  return `${(address ?? "").trim()}\u0000${(apiKey ?? "").trim()}`;
}

/**
 * The selected platform disagrees with a confident detection. Low-confidence or
 * platform-less results never warn: they only list candidates.
 */
export function platformMismatch(detection: Sub2ApiPlatformDetection | null | undefined, channel?: string | null): boolean {
  if (!detection?.platform || detection.confidence === "low") return false;
  return detection.platform !== (channel ?? "").trim().toLowerCase();
}

const EVIDENCE_ZH: Array<[RegExp, (match: RegExpMatchArray) => string]> = [
  [/^The model list uses Sub2API's (\w+) format\.$/, (m) => `模型列表是 Sub2API 的 ${m[1]} 格式。`],
  [/^The key is accepted on \/v1beta \(Gemini routes\)\.$/, () => "该 Key 可访问 /v1beta（Gemini 路由）。"],
  [/^The key is rejected on \/v1beta \(not a Gemini group\)\.$/, () => "该 Key 无法访问 /v1beta（不是 Gemini 分组）。"],
  [/^Models from several families are listed next to the Gemini route\.$/, () => "可访问 Gemini 路由，同时列出了多个模型家族。"],
  [/^The group lists no models\.$/, () => "分组没有列出任何模型。"],
  [/^Claude and Gemini models are listed together, as Antigravity groups do\.$/, () => "同时列出了 Claude 和 Gemini 模型，符合 Antigravity 分组的特征。"],
  [/^Only Gemini models are listed, but \/v1beta is not accepted\.$/, () => "只列出了 Gemini 模型，但无法访问 /v1beta。"],
  [/^Only Gemini models are listed; the Gemini route could not be verified/, () => "模型列表只包含 Gemini，但 Gemini 路由暂时无法验证。"],
  [/^Only Claude models are listed\./, () => "只列出了 Claude 模型。未配置模型映射的分组也会列出 Claude 默认模型，请在 Sub2API 中确认平台。"],
  [/^Only (\w+) models are listed\.$/, (m) => `只列出了 ${m[1]} 模型。`],
  [/^Models from several vendors are listed, as aggregator and composite groups do\.$/, () => "列出了多个厂商的模型，符合聚合平台或组合分组的特征。"],
  [/^The group name suggests the (\w+) platform\.$/, (m) => `分组名称指向 ${m[1]} 平台。`],
  [/^The group name suggests (\w+), which the model evidence does not support\.$/, (m) => `分组名称指向 ${m[1]}，但模型列表不支持该判断。`],
];

/** Localize trusted Nyro operator hints while retaining the original HTTP error. */
export function sub2apiHintText(text: string, isZh: boolean): string {
  if (!isZh) return text;
  const hints = [
    ["Sub2API rejected the key for billing", "Sub2API 拒绝了此 Key 的计费请求。请检查钱包余额、订阅状态和 Key 额度。"],
    ["This Sub2API group only accepts Claude Code clients.", "此分组仅允许 Claude Code 客户端。请为其他客户端选择另一个分组，或在 Sub2API 中调整分组限制。"],
    ["This Sub2API OpenAI group does not allow /v1/messages.", "此 OpenAI 分组未开启 /v1/messages。可取消勾选 Anthropic Messages，由 Nyro 转成 Responses，也可在 Sub2API 中开启该分组的 Messages 调度。"],
    ["The Sub2API key's group platform does not serve this route.", "此 Key 所属分组不支持这条路由。请使用「识别平台」核对 Provider 的平台选择。"],
    ["This model is not enabled for the Sub2API key's group.", "此模型不在该 Key 的分组中。请选择分组列表中的模型，或在 Sub2API 中修改模型白名单。"],
  ];
  for (const [prefix, message] of hints) {
    if (text.startsWith(prefix)) return message;
    const suffix = text.lastIndexOf(" — " + prefix);
    if (suffix >= 0) return text.slice(0, suffix) + " — " + message;
  }
  return text;
}

/** Localize one backend evidence line (the backend speaks English). */
export function evidenceText(line: string, isZh: boolean): string {
  if (!isZh) return line;
  for (const [pattern, render] of EVIDENCE_ZH) {
    const match = line.match(pattern);
    if (match) return render(match);
  }
  return line;
}

/** A detection result remembered for the address + key it was read with. */
export interface Sub2ApiDetectionState {
  key: string;
  result: Sub2ApiPlatformDetection;
}

/** Localized label of a platform (channel) id. */
export function platformLabel(channels: ProviderChannelPreset[], id: string, isZh: boolean): string {
  const channel = channels.find((item) => item.id === id);
  return channel ? (isZh ? channel.label.zh : channel.label.en) : id;
}
